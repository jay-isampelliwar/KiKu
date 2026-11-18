mod audio;
mod cli;
mod config;
mod db;
mod detect;
mod labels;
mod meeting;
mod popup;
mod recover;
mod view;
mod worker;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing::{debug, error, info, warn};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use config::Config;
use detect::{Action, Detector, State, Tick};
use meeting::Capture;

/// Records meetings in the background and turns them into notes and to-dos.
/// Run with no command to open the menu.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to the config file.
    #[arg(long, default_value = "config/config.toml", global = true)]
    config: PathBuf,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon and keep running.
    Run,
    /// Show config, database and model paths.
    Status,
    /// List past meetings.
    List,
    /// Summary and to-dos of the most recent meeting.
    Last,
    /// Every to-do from every meeting.
    Todos,
    /// Keep a to-do (use the #number shown next to it).
    Keep {
        id: i64,
    },
    /// Drop a to-do (use the #number shown next to it).
    Drop {
        id: i64,
    },
    /// Mark a to-do as done (use the #number shown next to it).
    Done {
        id: i64,
    },
    /// Kept to-dos that are not done yet.
    Tasks,
    /// Print the notes, to-dos and text of one meeting.
    Show {
        /// Meeting id, like 2026-11-18-001.
        id: String,
    },
    /// The screen Terminal opens when a meeting's notes are ready.
    #[command(hide = true)]
    Popup {
        id: String,
    },
    /// Ask the LLM again for a meeting's title, summary and to-dos.
    Resummarize {
        /// Meeting id, like 2026-11-18-001.
        id: String,
    },
    /// List audio input devices.
    Devices,
    /// Show which apps are using the mic right now.
    MicUsers,
    /// Stop recording meetings until resume.
    Pause,
    /// Start recording meetings again.
    Resume,
    /// Record mic and system audio into chunks by hand (test only).
    Record {
        /// Stop after this many seconds. Without it, stop with Ctrl+C.
        #[arg(long)]
        seconds: Option<u64>,
    },
}

fn main() -> Result<()> {
    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    let config = Config::load(&cli.config)?;

    let Some(command) = cli.command else {
        return cli::run(&config);
    };

    match command {
        Command::Run => run(&config),
        Command::Status => status(&cli.config, &config),
        Command::List => list(&config),
        Command::Show { id } => show(&config, &id),
        Command::Popup { id } => cli::popup(&config, &id),
        Command::Last => print_with_db(&config, |conn| match db::last_meeting_id(conn)? {
            Some(id) => view::notes(conn, &id),
            None => Ok("No meetings yet.\n".into()),
        }),
        Command::Todos => print_with_db(&config, view::all_todos),
        Command::Keep { id } => set_todo(&config, id, "kept"),
        Command::Drop { id } => set_todo(&config, id, "dropped"),
        Command::Done { id } => set_todo(&config, id, "done"),
        Command::Tasks => print_with_db(&config, view::open_tasks),
        Command::Resummarize { id } => resummarize(&config, &id),
        Command::Devices => devices(),
        Command::MicUsers => {
            let users = detect::mic_users()?;
            if users.is_empty() {
                println!("no app is using the mic");
            }
            for user in users {
                println!("{:<8} {}", user.pid, user.app);
            }
            Ok(())
        }
        Command::Record { seconds } => record(&config, seconds),
        Command::Pause => {
            cli::set_paused(&config, true)?;
            println!("paused: no meetings will be recorded until `resume`");
            Ok(())
        }
        Command::Resume => {
            cli::set_paused(&config, false)?;
            println!("resumed: watching for meetings");
            Ok(())
        }
    }
}

/// Print every audio input device name.
fn devices() -> Result<()> {
    for name in audio::list_inputs()? {
        println!("{name}");
    }

    Ok(())
}

/// Set a flag when Ctrl+C is pressed, so loops can stop cleanly.
fn ctrl_c_flag() -> Result<Arc<AtomicBool>> {
    let stop = Arc::new(AtomicBool::new(false));
    let flag = stop.clone();
    ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))?;

    Ok(stop)
}

/// Record both sources into a test folder until the time is up or Ctrl+C.
fn record(config: &Config, seconds: Option<u64>) -> Result<()> {
    let _guard = init_logging(&config.log_dir)?;

    let stamp = chrono::Local::now().format("%Y-%m-%d_%H-%M-%S");
    let dir = config.meetings_dir.join(format!("test-{stamp}"));
    let stop = ctrl_c_flag()?;
    let (tx, _chunks) = std::sync::mpsc::channel();

    let sources = [
        audio::Source::start("mic", &config.mic_device, &dir.join("mic"), config.chunk_seconds, tx.clone())?,
        audio::Source::start("system", &config.system_device, &dir.join("system"), config.chunk_seconds, tx)?,
    ];

    info!(dir = %dir.display(), "recording (Ctrl+C to stop)");

    let started = Instant::now();
    let limit = seconds.map(Duration::from_secs);

    while !stop.load(Ordering::Relaxed) && limit.is_none_or(|l| started.elapsed() < l) {
        std::thread::sleep(Duration::from_secs(1));

        let levels: Vec<String> = sources
            .iter()
            .map(|s| format!("{} {:>4.0} dB", s.label(), s.take_peak_db()))
            .collect();
        info!("{:>3}s  {}", started.elapsed().as_secs(), levels.join("   "));
    }

    for source in sources {
        let label = source.label().to_string();
        let chunks = source.stop()?;
        println!("{label:<7} {chunks} chunk(s) in {}", dir.join(&label).display());
    }

    Ok(())
}

/// After a failure (like an unplugged mic), wait this long before trying again.
const START_FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

/// The daemon: check once a second for a meeting, and record it while it lasts.
fn run(config: &Config) -> Result<()> {
    let _guard = init_logging(&config.log_dir)?;
    let _lock = recover::single_instance_lock(config)?;
    let conn = db::open(&config.db_path)?;
    recover::recover(config, &conn)?;
    let stop = ctrl_c_flag()?;

    let transcriber = worker::start(config.clone(), stop.clone())?;
    let mut detector = Detector::new(config.detector_settings());
    let mut capture: Option<Capture> = None;
    let mut cooldown_until: Option<Instant> = None;

    info!(
        apps = ?config.meeting_apps,
        ignored = ?config.ignore_apps,
        "meetingd started, watching for meetings (Ctrl+C to stop)"
    );

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));

        if cooldown_until.is_some_and(|until| Instant::now() < until) {
            continue;
        }
        cooldown_until = None;

        let paused = config.pause_file().exists();
        let app = if paused { None } else { current_meeting_app(config) };
        let (mic_db, system_db) = capture.as_mut().map_or((-100.0, -100.0), Capture::take_peaks);

        let tick = Tick { meeting_app_on_mic: app.is_some(), mic_db, system_db, paused };
        let action = detector.tick(tick);

        if let Err(err) = apply(action, &mut capture, config, &conn, app.as_deref()) {
            error!("{action:?} failed: {err:#}");
            if let Some(c) = capture.take() {
                c.discard().ok();
            }
            detector = Detector::new(config.detector_settings());
            cooldown_until = Some(Instant::now() + START_FAILURE_COOLDOWN);
            warn!("pausing detection for {}s after the failure", START_FAILURE_COOLDOWN.as_secs());
            continue;
        }

        if let Some(c) = &capture {
            c.save_closed_chunks(&conn)?;
            debug!(state = ?detector.state(), mic_db, system_db, "tick");
        }
    }

    if let Some(c) = capture.take() {
        match detector.state() {
            State::Recording { .. } => c.finish(&conn)?,
            _ => c.discard()?,
        }
    }

    transcriber.join().ok();
    info!("meetingd stopped");

    Ok(())
}

/// Carry out what the detector asked for.
fn apply(
    action: Action,
    capture: &mut Option<Capture>,
    config: &Config,
    conn: &rusqlite::Connection,
    app: Option<&str>,
) -> Result<()> {
    match action {
        Action::Nothing => {}
        Action::StartCapture => {
            *capture = Some(Capture::start(config, conn, app.unwrap_or("unknown"))?);
        }
        Action::ConfirmMeeting => {
            if let Some(c) = capture.as_mut() {
                c.confirm(conn)?;
            }
        }
        Action::DiscardCapture => {
            if let Some(c) = capture.take() {
                c.discard()?;
            }
        }
        Action::EndMeeting => {
            if let Some(c) = capture.take() {
                c.finish(conn)?;
            }
        }
    }

    Ok(())
}

/// The allowed meeting app using the mic right now, if any.
fn current_meeting_app(config: &Config) -> Option<String> {
    match detect::mic_users() {
        Ok(users) => detect::meeting_app_on_mic(&users, &config.meeting_apps, &config.ignore_apps),
        Err(err) => {
            warn!("could not read mic users: {err:#}");
            None
        }
    }
}

/// Print every path the daemon uses and whether it exists.
fn status(config_path: &Path, config: &Config) -> Result<()> {
    let config_note = if config_path.exists() { "" } else { "  (missing, using defaults)" };
    println!("config        {}{config_note}", config_path.display());

    for (name, path) in [
        ("database", &config.db_path),
        ("logs", &config.log_dir),
        ("meetings", &config.meetings_dir),
        ("python", &config.python_path),
        ("whisper", &config.whisper_model_path),
        ("pyannote", &config.pyannote_model_path),
    ] {
        let mark = if path.exists() { "ok" } else { "MISSING" };
        println!("{name:<13} {}  [{mark}]", path.display());
    }

    println!(
        "timings       chunk {}s, start after {}s, stop after {}s silence",
        config.chunk_seconds, config.start_confirm_seconds, config.silence_stop_seconds
    );
    println!("devices       mic \"{}\", system \"{}\"", config.mic_device, config.system_device);
    println!("detection     {}", if config.pause_file().exists() { "PAUSED" } else { "on" });

    Ok(())
}

/// Print all meetings from the database.
fn list(config: &Config) -> Result<()> {
    print_with_db(config, view::meetings)
}

/// Mark one to-do as kept or dropped.
fn set_todo(config: &Config, id: i64, status: &str) -> Result<()> {
    let conn = db::open(&config.db_path)?;

    if !db::set_todo_status(&conn, id, status)? {
        anyhow::bail!("no to-do #{id}. Run `preeti todos` to see the numbers");
    }
    println!("to-do #{id} {status}");

    Ok(())
}

/// Open the DB, build some text, and print it.
fn print_with_db(config: &Config, build: impl Fn(&rusqlite::Connection) -> Result<String>) -> Result<()> {
    let conn = db::open(&config.db_path)?;
    print!("{}", build(&conn)?);

    Ok(())
}

/// Print one meeting's notes, to-dos and full transcript.
fn show(config: &Config, id: &str) -> Result<()> {
    print_with_db(config, |conn| {
        Ok(format!("{}\n{}\n{}", view::notes(conn, id)?, console::style("Transcript").bold(), view::transcript(conn, id)?))
    })
}

/// Queue a meeting for a fresh summary. Kept or dropped to-dos are left as they are.
fn resummarize(config: &Config, id: &str) -> Result<()> {
    let conn = db::open(&config.db_path)?;
    db::set_meeting_status(&conn, id, "labelled")?;
    println!("{id} queued; the running daemon will summarize it again");

    Ok(())
}

const LOG_DAYS_KEPT: usize = 7;

/// Send logs to the terminal (when there is one) and to a daily `meetingd.YYYY-MM-DD.log`, keeping the last 7 days. Keep the guard alive so logs flush.
fn init_logging(log_dir: &Path) -> Result<WorkerGuard> {
    std::fs::create_dir_all(log_dir)?;

    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("meetingd")
        .filename_suffix("log")
        .max_log_files(LOG_DAYS_KEPT)
        .build(log_dir)?;
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    let to_terminal = std::io::stdout().is_terminal().then(fmt::layer);

    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(to_terminal)
        .with(fmt::layer().with_ansi(false).with_writer(file_writer))
        .init();

    Ok(guard)
}
