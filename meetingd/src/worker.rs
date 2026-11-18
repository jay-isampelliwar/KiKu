//! Runs the Python worker on its own thread, so meeting detection never waits on it.
//! Jobs in order: turn pending chunks into text, label finished meetings, then summarize them.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, channel};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, error, info, warn};

use crate::config::Config;
use crate::db::{self, NewSegment, PendingChunk};
use crate::labels::{self, Turn, Word};

const READY_TIMEOUT: Duration = Duration::from_secs(180);
const CHUNK_TIMEOUT: Duration = Duration::from_secs(300);
const DIARIZE_TIMEOUT: Duration = Duration::from_secs(3600);
const SUMMARY_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_ATTEMPTS: u32 = 3;
const IDLE_WAIT: Duration = Duration::from_secs(2);

/// One JSON line from the worker.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
enum Reply {
    Ready { load_seconds: f64 },
    Result { request_id: String, segments: Vec<RawSegment>, seconds: f64 },
    Turns { request_id: String, turns: Vec<Turn>, seconds: f64 },
    Summary { request_id: String, summary: Value, seconds: f64 },
    Error { request_id: Option<String>, error: String },
}

impl Reply {
    fn request_id(&self) -> Option<&str> {
        match self {
            Reply::Ready { .. } => None,
            Reply::Result { request_id, .. }
            | Reply::Turns { request_id, .. }
            | Reply::Summary { request_id, .. } => Some(request_id),
            Reply::Error { request_id, .. } => request_id.as_deref(),
        }
    }
}

#[derive(Debug, Deserialize)]
struct RawSegment {
    start: f64,
    end: f64,
    text: String,
    #[serde(default)]
    words: Vec<Word>,
}

/// Why a request did not succeed.
enum Failure {
    /// The worker answered with an error for this request. The worker is still fine.
    Request(String),
    /// The worker died, hung, or sent nonsense. It must be restarted.
    Process(anyhow::Error),
    /// The daemon is shutting down. The work stays queued for next time.
    Stopped,
}

/// A running Python worker with Whisper loaded.
struct WorkerProcess {
    child: Child,
    stdin: ChildStdin,
    replies: Receiver<Reply>,
}

impl WorkerProcess {
    /// Start Python and wait until it says the model is loaded.
    fn spawn(config: &Config) -> Result<Self> {
        let mut child = Command::new(&config.python_path)
            .arg(&config.worker_script)
            .arg("--whisper-model")
            .arg(&config.whisper_model_path)
            .arg("--pyannote-model")
            .arg(&config.pyannote_model_path)
            .env("HF_HUB_OFFLINE", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("starting {}", config.python_path.display()))?;

        let stdin = child.stdin.take().expect("stdin is piped");
        let stdout = child.stdout.take().expect("stdout is piped");
        let stderr = child.stderr.take().expect("stderr is piped");

        let (tx, replies) = channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                match serde_json::from_str::<Reply>(&line) {
                    Ok(reply) => {
                        if tx.send(reply).is_err() {
                            break;
                        }
                    }
                    Err(err) => warn!("worker sent a bad line ({err}): {line}"),
                }
            }
        });
        std::thread::spawn(move || {
            for line in BufReader::new(stderr).lines().map_while(Result::ok) {
                debug!(target: "worker", "{line}");
            }
        });

        let worker = Self { child, stdin, replies };

        match worker.replies.recv_timeout(READY_TIMEOUT) {
            Ok(Reply::Ready { load_seconds }) => {
                info!(pid = worker.child.id(), load_seconds, "worker ready");
                Ok(worker)
            }
            Ok(other) => bail!("worker sent {other:?} before ready"),
            Err(_) => bail!(
                "worker did not get ready. Run it by hand to see why: {} {}",
                config.python_path.display(),
                config.worker_script.display()
            ),
        }
    }

    /// Send one request and wait for the reply with the same `request_id`.
    fn request(&mut self, request: Value, timeout: Duration, stop: &AtomicBool) -> Result<Reply, Failure> {
        let request_id = request["request_id"].as_str().unwrap_or_default().to_string();

        writeln!(self.stdin, "{request}")
            .and_then(|_| self.stdin.flush())
            .map_err(|e| Failure::Process(anyhow!("writing to worker: {e}")))?;

        let deadline = Instant::now() + timeout;

        loop {
            if stop.load(Ordering::Relaxed) {
                return Err(Failure::Stopped);
            }

            match self.replies.recv_timeout(Duration::from_millis(500)) {
                Ok(Reply::Error { error, request_id: Some(id) }) if id == request_id => {
                    return Err(Failure::Request(error));
                }
                Ok(reply) if reply.request_id() == Some(request_id.as_str()) => return Ok(reply),
                Ok(other) => warn!("ignoring unexpected worker reply: {other:?}"),
                Err(RecvTimeoutError::Timeout) if Instant::now() >= deadline => {
                    return Err(Failure::Process(anyhow!("no reply in {}s", timeout.as_secs())));
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(Failure::Process(anyhow!("worker exited")));
                }
            }
        }
    }
}

impl Drop for WorkerProcess {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
    }
}

/// Start the thread that does all worker jobs until `stop` is set.
pub fn start(config: Config, stop: Arc<AtomicBool>) -> Result<JoinHandle<()>> {
    let conn = db::open(&config.db_path)?;

    let handle = std::thread::Builder::new()
        .name("transcriber".into())
        .spawn(move || worker_loop(&config, conn, &stop))?;

    Ok(handle)
}

fn worker_loop(config: &Config, mut conn: Connection, stop: &AtomicBool) {
    let mut worker: Option<WorkerProcess> = None;

    while !stop.load(Ordering::Relaxed) {
        if worker.is_none() {
            match WorkerProcess::spawn(config) {
                Ok(w) => worker = Some(w),
                Err(err) => {
                    error!("worker failed to start: {err:#}");
                    sleep_unless_stopped(Duration::from_secs(10), stop);
                    continue;
                }
            }
        }

        let w = worker.as_mut().expect("worker started above");

        let outcome = match next_job(&conn) {
            Ok(Some(Job::Transcribe(chunk))) => transcribe_chunk(config, &mut conn, w, &chunk, stop),
            Ok(Some(Job::Label(meeting_id))) => {
                let outcome = label_meeting(config, &mut conn, w, &meeting_id, stop);
                if !matches!(outcome, Outcome::Stopped) {
                    delete_audio(config, &conn, &meeting_id);
                }
                outcome
            }
            Ok(Some(Job::Summarize(meeting_id))) => {
                delete_audio(config, &conn, &meeting_id);
                summarize_meeting(config, &mut conn, w, &meeting_id, stop)
            }
            Ok(None) => {
                sleep_unless_stopped(IDLE_WAIT, stop);
                continue;
            }
            Err(err) => {
                error!("reading the job queue: {err:#}");
                sleep_unless_stopped(Duration::from_secs(5), stop);
                continue;
            }
        };

        match outcome {
            Outcome::Done => {}
            Outcome::Stopped => break,
            Outcome::RestartWorker => worker = None,
        }
    }
}

enum Job {
    Transcribe(PendingChunk),
    Label(String),
    Summarize(String),
}

enum Outcome {
    Done,
    Stopped,
    RestartWorker,
}

/// Chunks first, so a live meeting stays up to date. Then labelling, then summaries.
fn next_job(conn: &Connection) -> Result<Option<Job>> {
    if let Some(chunk) = db::next_pending_chunk(conn)? {
        return Ok(Some(Job::Transcribe(chunk)));
    }

    if let Some(id) = db::next_meeting_to_label(conn)? {
        return Ok(Some(Job::Label(id)));
    }

    Ok(db::next_meeting_to_summarize(conn)?.map(Job::Summarize))
}

fn transcribe_chunk(
    config: &Config,
    conn: &mut Connection,
    worker: &mut WorkerProcess,
    chunk: &PendingChunk,
    stop: &AtomicBool,
) -> Outcome {
    let request = json!({
        "type": "transcribe",
        "request_id": format!("chunk-{}", chunk.id),
        "path": chunk.path,
    });

    let (failure, outcome) = match worker.request(request, CHUNK_TIMEOUT, stop) {
        Ok(Reply::Result { segments, seconds, .. }) => match save_text(config, conn, chunk, segments) {
            Ok(lines) => {
                info!(
                    meeting = %chunk.meeting_id,
                    source = %chunk.source,
                    chunk = chunk.chunk_index,
                    lines,
                    seconds,
                    "chunk transcribed"
                );
                return Outcome::Done;
            }
            Err(err) => (format!("saving text: {err:#}"), Outcome::Done),
        },
        Ok(other) => (format!("wrong reply: {other:?}"), Outcome::Done),
        Err(Failure::Stopped) => return Outcome::Stopped,
        Err(Failure::Request(msg)) => (msg, Outcome::Done),
        Err(Failure::Process(err)) => {
            warn!("restarting worker: {err:#}");
            (format!("worker problem: {err:#}"), Outcome::RestartWorker)
        }
    };

    match db::chunk_failed(conn, chunk.id, &failure, MAX_ATTEMPTS) {
        Ok(true) => error!(chunk = chunk.id, path = %chunk.path, "chunk failed for good: {failure}"),
        Ok(false) => warn!(chunk = chunk.id, "chunk failed, will retry: {failure}"),
        Err(err) => error!("recording chunk failure: {err:#}"),
    }

    outcome
}

/// Diarize the meeting's system audio, then name speakers and remove mic leak.
fn label_meeting(
    config: &Config,
    conn: &mut Connection,
    worker: &mut WorkerProcess,
    meeting_id: &str,
    stop: &AtomicBool,
) -> Outcome {
    let result = (|| -> Result<Result<(Vec<Turn>, f64), Failure>> {
        let chunks: Vec<Value> = db::system_chunks(conn, meeting_id)?
            .into_iter()
            .filter(|(_, path)| std::path::Path::new(path).exists())
            .map(|(index, path)| json!({ "start": chunk_start(config, index), "path": path }))
            .collect();

        if chunks.is_empty() {
            return Ok(Ok((Vec::new(), 0.0)));
        }

        info!(meeting = %meeting_id, chunks = chunks.len(), "finding speakers");
        let request = json!({
            "type": "diarize",
            "request_id": format!("meeting-{meeting_id}"),
            "chunks": chunks,
        });

        Ok(match worker.request(request, DIARIZE_TIMEOUT, stop) {
            Ok(Reply::Turns { turns, seconds, .. }) => Ok((turns, seconds)),
            Ok(other) => Err(Failure::Request(format!("wrong reply: {other:?}"))),
            Err(failure) => Err(failure),
        })
    })();

    let (turns, seconds, outcome) = match result {
        Ok(Ok((turns, seconds))) => (turns, seconds, Outcome::Done),
        Ok(Err(Failure::Stopped)) => return Outcome::Stopped,
        Ok(Err(Failure::Request(msg))) => {
            error!(meeting = %meeting_id, "finding speakers failed: {msg}");
            return mark_label_failed(conn, meeting_id, Outcome::Done);
        }
        Ok(Err(Failure::Process(err))) => {
            error!(meeting = %meeting_id, "worker crashed while finding speakers: {err:#}");
            return mark_label_failed(conn, meeting_id, Outcome::RestartWorker);
        }
        Err(err) => {
            error!(meeting = %meeting_id, "preparing speaker labels: {err:#}");
            return mark_label_failed(conn, meeting_id, Outcome::Done);
        }
    };

    let applied = db::label_lines(conn, meeting_id).and_then(|lines| {
        let changes = labels::plan(&lines, &turns);
        db::apply_labels(conn, meeting_id, &changes, "labelled")?;
        Ok(changes)
    });

    match applied {
        Ok(changes) => {
            let count = |f: fn(&labels::Change) -> bool| changes.iter().filter(|c| f(c)).count();
            info!(
                meeting = %meeting_id,
                seconds,
                turns = turns.len(),
                labelled = count(|c| matches!(c, labels::Change::Speaker { .. })),
                trimmed = count(|c| matches!(c, labels::Change::Text { .. })),
                removed = count(|c| matches!(c, labels::Change::Delete { .. })),
                "meeting labelled"
            );
            outcome
        }
        Err(err) => {
            error!(meeting = %meeting_id, "saving speaker labels: {err:#}");
            mark_label_failed(conn, meeting_id, outcome)
        }
    }
}

/// Send the transcript to the LLM and save the title, notes and to-dos.
fn summarize_meeting(
    config: &Config,
    conn: &mut Connection,
    worker: &mut WorkerProcess,
    meeting_id: &str,
    stop: &AtomicBool,
) -> Outcome {
    let prepared = db::meeting_info(conn, meeting_id).and_then(|info| {
        let lines = db::meeting_segments(conn, meeting_id)?;
        Ok((info, lines))
    });

    let ((started_at, ended_at, _), lines) = match prepared {
        Ok(found) => found,
        Err(err) => {
            error!(meeting = %meeting_id, "reading transcript: {err:#}");
            db::summary_failed(conn, meeting_id, &format!("{err:#}")).ok();
            return Outcome::Done;
        }
    };

    if lines.is_empty() {
        let notes = json!({ "title": "No speech", "topics": [], "summary": "", "key_points": [], "todos": [] });
        if let Err(err) = db::save_summary(conn, meeting_id, "No speech", &notes, &[]) {
            error!(meeting = %meeting_id, "saving empty summary: {err:#}");
        }
        return Outcome::Done;
    }

    info!(meeting = %meeting_id, lines = lines.len(), model = %config.llm_model, "summarizing");

    let request = json!({
        "type": "summarize",
        "request_id": format!("summary-{meeting_id}"),
        "model": config.llm_model,
        "meeting": { "id": meeting_id, "started_at": started_at, "ended_at": ended_at },
        "lines": lines
            .iter()
            .map(|l| json!({ "start": l.start_sec, "speaker": l.speaker, "text": l.text }))
            .collect::<Vec<_>>(),
    });

    let (error, outcome) = match worker.request(request, SUMMARY_TIMEOUT, stop) {
        Ok(Reply::Summary { summary, seconds, .. }) => match save_notes(conn, meeting_id, &summary) {
            Ok(todos) => {
                info!(meeting = %meeting_id, seconds, todos, "meeting completed");
                if config.popup_when_done
                    && let Err(err) = crate::popup::show_meeting(config, meeting_id)
                {
                    warn!(meeting = %meeting_id, "opening notes window: {err:#}");
                }
                return Outcome::Done;
            }
            Err(err) => (format!("saving summary: {err:#}"), Outcome::Done),
        },
        Ok(other) => (format!("wrong reply: {other:?}"), Outcome::Done),
        Err(Failure::Stopped) => return Outcome::Stopped,
        Err(Failure::Request(msg)) => (msg, Outcome::Done),
        Err(Failure::Process(err)) => (format!("worker problem: {err:#}"), Outcome::RestartWorker),
    };

    error!(meeting = %meeting_id, "summary failed: {error}");
    if let Err(err) = db::summary_failed(conn, meeting_id, &error) {
        error!("recording summary failure: {err:#}");
    }

    outcome
}

/// Pull the title and to-dos out of the LLM notes and save everything.
fn save_notes(conn: &mut Connection, meeting_id: &str, notes: &Value) -> Result<usize> {
    let title = notes["title"].as_str().unwrap_or("Untitled meeting");
    let text = |v: &Value| v.as_str().map(str::to_string);

    let todos: Vec<db::NewTodo> = notes["todos"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            Some(db::NewTodo { task: text(&t["task"])?, due: text(&t["due"]), quote: text(&t["quote"]) })
        })
        .collect();

    db::save_summary(conn, meeting_id, title, notes, &todos)?;

    Ok(todos.len())
}

/// Delete the meeting's audio folder. Only text is kept.
fn delete_audio(config: &Config, conn: &Connection, meeting_id: &str) {
    match db::meeting_info(conn, meeting_id) {
        Ok((_, _, folder)) => {
            if crate::recover::delete_meeting_folder(config, std::path::Path::new(&folder)) {
                info!(meeting = %meeting_id, "audio deleted");
            }
        }
        Err(err) => warn!(meeting = %meeting_id, "cannot find audio folder: {err:#}"),
    }
}

/// Stop retrying this meeting. Its text stays, just without speaker names.
fn mark_label_failed(conn: &Connection, meeting_id: &str, outcome: Outcome) -> Outcome {
    if let Err(err) = db::set_meeting_status(conn, meeting_id, "label_failed") {
        error!("marking meeting {meeting_id} label_failed: {err:#}");
    }

    outcome
}

/// Where a chunk starts on the meeting clock, in seconds.
fn chunk_start(config: &Config, chunk_index: u32) -> f64 {
    f64::from(chunk_index.saturating_sub(1) * config.chunk_seconds)
}

/// Shift times to the meeting clock, name the speaker by source, and save.
fn save_text(config: &Config, conn: &mut Connection, chunk: &PendingChunk, raw: Vec<RawSegment>) -> Result<usize> {
    let offset = chunk_start(config, chunk.chunk_index);
    let speaker = if chunk.source == "mic" { "Me" } else { "Others" };

    let segments: Vec<NewSegment> = raw
        .into_iter()
        .filter(|s| !s.text.trim().is_empty())
        .map(|s| NewSegment {
            start_sec: offset + s.start,
            end_sec: offset + s.end,
            speaker: speaker.to_string(),
            text: s.text,
            words: s
                .words
                .into_iter()
                .map(|w| Word { start: offset + w.start, end: offset + w.end, word: w.word })
                .collect(),
        })
        .collect();

    db::save_chunk_text(conn, chunk, &segments)?;

    Ok(segments.len())
}

fn sleep_unless_stopped(total: Duration, stop: &AtomicBool) {
    let until = Instant::now() + total;

    while Instant::now() < until && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(200));
    }
}
