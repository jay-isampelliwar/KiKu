//! Startup safety: one daemon at a time, and clean up whatever a crash or power cut left behind.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use rusqlite::Connection;
use tracing::{info, warn};

use crate::config::Config;
use crate::db;

const WAV_HEADER: u64 = 44;
/// Chunks shorter than half a second are not worth keeping.
const MIN_CHUNK_BYTES: u64 = 16_000;

/// Hold an exclusive lock so a second `meetingd run` refuses to start. Keep the file alive.
pub fn single_instance_lock(config: &Config) -> Result<File> {
    let path = config.db_path.with_file_name("meetingd.lock");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }

    let file = File::create(&path).with_context(|| format!("creating {}", path.display()))?;

    match file.try_lock() {
        Ok(()) => Ok(file),
        Err(TryLockError::WouldBlock) => bail!("another meetingd is already running"),
        Err(TryLockError::Error(err)) => Err(err).context("locking meetingd.lock"),
    }
}

/// True if a `meetingd run` currently holds the lock.
pub fn daemon_running(config: &Config) -> bool {
    let path = config.db_path.with_file_name("meetingd.lock");

    match File::open(&path) {
        Ok(file) => matches!(file.try_lock(), Err(TryLockError::WouldBlock)),
        Err(_) => false,
    }
}

/// Fix everything a crash can leave behind. Safe to run on every start.
pub fn recover(config: &Config, conn: &Connection) -> Result<()> {
    for (id, folder) in db::meetings_with_status(conn, &["recording"])? {
        close_cut_off_meeting(conn, &id, Path::new(&folder))?;
    }

    for (id, folder) in db::meetings_with_status(conn, &["labelled", "label_failed", "completed", "summary_failed"])? {
        if Path::new(&folder).exists() {
            info!(meeting = %id, "removing leftover audio");
            delete_meeting_folder(config, Path::new(&folder));
        }
    }

    remove_unconfirmed_captures(config, conn)?;

    Ok(())
}

/// A meeting was recording when the daemon died. Save the chunks on disk and close it,
/// so it goes through text, labels and summary like any other meeting.
fn close_cut_off_meeting(conn: &Connection, id: &str, folder: &Path) -> Result<()> {
    let mut added = 0;
    let mut last_write = None;

    for source in ["mic", "system"] {
        for (index, path) in chunk_files(&folder.join(source))? {
            if let Ok(modified) = path.metadata().and_then(|m| m.modified()) {
                last_write = last_write.max(Some(modified));
            }

            if db::chunk_exists(conn, id, source, index)? {
                continue;
            }

            if repair_wav(&path)? {
                db::add_chunk(conn, id, index, source, &path)?;
                added += 1;
            } else {
                std::fs::remove_file(&path).ok();
            }
        }
    }

    let ended_at = last_write
        .map(|t| chrono::DateTime::<chrono::Local>::from(t).to_rfc3339())
        .unwrap_or_else(|| chrono::Local::now().to_rfc3339());

    db::finish_meeting(conn, id, &ended_at)?;
    warn!(meeting = %id, recovered_chunks = added, "meeting was cut off by a crash; closed it");

    Ok(())
}

/// `chunk-0007.wav` files in a folder, as (7, path), sorted.
fn chunk_files(dir: &Path) -> Result<Vec<(u32, PathBuf)>> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Ok(Vec::new());
    };

    let mut files: Vec<(u32, PathBuf)> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter_map(|path| {
            let name = path.file_name()?.to_str()?;
            let index = name.strip_prefix("chunk-")?.strip_suffix(".wav")?.parse().ok()?;
            Some((index, path))
        })
        .collect();

    files.sort();
    Ok(files)
}

/// Make a WAV written up to a crash readable again by fixing its two size fields.
/// Returns false if the file is too short or not one of our WAV files.
fn repair_wav(path: &Path) -> Result<bool> {
    let mut file = OpenOptions::new().read(true).write(true).open(path)?;
    let mut len = file.metadata()?.len();

    if len < WAV_HEADER + MIN_CHUNK_BYTES {
        return Ok(false);
    }

    let mut header = [0u8; WAV_HEADER as usize];
    file.read_exact(&mut header)?;
    if &header[0..4] != b"RIFF" || &header[36..40] != b"data" {
        warn!(path = %path.display(), "not a chunk WAV, skipping");
        return Ok(false);
    }

    if (len - WAV_HEADER) % 2 == 1 {
        len -= 1;
        file.set_len(len)?;
    }

    let riff_size = u32::try_from(len - 8).context("chunk too large")?;
    let data_size = u32::try_from(len - WAV_HEADER).context("chunk too large")?;

    file.seek(SeekFrom::Start(4))?;
    file.write_all(&riff_size.to_le_bytes())?;
    file.seek(SeekFrom::Start(40))?;
    file.write_all(&data_size.to_le_bytes())?;

    Ok(true)
}

/// Folders like `2026-11-18-004` with no DB row were captures that never became a meeting.
fn remove_unconfirmed_captures(config: &Config, conn: &Connection) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(&config.meetings_dir) else {
        return Ok(());
    };

    for path in entries.filter_map(|e| e.ok().map(|e| e.path())) {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };

        if path.is_dir() && looks_like_meeting_id(name) && !db::meeting_exists(conn, name)? {
            info!(folder = %path.display(), "removing unconfirmed capture");
            delete_meeting_folder(config, &path);
        }
    }

    Ok(())
}

/// `YYYY-MM-DD-NNN`, digits and dashes in exactly those places.
fn looks_like_meeting_id(name: &str) -> bool {
    let b = name.as_bytes();

    b.len() == 14
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 | 10 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

/// Delete a meeting's audio folder. Refuses anything outside `meetings_dir`.
/// Returns true only if a folder was really removed.
pub fn delete_meeting_folder(config: &Config, folder: &Path) -> bool {
    if !folder.starts_with(&config.meetings_dir) || folder == config.meetings_dir {
        warn!(folder = %folder.display(), "not deleting a folder outside meetings_dir");
        return false;
    }

    match std::fs::remove_dir_all(folder) {
        Ok(()) => true,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => false,
        Err(err) => {
            warn!(folder = %folder.display(), "deleting audio: {err}");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meeting_id_pattern() {
        assert!(looks_like_meeting_id("2026-11-18-004"));
        assert!(!looks_like_meeting_id("test-2026-11-18_11-26-46"));
        assert!(!looks_like_meeting_id("2026-11-18-04"));
        assert!(!looks_like_meeting_id("2026_11-18-004"));
    }

    #[test]
    fn repairs_a_wav_cut_off_mid_write() {
        let dir = std::env::temp_dir().join(format!("meetingd-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("chunk-0001.wav");

        let spec = hound::WavSpec { channels: 1, sample_rate: 16_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let mut writer = hound::WavWriter::create(&path, spec).unwrap();
        for i in 0..16_000 {
            writer.write_sample((i % 100) as i16).unwrap();
        }
        writer.flush().unwrap();
        std::mem::forget(writer);

        let mut file = OpenOptions::new().write(true).open(&path).unwrap();
        file.seek(SeekFrom::Start(4)).unwrap();
        file.write_all(&[0; 4]).unwrap();
        file.seek(SeekFrom::Start(40)).unwrap();
        file.write_all(&[0; 4]).unwrap();
        drop(file);

        assert!(repair_wav(&path).unwrap());
        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.len(), 16_000);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn too_short_wav_is_rejected() {
        let path = std::env::temp_dir().join(format!("meetingd-short-{}.wav", std::process::id()));
        std::fs::write(&path, b"RIFF").unwrap();
        assert!(!repair_wav(&path).unwrap());
        std::fs::remove_file(&path).ok();
    }
}
