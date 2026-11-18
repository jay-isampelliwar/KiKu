use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

use crate::labels::{Change, Line, Word};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meetings (
    id          TEXT PRIMARY KEY,
    started_at  TEXT NOT NULL,
    ended_at    TEXT,
    status      TEXT NOT NULL,
    folder      TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS chunks (
    id          INTEGER PRIMARY KEY,
    meeting_id  TEXT NOT NULL REFERENCES meetings(id),
    chunk_index INTEGER NOT NULL,
    source      TEXT NOT NULL,
    path        TEXT NOT NULL,
    status      TEXT NOT NULL DEFAULT 'pending'
);

CREATE TABLE IF NOT EXISTS todos (
    id          INTEGER PRIMARY KEY,
    meeting_id  TEXT NOT NULL REFERENCES meetings(id),
    task        TEXT NOT NULL,
    due         TEXT,
    quote       TEXT,
    status      TEXT NOT NULL DEFAULT 'suggested',
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS segments (
    id          INTEGER PRIMARY KEY,
    meeting_id  TEXT NOT NULL REFERENCES meetings(id),
    start_sec   REAL NOT NULL,
    end_sec     REAL NOT NULL,
    speaker     TEXT,
    text        TEXT NOT NULL
);
";

/// One row of the `meetings` table.
pub struct Meeting {
    pub id: String,
    pub title: Option<String>,
    pub started_at: String,
    pub ended_at: Option<String>,
    pub status: String,
}

/// Open the database, creating the file, its folder and the tables if needed.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating db folder {}", dir.display()))?;
    }

    let conn =
        Connection::open(path).with_context(|| format!("opening db {}", path.display()))?;
    conn.busy_timeout(std::time::Duration::from_secs(5))?;
    conn.execute_batch("PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL;")?;
    conn.execute_batch(SCHEMA)?;
    add_column_if_missing(&conn, "chunks", "attempts", "INTEGER NOT NULL DEFAULT 0")?;
    add_column_if_missing(&conn, "chunks", "error", "TEXT")?;
    add_column_if_missing(&conn, "segments", "chunk_id", "INTEGER REFERENCES chunks(id)")?;
    add_column_if_missing(&conn, "segments", "words", "TEXT")?;
    add_column_if_missing(&conn, "meetings", "title", "TEXT")?;
    add_column_if_missing(&conn, "meetings", "summary", "TEXT")?;
    add_column_if_missing(&conn, "meetings", "summary_error", "TEXT")?;
    add_column_if_missing(&conn, "todos", "done_at", "TEXT")?;

    Ok(conn)
}

/// All meetings, newest first.
pub fn list_meetings(conn: &Connection) -> Result<Vec<Meeting>> {
    let mut stmt = conn.prepare(
        "SELECT id, started_at, ended_at, status, title FROM meetings ORDER BY started_at DESC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(Meeting {
            id: row.get(0)?,
            started_at: row.get(1)?,
            ended_at: row.get(2)?,
            status: row.get(3)?,
            title: row.get(4)?,
        })
    })?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// Next free meeting id for a date, like `2026-11-18-003`.
pub fn next_meeting_id(conn: &Connection, date: &str) -> Result<String> {
    let count: u32 = conn.query_row(
        "SELECT COUNT(*) FROM meetings WHERE id LIKE ?1 || '-%'",
        [date],
        |row| row.get(0),
    )?;

    Ok(format!("{date}-{:03}", count + 1))
}

/// Save a new meeting with status `recording`.
pub fn create_meeting(conn: &Connection, id: &str, started_at: &str, folder: &Path) -> Result<()> {
    conn.execute(
        "INSERT INTO meetings (id, started_at, status, folder) VALUES (?1, ?2, 'recording', ?3)",
        (id, started_at, folder.to_string_lossy()),
    )?;

    Ok(())
}

/// Save one finished chunk file with status `pending`.
pub fn add_chunk(conn: &Connection, meeting_id: &str, index: u32, source: &str, path: &Path) -> Result<()> {
    conn.execute(
        "INSERT INTO chunks (meeting_id, chunk_index, source, path) VALUES (?1, ?2, ?3, ?4)",
        (meeting_id, index, source, path.to_string_lossy()),
    )?;

    Ok(())
}

/// Mark the meeting as recorded and set its end time.
pub fn finish_meeting(conn: &Connection, id: &str, ended_at: &str) -> Result<()> {
    conn.execute(
        "UPDATE meetings SET ended_at = ?2, status = 'recorded' WHERE id = ?1",
        (id, ended_at),
    )?;

    Ok(())
}

/// Add a column to an existing table if an older database does not have it yet.
fn add_column_if_missing(conn: &Connection, table: &str, column: &str, definition: &str) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM pragma_table_info(?1) WHERE name = ?2",
        (table, column),
        |row| row.get(0),
    )?;

    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {definition}"))?;
    }

    Ok(())
}

/// A chunk waiting to be turned into text.
pub struct PendingChunk {
    pub id: i64,
    pub meeting_id: String,
    pub chunk_index: u32,
    pub source: String,
    pub path: String,
}

/// The oldest chunk that still needs text, if any.
pub fn next_pending_chunk(conn: &Connection) -> Result<Option<PendingChunk>> {
    let mut stmt = conn.prepare(
        "SELECT id, meeting_id, chunk_index, source, path FROM chunks
         WHERE status = 'pending' ORDER BY id LIMIT 1",
    )?;

    let mut rows = stmt.query_map([], |row| {
        Ok(PendingChunk {
            id: row.get(0)?,
            meeting_id: row.get(1)?,
            chunk_index: row.get(2)?,
            source: row.get(3)?,
            path: row.get(4)?,
        })
    })?;

    Ok(rows.next().transpose()?)
}

/// One line of text with times relative to the start of the meeting.
pub struct NewSegment {
    pub start_sec: f64,
    pub end_sec: f64,
    pub speaker: String,
    pub text: String,
    pub words: Vec<Word>,
}

/// Save a chunk's text and mark the chunk done, all in one transaction.
/// Old segments of the same chunk are removed first, so a retry never duplicates lines.
pub fn save_chunk_text(conn: &mut Connection, chunk: &PendingChunk, segments: &[NewSegment]) -> Result<()> {
    let tx = conn.transaction()?;

    tx.execute("DELETE FROM segments WHERE chunk_id = ?1", [chunk.id])?;

    for s in segments {
        tx.execute(
            "INSERT INTO segments (meeting_id, chunk_id, start_sec, end_sec, speaker, text, words)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                &chunk.meeting_id,
                chunk.id,
                s.start_sec,
                s.end_sec,
                &s.speaker,
                &s.text,
                serde_json::to_string(&s.words)?,
            ),
        )?;
    }

    tx.execute("UPDATE chunks SET status = 'done', error = NULL WHERE id = ?1", [chunk.id])?;
    tx.commit()?;

    Ok(())
}

/// Count a failed try. After `max_attempts` the chunk is marked `failed` and skipped.
pub fn chunk_failed(conn: &Connection, chunk_id: i64, error: &str, max_attempts: u32) -> Result<bool> {
    conn.execute(
        "UPDATE chunks SET attempts = attempts + 1, error = ?2,
           status = CASE WHEN attempts + 1 >= ?3 THEN 'failed' ELSE status END
         WHERE id = ?1",
        (chunk_id, error, max_attempts),
    )?;

    let status: String =
        conn.query_row("SELECT status FROM chunks WHERE id = ?1", [chunk_id], |row| row.get(0))?;

    Ok(status == "failed")
}

/// One saved line of a meeting transcript.
pub struct Segment {
    pub start_sec: f64,
    pub speaker: String,
    pub text: String,
}

/// All lines of a meeting, in time order.
pub fn meeting_segments(conn: &Connection, meeting_id: &str) -> Result<Vec<Segment>> {
    let mut stmt = conn.prepare(
        "SELECT start_sec, COALESCE(speaker, '?'), text FROM segments
         WHERE meeting_id = ?1 ORDER BY start_sec, id",
    )?;

    let rows = stmt.query_map([meeting_id], |row| {
        Ok(Segment { start_sec: row.get(0)?, speaker: row.get(1)?, text: row.get(2)? })
    })?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// The oldest ended meeting whose chunks all have text, so it can get speaker labels.
pub fn next_meeting_to_label(conn: &Connection) -> Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM meetings m WHERE status = 'recorded'
           AND NOT EXISTS (SELECT 1 FROM chunks c WHERE c.meeting_id = m.id AND c.status = 'pending')
         ORDER BY started_at LIMIT 1",
    )?;

    let mut rows = stmt.query_map([], |row| row.get(0))?;

    Ok(rows.next().transpose()?)
}

/// System-audio chunk files of a meeting as (chunk number, path), in order.
pub fn system_chunks(conn: &Connection, meeting_id: &str) -> Result<Vec<(u32, String)>> {
    let mut stmt = conn.prepare(
        "SELECT chunk_index, path FROM chunks
         WHERE meeting_id = ?1 AND source = 'system' ORDER BY chunk_index",
    )?;

    let rows = stmt.query_map([meeting_id], |row| Ok((row.get(0)?, row.get(1)?)))?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// Every line of a meeting with its source and words, for labelling.
pub fn label_lines(conn: &Connection, meeting_id: &str) -> Result<Vec<Line>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, c.source, s.start_sec, s.end_sec, s.words FROM segments s
         JOIN chunks c ON c.id = s.chunk_id
         WHERE s.meeting_id = ?1 ORDER BY s.start_sec",
    )?;

    let rows = stmt.query_map([meeting_id], |row| {
        let source: String = row.get(1)?;
        let words: Option<String> = row.get(4)?;
        Ok(Line {
            id: row.get(0)?,
            is_me: source == "mic",
            start: row.get(2)?,
            end: row.get(3)?,
            words: words.and_then(|w| serde_json::from_str(&w).ok()).unwrap_or_default(),
        })
    })?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// Apply all label edits and set the meeting's new status, in one transaction.
pub fn apply_labels(conn: &mut Connection, meeting_id: &str, changes: &[Change], status: &str) -> Result<()> {
    let tx = conn.transaction()?;

    for change in changes {
        match change {
            Change::Speaker { id, speaker } => {
                tx.execute("UPDATE segments SET speaker = ?2 WHERE id = ?1", (id, speaker))?;
            }
            Change::Text { id, text, words } => {
                tx.execute(
                    "UPDATE segments SET text = ?2, words = ?3 WHERE id = ?1",
                    (id, text, serde_json::to_string(words)?),
                )?;
            }
            Change::Delete { id } => {
                tx.execute("DELETE FROM segments WHERE id = ?1", [id])?;
            }
        }
    }

    tx.execute("UPDATE meetings SET status = ?2 WHERE id = ?1", (meeting_id, status))?;
    tx.commit()?;

    Ok(())
}

/// Set a meeting's status.
pub fn set_meeting_status(conn: &Connection, meeting_id: &str, status: &str) -> Result<()> {
    conn.execute("UPDATE meetings SET status = ?2 WHERE id = ?1", (meeting_id, status))?;

    Ok(())
}

/// The oldest labelled meeting that still needs a summary.
pub fn next_meeting_to_summarize(conn: &Connection) -> Result<Option<String>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM meetings WHERE status IN ('labelled', 'label_failed')
         ORDER BY started_at LIMIT 1",
    )?;

    let mut rows = stmt.query_map([], |row| row.get(0))?;

    Ok(rows.next().transpose()?)
}

/// Start time, end time and folder of a meeting.
pub fn meeting_info(conn: &Connection, id: &str) -> Result<(String, Option<String>, String)> {
    Ok(conn.query_row(
        "SELECT started_at, ended_at, folder FROM meetings WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

/// One to-do suggested by the LLM.
pub struct NewTodo {
    pub task: String,
    pub due: Option<String>,
    pub quote: Option<String>,
}

/// Save the LLM notes and to-dos and mark the meeting completed, in one transaction.
/// Earlier suggestions that were never kept or dropped are replaced.
pub fn save_summary(
    conn: &mut Connection,
    meeting_id: &str,
    title: &str,
    summary: &serde_json::Value,
    todos: &[NewTodo],
) -> Result<()> {
    let tx = conn.transaction()?;
    let now = chrono::Local::now().to_rfc3339();

    tx.execute("DELETE FROM todos WHERE meeting_id = ?1 AND status = 'suggested'", [meeting_id])?;

    for todo in todos {
        tx.execute(
            "INSERT INTO todos (meeting_id, task, due, quote, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            (meeting_id, &todo.task, &todo.due, &todo.quote, &now),
        )?;
    }

    tx.execute(
        "UPDATE meetings SET title = ?2, summary = ?3, summary_error = NULL, status = 'completed'
         WHERE id = ?1",
        (meeting_id, title, summary.to_string()),
    )?;
    tx.commit()?;

    Ok(())
}

/// Record why the summary failed. It is not retried until `resummarize`.
pub fn summary_failed(conn: &Connection, meeting_id: &str, error: &str) -> Result<()> {
    conn.execute(
        "UPDATE meetings SET status = 'summary_failed', summary_error = ?2 WHERE id = ?1",
        (meeting_id, error),
    )?;

    Ok(())
}

/// Title, notes JSON and last error of a meeting.
pub fn meeting_notes(conn: &Connection, id: &str) -> Result<(Option<String>, Option<String>, Option<String>)> {
    Ok(conn.query_row(
        "SELECT title, summary, summary_error FROM meetings WHERE id = ?1",
        [id],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?)
}

/// One saved to-do.
pub struct Todo {
    pub id: i64,
    pub task: String,
    pub due: Option<String>,
    pub quote: Option<String>,
    pub status: String,
}

/// All to-dos of a meeting.
pub fn meeting_todos(conn: &Connection, meeting_id: &str) -> Result<Vec<Todo>> {
    let mut stmt = conn.prepare("SELECT id, task, due, quote, status FROM todos WHERE meeting_id = ?1 ORDER BY id")?;

    let rows = stmt.query_map([meeting_id], |row| {
        Ok(Todo { id: row.get(0)?, task: row.get(1)?, due: row.get(2)?, quote: row.get(3)?, status: row.get(4)? })
    })?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// (id, folder) of every meeting in one of these statuses.
pub fn meetings_with_status(conn: &Connection, statuses: &[&str]) -> Result<Vec<(String, String)>> {
    let marks = vec!["?"; statuses.len()].join(", ");
    let mut stmt = conn.prepare(&format!("SELECT id, folder FROM meetings WHERE status IN ({marks})"))?;

    let rows = stmt.query_map(rusqlite::params_from_iter(statuses), |row| Ok((row.get(0)?, row.get(1)?)))?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// True if this chunk is already saved.
pub fn chunk_exists(conn: &Connection, meeting_id: &str, source: &str, index: u32) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) > 0 FROM chunks WHERE meeting_id = ?1 AND source = ?2 AND chunk_index = ?3",
        (meeting_id, source, index),
        |row| row.get(0),
    )?)
}

/// True if a meeting with this id is saved.
pub fn meeting_exists(conn: &Connection, id: &str) -> Result<bool> {
    Ok(conn.query_row("SELECT COUNT(*) > 0 FROM meetings WHERE id = ?1", [id], |row| row.get(0))?)
}

/// One meeting row, if it exists.
pub fn meeting(conn: &Connection, id: &str) -> Result<Option<Meeting>> {
    let mut stmt =
        conn.prepare("SELECT id, started_at, ended_at, status, title FROM meetings WHERE id = ?1")?;

    let mut rows = stmt.query_map([id], |row| {
        Ok(Meeting {
            id: row.get(0)?,
            started_at: row.get(1)?,
            ended_at: row.get(2)?,
            status: row.get(3)?,
            title: row.get(4)?,
        })
    })?;

    Ok(rows.next().transpose()?)
}

/// The id of the most recent meeting.
pub fn last_meeting_id(conn: &Connection) -> Result<Option<String>> {
    let mut stmt = conn.prepare("SELECT id FROM meetings ORDER BY started_at DESC LIMIT 1")?;
    let mut rows = stmt.query_map([], |row| row.get(0))?;

    Ok(rows.next().transpose()?)
}

/// A to-do together with the meeting it came from.
pub struct TodoWithMeeting {
    pub todo: Todo,
    pub meeting_id: String,
    pub meeting_title: Option<String>,
    pub started_at: String,
}

/// Every to-do, newest meeting first.
pub fn all_todos(conn: &Connection) -> Result<Vec<TodoWithMeeting>> {
    let mut stmt = conn.prepare(
        "SELECT t.id, t.task, t.due, t.quote, t.status, m.id, m.title, m.started_at
         FROM todos t JOIN meetings m ON m.id = t.meeting_id
         ORDER BY m.started_at DESC, t.id",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok(TodoWithMeeting {
            todo: Todo {
                id: row.get(0)?,
                task: row.get(1)?,
                due: row.get(2)?,
                quote: row.get(3)?,
                status: row.get(4)?,
            },
            meeting_id: row.get(5)?,
            meeting_title: row.get(6)?,
            started_at: row.get(7)?,
        })
    })?;

    Ok(rows.collect::<Result<_, _>>()?)
}

/// Meetings not yet finished being processed, and chunks waiting for text.
pub fn work_in_progress(conn: &Connection) -> Result<(u32, u32)> {
    let meetings = conn.query_row(
        "SELECT COUNT(*) FROM meetings WHERE status IN ('recording', 'recorded', 'labelled', 'label_failed')",
        [],
        |row| row.get(0),
    )?;
    let chunks = conn.query_row("SELECT COUNT(*) FROM chunks WHERE status = 'pending'", [], |row| row.get(0))?;

    Ok((meetings, chunks))
}

/// Set a to-do to `suggested`, `kept`, `done` or `dropped`, and stamp when it was done.
/// Returns false if there is no such to-do.
pub fn set_todo_status(conn: &Connection, id: i64, status: &str) -> Result<bool> {
    let now = chrono::Local::now().to_rfc3339();
    let changed = conn.execute(
        "UPDATE todos SET status = ?2, done_at = CASE WHEN ?2 = 'done' THEN ?3 ELSE NULL END WHERE id = ?1",
        (id, status, now),
    )?;

    Ok(changed > 0)
}
