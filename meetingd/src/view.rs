//! Text for the terminal: meeting lists, notes, to-dos and transcripts.
//! Shared by the one-shot commands and the interactive menu.

use std::fmt::Write;

use anyhow::Result;
use chrono::{DateTime, FixedOffset};
use console::style;
use rusqlite::Connection;

use crate::db::{self, Meeting, Todo};

fn parse(time: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(time).ok()
}

/// "Wed 18 Nov, 11:55"
fn when(m: &Meeting) -> String {
    parse(&m.started_at).map_or(m.started_at.clone(), |t| t.format("%a %d %b, %H:%M").to_string())
}

/// "12 min", "<1 min", or "..." while still recording.
fn duration(m: &Meeting) -> String {
    let minutes = parse(&m.started_at)
        .zip(m.ended_at.as_deref().and_then(parse))
        .map(|(a, b)| (b - a).num_minutes());

    match minutes {
        None => "...".into(),
        Some(0) => "<1 min".into(),
        Some(n) => format!("{n} min"),
    }
}

/// Plain words for where a meeting is in the pipeline. Empty when done.
fn stage(status: &str) -> &'static str {
    match status {
        "recording" => "recording now",
        "recorded" | "labelled" | "label_failed" => "processing",
        "summary_failed" => "summary failed",
        _ => "",
    }
}

fn title(m: &Meeting) -> String {
    m.title.clone().unwrap_or_else(|| "(no title yet)".into())
}

/// One line for a meeting in a list.
pub fn meeting_line(m: &Meeting) -> String {
    let stage = stage(&m.status);
    let stage = if stage.is_empty() { String::new() } else { format!("  {}", style(format!("[{stage}]")).yellow()) };

    format!("{:<18} {:>7}   {}{stage}", when(m), duration(m), title(m))
}

/// All meetings, newest first.
pub fn meetings(conn: &Connection) -> Result<String> {
    let meetings = db::list_meetings(conn)?;

    if meetings.is_empty() {
        return Ok("No meetings yet. They appear here once a call ends.\n".into());
    }

    let mut out = format!("{}\n\n", style(format!("Meetings ({})", meetings.len())).bold());
    for m in &meetings {
        writeln!(out, "  {}  {}", style(&m.id).dim(), meeting_line(m))?;
    }

    Ok(out)
}

/// Key for the to-do marks.
pub const TODO_LEGEND: &str = "• not reviewed   ✔ open task   ☑ done   ✘ dropped";

/// "✔ Send the budget sheet  (due: Monday)  #3". Done and dropped ones are dimmed.
pub fn todo_line(t: &Todo) -> String {
    let due = t.due.as_deref().map(|d| style(format!("  (due: {d})")).yellow().to_string()).unwrap_or_default();
    let id = style(format!("  #{}", t.id)).dim();

    match t.status.as_str() {
        "kept" => format!("  {} {}{due}{id}", style("✔").green(), t.task),
        "dropped" => format!("  {} {}{id}", style("✘").dim(), style(&t.task).dim().strikethrough()),
        "done" => format!("  {} {}{id}", style("☑").green().dim(), style(&t.task).dim()),
        _ => format!("  {} {}{due}{id}", style("•").cyan(), t.task),
    }
}

/// Plain word for a to-do status.
pub fn todo_status(status: &str) -> &'static str {
    match status {
        "kept" => "open task",
        "done" => "done",
        "dropped" => "dropped",
        _ => "not reviewed",
    }
}

/// Title, time, summary, topics, key points and to-dos of one meeting.
pub fn notes(conn: &Connection, id: &str) -> Result<String> {
    let Some(m) = db::meeting(conn, id)? else {
        return Ok(format!("No meeting called {id}.\n"));
    };

    let mut out = String::new();
    writeln!(out, "{}", style(title(&m)).bold().underlined())?;
    writeln!(out, "{}  ·  {}  ·  {}", when(&m), duration(&m), style(&m.id).dim())?;

    let stage = stage(&m.status);
    if !stage.is_empty() {
        writeln!(out, "{}", style(format!("Status: {stage}")).yellow())?;
    }
    writeln!(out)?;

    let (_, notes, error) = db::meeting_notes(conn, id)?;
    if let Some(error) = error.filter(|_| m.status == "summary_failed") {
        writeln!(out, "{} {error}\n", style("Summary failed:").red())?;
    }

    let notes: serde_json::Value = notes.and_then(|n| serde_json::from_str(&n).ok()).unwrap_or_default();

    if let Some(summary) = notes["summary"].as_str().filter(|s| !s.is_empty()) {
        writeln!(out, "{summary}\n")?;
    }

    for (heading, key) in [("Topics", "topics"), ("Key points", "key_points")] {
        let items: Vec<&str> = notes[key].as_array().into_iter().flatten().filter_map(|v| v.as_str()).collect();
        if !items.is_empty() {
            writeln!(out, "{}", style(heading).bold())?;
            for item in items {
                writeln!(out, "  - {item}")?;
            }
            writeln!(out)?;
        }
    }

    out.push_str(&todos(conn, id)?);

    Ok(out)
}

/// The to-dos of one meeting.
pub fn todos(conn: &Connection, id: &str) -> Result<String> {
    let todos = db::meeting_todos(conn, id)?;
    let mut out = format!("{}\n", style("My to-dos").bold());

    if todos.is_empty() {
        out.push_str("  (none)\n");
    }
    for t in &todos {
        writeln!(out, "{}", todo_line(t))?;
    }

    Ok(out)
}

/// Every to-do, grouped under its meeting, newest meeting first.
pub fn all_todos(conn: &Connection) -> Result<String> {
    let rows = db::all_todos(conn)?;

    if rows.is_empty() {
        return Ok("No to-dos yet.\n".into());
    }

    let open = rows.iter().filter(|r| r.todo.status == "suggested").count();
    let mut out = format!(
        "{}  {}\n{}\n",
        style(format!("All to-dos ({})", rows.len())).bold(),
        style(format!("{open} not reviewed")).dim(),
        style(TODO_LEGEND).dim()
    );
    let mut current = "";

    for row in &rows {
        if row.meeting_id != current {
            current = &row.meeting_id;
            let date = parse(&row.started_at).map_or(String::new(), |t| t.format("%a %d %b").to_string());
            let title = row.meeting_title.as_deref().unwrap_or("(no title)");
            writeln!(out, "\n{}  {}", style(date).dim(), style(title).bold())?;
        }
        writeln!(out, "{}", todo_line(&row.todo))?;
    }

    Ok(out)
}

/// Kept tasks that are not done yet. Ones with a due date first.
pub fn open_tasks(conn: &Connection) -> Result<String> {
    let mut rows: Vec<_> = db::all_todos(conn)?.into_iter().filter(|r| r.todo.status == "kept").collect();
    rows.sort_by_key(|r| r.todo.due.is_none());

    if rows.is_empty() {
        return Ok("No open tasks. Keep a to-do to add it here.\n".into());
    }

    let mut out = format!("{}\n\n", style(format!("My open to-dos ({})", rows.len())).bold());
    for r in &rows {
        let from = r.meeting_title.as_deref().unwrap_or("(no title)");
        writeln!(out, "{}  {}", todo_line(&r.todo), style(format!("— {from}")).dim())?;
    }

    Ok(out)
}

/// The full transcript of one meeting.
pub fn transcript(conn: &Connection, id: &str) -> Result<String> {
    let lines = db::meeting_segments(conn, id)?;

    if lines.is_empty() {
        return Ok("No transcript yet.\n".into());
    }

    let mut out = String::new();
    for s in lines {
        let secs = s.start_sec as u64;
        let speaker = if s.speaker == "Me" { style(format!("{:<9}", s.speaker)).green() } else { style(format!("{:<9}", s.speaker)).cyan() };
        writeln!(out, "{} {speaker} {}", style(format!("[{:02}:{:02}]", secs / 60, secs % 60)).dim(), s.text)?;
    }

    Ok(out)
}
