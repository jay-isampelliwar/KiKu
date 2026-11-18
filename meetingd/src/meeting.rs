//! One capture: both audio sources writing into a meeting folder, plus its DB rows.

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, channel};

use anyhow::Result;
use rusqlite::Connection;
use tracing::{info, warn};

use crate::audio::{ClosedChunk, Source};
use crate::config::Config;
use crate::db;

pub struct Capture {
    pub id: String,
    pub dir: PathBuf,
    started_at: String,
    sources: Vec<Source>,
    chunks: Receiver<ClosedChunk>,
    /// Saved in the DB only once the meeting is confirmed.
    confirmed: bool,
    /// Loudest mic level seen. A real mic always hears some room noise.
    mic_loudest: f32,
}

impl Capture {
    /// Start recording mic and system audio into a new meeting folder. Nothing is in the DB yet.
    pub fn start(config: &Config, conn: &Connection, app: &str) -> Result<Self> {
        let now = chrono::Local::now();
        let id = db::next_meeting_id(conn, &now.format("%Y-%m-%d").to_string())?;
        let dir = config.meetings_dir.join(&id);
        let (tx, chunks) = channel();

        let sources = vec![
            Source::start("mic", &config.mic_device, &dir.join("mic"), config.chunk_seconds, tx.clone())?,
            Source::start("system", &config.system_device, &dir.join("system"), config.chunk_seconds, tx)?,
        ];

        info!(meeting = %id, %app, "possible meeting, capturing");

        Ok(Self {
            id,
            dir,
            started_at: now.to_rfc3339(),
            sources,
            chunks,
            confirmed: false,
            mic_loudest: -100.0,
        })
    }

    /// Loudest mic and system level in the last second, in dB.
    pub fn take_peaks(&mut self) -> (f32, f32) {
        let (mic, system) = (self.sources[0].take_peak_db(), self.sources[1].take_peak_db());
        self.mic_loudest = self.mic_loudest.max(mic);

        (mic, system)
    }

    /// Pure digital silence from the mic means macOS is blocking it, not a quiet room.
    fn warn_if_mic_blocked(&self) {
        if self.mic_loudest <= -99.0 {
            warn!(
                meeting = %self.id,
                "the mic gave pure silence. macOS is probably blocking it: open System Settings > \
                 Privacy & Security > Microphone and allow meetingd"
            );
        }
    }

    /// It is a real call: save the meeting and every chunk closed so far.
    pub fn confirm(&mut self, conn: &Connection) -> Result<()> {
        db::create_meeting(conn, &self.id, &self.started_at, &self.dir)?;
        self.confirmed = true;
        self.save_closed_chunks(conn)?;
        info!(meeting = %self.id, dir = %self.dir.display(), "meeting started");

        Ok(())
    }

    /// Write rows for chunks that finished since the last call. Only after confirm.
    pub fn save_closed_chunks(&self, conn: &Connection) -> Result<()> {
        if !self.confirmed {
            return Ok(());
        }

        for chunk in self.chunks.try_iter() {
            db::add_chunk(conn, &self.id, chunk.index, &chunk.source, &chunk.path)?;
        }

        Ok(())
    }

    /// Not a call. Stop and delete everything.
    pub fn discard(self) -> Result<()> {
        self.warn_if_mic_blocked();
        let Capture { id, dir, sources, .. } = self;
        stop_all(sources);

        std::fs::remove_dir_all(&dir).ok();
        info!(meeting = %id, "not a meeting, discarded");

        Ok(())
    }

    /// Meeting over. Stop, save the last chunks, and mark it recorded.
    pub fn finish(self, conn: &Connection) -> Result<()> {
        self.warn_if_mic_blocked();
        let Capture { id, sources, chunks, .. } = self;
        stop_all(sources);

        for chunk in chunks.try_iter() {
            db::add_chunk(conn, &id, chunk.index, &chunk.source, &chunk.path)?;
        }

        db::finish_meeting(conn, &id, &chrono::Local::now().to_rfc3339())?;
        info!(meeting = %id, "meeting ended");

        Ok(())
    }
}

/// Stop every source. A failure on one is logged and does not stop the others.
fn stop_all(sources: Vec<Source>) {
    for source in sources {
        let label = source.label().to_string();
        if let Err(err) = source.stop() {
            warn!(source = %label, "stopping failed: {err:#}");
        }
    }
}
