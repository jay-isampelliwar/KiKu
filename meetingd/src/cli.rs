//! The interactive `preeti` menu: pick with arrow keys, Enter to open, Esc or q to go back.

use std::io::Write;
use std::process::{Command, Stdio};

use anyhow::{Result, bail};
use console::{Term, style};
use dialoguer::Select;
use dialoguer::theme::ColorfulTheme;
use rusqlite::Connection;

use crate::config::Config;
use crate::{db, recover, view};

pub fn run(config: &Config) -> Result<()> {
    let term = Term::stdout();
    if !term.is_term() {
        bail!("the menu needs a terminal. Try: preeti list, preeti last, preeti todos");
    }

    let conn = db::open(&config.db_path)?;
    let theme = ColorfulTheme::default();
    let mut screen = String::new();
    let mut picked = 0;

    loop {
        term.clear_screen()?;
        println!("{}\n", header(config, &conn)?);
        if !screen.is_empty() {
            println!("{screen}");
        }

        let paused = config.pause_file().exists();
        let items = [
            "Last meeting: summary",
            "Last meeting: to-dos",
            "My open to-dos (mark done)",
            "All to-dos",
            "Review to-dos (keep / drop)",
            "Browse meetings",
            if paused { "Resume recording" } else { "Pause recording" },
            "Quit",
        ];

        let Some(choice) = Select::with_theme(&theme).items(items).default(picked).interact_opt()? else {
            break;
        };
        picked = choice;

        screen = match choice {
            0 => with_last(&conn, view::notes)?,
            1 => with_last(&conn, view::todos)?,
            2 => {
                review_todos(config, &conn, &term, &theme, Scope::OpenTasks)?;
                view::open_tasks(&conn)?
            }
            3 => view::all_todos(&conn)?,
            4 => {
                review_todos(config, &conn, &term, &theme, Scope::All)?;
                view::all_todos(&conn)?
            }
            5 => {
                browse(config, &conn, &term, &theme)?;
                String::new()
            }
            6 => {
                set_paused(config, !paused)?;
                if paused { "Recording resumed.\n".into() } else { "Recording paused. Meetings are not recorded.\n".into() }
            }
            _ => break,
        };
    }

    term.clear_screen()?;
    Ok(())
}

/// App name, whether recording is on, and a few counts.
fn header(config: &Config, conn: &Connection) -> Result<String> {
    let state = if !recover::daemon_running(config) {
        style("○ Background service is not running").red().to_string()
    } else if config.pause_file().exists() {
        style("❚❚ Paused").yellow().to_string()
    } else {
        style("● Recording meetings automatically").green().to_string()
    };

    let meetings = db::list_meetings(conn)?.len();
    let todos = db::all_todos(conn)?;
    let count = |status: &str| todos.iter().filter(|t| t.todo.status == status).count();
    let (busy, _) = db::work_in_progress(conn)?;
    let busy = if busy > 0 { format!("  ·  {busy} being processed") } else { String::new() };
    let plural = |n: usize| if n == 1 { "" } else { "s" };
    let (meetings, unreviewed, open) = (meetings, count("suggested"), count("kept"));

    Ok(format!(
        "{}   {state}\n{}",
        style("Preeti · meeting notes").bold(),
        style(format!(
            "{meetings} meeting{}  ·  {open} open task{}  ·  {unreviewed} to review{busy}",
            plural(meetings),
            plural(open)
        ))
        .dim()
    ))
}

fn with_last(conn: &Connection, show: fn(&Connection, &str) -> Result<String>) -> Result<String> {
    match db::last_meeting_id(conn)? {
        Some(id) => show(conn, &id),
        None => Ok("No meetings yet.\n".into()),
    }
}

/// Pick a meeting from the list, then read its notes or transcript.
fn browse(config: &Config, conn: &Connection, term: &Term, theme: &ColorfulTheme) -> Result<()> {
    let mut picked = 0;

    loop {
        let meetings = db::list_meetings(conn)?;
        term.clear_screen()?;
        println!("{}\n", header(config, conn)?);

        if meetings.is_empty() {
            println!("No meetings yet.\n");
            term.read_key()?;
            return Ok(());
        }

        let mut items: Vec<String> = meetings.iter().map(view::meeting_line).collect();
        items.push("← Back".into());

        let Some(choice) = Select::with_theme(theme)
            .with_prompt("Pick a meeting (Esc to go back)")
            .items(&items)
            .default(picked)
            .max_length(15)
            .interact_opt()?
        else {
            return Ok(());
        };

        let Some(meeting) = meetings.get(choice) else { return Ok(()) };
        picked = choice;

        if !meeting_page(config, conn, term, theme, &meeting.id)? {
            return Ok(());
        }
    }
}

/// One meeting's notes, with a way to read the transcript. Returns false to go to the main menu.
fn meeting_page(config: &Config, conn: &Connection, term: &Term, theme: &ColorfulTheme, id: &str) -> Result<bool> {
    loop {
        term.clear_screen()?;
        println!("{}\n", header(config, conn)?);
        println!("{}", view::notes(conn, id)?);

        let items = ["Review to-dos (keep / drop)", "Read transcript", "← Back to meetings", "← Main menu"];
        match Select::with_theme(theme).items(items).default(0).interact_opt()? {
            Some(0) => review_todos(config, conn, term, theme, Scope::Meeting(id))?,
            Some(1) => page(&view::transcript(conn, id)?)?,
            Some(3) => return Ok(false),
            _ => return Ok(true),
        }
    }
}

/// Which to-dos a review screen shows.
#[derive(Clone, Copy)]
enum Scope<'a> {
    /// Everything, not-reviewed first.
    All,
    /// One meeting's to-dos.
    Meeting(&'a str),
    /// Only kept tasks that are not done yet.
    OpenTasks,
}

/// Order for review lists: what needs a decision first, finished things last.
fn status_order(status: &str) -> u8 {
    match status {
        "suggested" => 0,
        "kept" => 1,
        "done" => 2,
        _ => 3,
    }
}

/// Pick to-dos one by one and keep, finish, drop or undo them.
fn review_todos(config: &Config, conn: &Connection, term: &Term, theme: &ColorfulTheme, scope: Scope) -> Result<()> {
    let mut picked = 0;

    loop {
        let mut rows: Vec<db::TodoWithMeeting> = db::all_todos(conn)?
            .into_iter()
            .filter(|r| match scope {
                Scope::All => true,
                Scope::Meeting(id) => r.meeting_id == id,
                Scope::OpenTasks => r.todo.status == "kept",
            })
            .collect();

        match scope {
            Scope::OpenTasks => rows.sort_by_key(|r| r.todo.due.is_none()),
            _ => rows.sort_by_key(|r| status_order(&r.todo.status)),
        }

        term.clear_screen()?;
        println!("{}\n", header(config, conn)?);

        if rows.is_empty() {
            let empty = match scope {
                Scope::OpenTasks => "No open tasks. Keep a to-do to add it here. Press any key.",
                _ => "No to-dos here. Press any key.",
            };
            println!("{empty}");
            term.read_key()?;
            return Ok(());
        }

        let title = match scope {
            Scope::OpenTasks => format!("My open to-dos ({})", rows.len()),
            _ => format!(
                "Review to-dos  {}",
                style(format!("{} not reviewed", rows.iter().filter(|r| r.todo.status == "suggested").count())).dim()
            ),
        };
        println!("{}\n{}\n", style(title).bold(), style(view::TODO_LEGEND).dim());

        let mut items: Vec<String> = rows
            .iter()
            .map(|r| {
                let from = r.meeting_title.as_deref().unwrap_or("(no title)");
                format!("{}  {}", view::todo_line(&r.todo).trim_start(), style(format!("— {from}")).dim())
            })
            .collect();
        items.push("← Back".into());

        let Some(choice) = Select::with_theme(theme)
            .with_prompt("Pick a to-do (Esc to go back)")
            .items(&items)
            .default(picked.min(items.len() - 1))
            .max_length(15)
            .interact_opt()?
        else {
            return Ok(());
        };

        let Some(row) = rows.get(choice) else { return Ok(()) };
        picked = choice;

        decide(conn, term, theme, row)?;
    }
}

/// Show one to-do with the line it came from, then keep, finish, drop or undo it.
fn decide(conn: &Connection, term: &Term, theme: &ColorfulTheme, row: &db::TodoWithMeeting) -> Result<()> {
    let t = &row.todo;

    term.clear_screen()?;
    println!("{}", style(&t.task).bold());
    if let Some(due) = &t.due {
        println!("{}", style(format!("Due: {due}")).yellow());
    }
    println!("From: {}", row.meeting_title.as_deref().unwrap_or("(no title)"));
    if let Some(quote) = &t.quote {
        println!("Said: {}", style(format!("\"{quote}\"")).italic());
    }
    println!("Now:  {}\n", view::todo_status(&t.status));

    let keep = if t.status == "done" { "✔ Reopen (not done yet)" } else { "✔ Keep (open task)" };
    let actions = [(keep, "kept"), ("☑ Mark done", "done"), ("✘ Drop", "dropped"), ("↺ Undo (not reviewed)", "suggested")];
    let mut items: Vec<&str> = actions.iter().map(|(label, _)| *label).collect();
    items.push("← Back");

    let start = if t.status == "kept" { 1 } else { 0 };
    if let Some(choice) = Select::with_theme(theme).items(&items).default(start).interact_opt()?
        && let Some((_, status)) = actions.get(choice)
    {
        db::set_todo_status(conn, t.id, status)?;
    }

    Ok(())
}

/// Show long text in `less`, so it can be scrolled. Falls back to printing it.
fn page(text: &str) -> Result<()> {
    let child = Command::new("less").args(["-R", "-F", "-X"]).stdin(Stdio::piped()).spawn();

    match child {
        Ok(mut less) => {
            if let Some(mut stdin) = less.stdin.take() {
                stdin.write_all(text.as_bytes()).ok();
            }
            less.wait()?;
        }
        Err(_) => {
            println!("{text}");
            Term::stdout().read_key()?;
        }
    }

    Ok(())
}

/// Turn meeting detection off or on, using the flag file the daemon checks every second.
pub fn set_paused(config: &Config, paused: bool) -> Result<()> {
    let file = config.pause_file();

    if paused {
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&file, "")?;
    } else if file.exists() {
        std::fs::remove_file(&file)?;
    }

    Ok(())
}
