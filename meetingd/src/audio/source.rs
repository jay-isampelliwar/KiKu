use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, SyncSender, TrySendError, sync_channel};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use tracing::{info, warn};

use super::device::{device_name, find_input};
use super::writer::{ChunkWriter, ClosedChunk, Downsampler, LevelMeter};

const SILENCE_DB: f32 = -100.0;

/// How many callback buffers may wait for the writer thread. About 5 s of audio at 48 kHz.
const QUEUE_BUFFERS: usize = 512;

/// One live recording from one input device, writing 30 s chunks to its own folder.
pub struct Source {
    label: String,
    stream: Stream,
    stop: Arc<AtomicBool>,
    peak_db: Arc<AtomicU32>,
    dropped: Arc<AtomicU64>,
    thread: JoinHandle<Result<u32>>,
}

impl Source {
    /// Open the device and start recording into `out_dir`. Each finished chunk is sent on `events`.
    pub fn start(
        label: &str,
        device_wanted: &str,
        out_dir: &Path,
        chunk_seconds: u32,
        events: Sender<ClosedChunk>,
    ) -> Result<Self> {
        let device = find_input(device_wanted)?;
        let name = device_name(&device).unwrap_or_else(|| device_wanted.to_string());

        let supported = device
            .default_input_config()
            .with_context(|| format!("reading input config of {name}"))?;
        let format = supported.sample_format();
        let config: StreamConfig = supported.into();
        let channels = usize::from(config.channels);
        let rate = config.sample_rate;

        info!(source = label, device = %name, rate, channels, ?format, "opening input");

        let (tx, rx) = sync_channel::<Vec<f32>>(QUEUE_BUFFERS);
        let stop = Arc::new(AtomicBool::new(false));
        let peak_db = Arc::new(AtomicU32::new(SILENCE_DB.to_bits()));
        let dropped = Arc::new(AtomicU64::new(0));

        let writer = ChunkWriter::new(label, out_dir, chunk_seconds, events)?;
        let downsampler = Downsampler::new(rate)?;
        let thread = {
            let (stop, peak_db, label) = (stop.clone(), peak_db.clone(), label.to_string());
            std::thread::Builder::new()
                .name(format!("writer-{label}"))
                .spawn(move || write_loop(&label, rx, downsampler, writer, &stop, &peak_db))?
        };

        let stream = match format {
            SampleFormat::F32 => build::<f32>(&device, &config, channels, tx, dropped.clone()),
            SampleFormat::I16 => build::<i16>(&device, &config, channels, tx, dropped.clone()),
            SampleFormat::I32 => build::<i32>(&device, &config, channels, tx, dropped.clone()),
            other => bail!("{name}: unsupported sample format {other}"),
        }
        .with_context(|| format!("opening stream on {name}"))?;

        stream.play().with_context(|| format!("starting stream on {name}"))?;

        Ok(Self { label: label.to_string(), stream, stop, peak_db, dropped, thread })
    }

    /// Loudest 100 ms window since the last call, in dB. Resets to silence after reading.
    pub fn take_peak_db(&self) -> f32 {
        f32::from_bits(self.peak_db.swap(SILENCE_DB.to_bits(), Ordering::Relaxed))
    }

    pub fn label(&self) -> &str {
        &self.label
    }

    /// Stop the device, write out what is left, and close the last chunk. Returns chunk count.
    pub fn stop(self) -> Result<u32> {
        drop(self.stream);
        self.stop.store(true, Ordering::Relaxed);

        let chunks = self
            .thread
            .join()
            .map_err(|_| anyhow!("{} writer thread panicked", self.label))??;

        let dropped = self.dropped.load(Ordering::Relaxed);
        if dropped > 0 {
            warn!(source = %self.label, dropped, "audio buffers dropped because the writer was slow");
        }

        Ok(chunks)
    }
}

/// Build the device stream. The callback only mixes to mono and hands off; it never blocks.
fn build<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    channels: usize,
    tx: SyncSender<Vec<f32>>,
    dropped: Arc<AtomicU64>,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let stream = device.build_input_stream::<T, _, _>(
        *config,
        move |data: &[T], _| {
            let mono: Vec<f32> = data
                .chunks_exact(channels)
                .map(|frame| frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / channels as f32)
                .collect();

            if let Err(TrySendError::Full(_)) = tx.try_send(mono) {
                dropped.fetch_add(1, Ordering::Relaxed);
            }
        },
        |err| warn!("audio stream error: {err}"),
        None,
    )?;

    Ok(stream)
}

/// Writer thread: resample to 16 kHz, update the level, write chunks. Ends after stop is set.
fn write_loop(
    label: &str,
    rx: Receiver<Vec<f32>>,
    mut downsampler: Downsampler,
    mut writer: ChunkWriter,
    stop: &AtomicBool,
    peak_db: &AtomicU32,
) -> Result<u32> {
    let mut meter = LevelMeter::new();

    loop {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(buffer) => {
                let samples = downsampler.push(&buffer)?;
                meter.feed(&samples, |db| raise_peak(peak_db, db));
                writer.write(&samples)?;
            }
            Err(RecvTimeoutError::Timeout) if stop.load(Ordering::Relaxed) => break,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    let chunks = writer.finish()?;
    info!(source = label, chunks, "recording closed");

    Ok(chunks)
}

/// Keep the larger of the stored peak and `db`.
fn raise_peak(peak: &AtomicU32, db: f32) {
    let _ = peak.try_update(Ordering::Relaxed, Ordering::Relaxed, |bits| {
        (db > f32::from_bits(bits)).then_some(db.to_bits())
    });
}
