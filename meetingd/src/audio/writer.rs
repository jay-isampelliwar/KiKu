use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;

use anyhow::{Context, Result};
use audioadapter_buffers::direct::InterleavedSlice;
use hound::{SampleFormat, WavSpec, WavWriter};
use rubato::{Fft, FixedSync, Resampler};

use super::TARGET_RATE;

/// Turns mono audio at the device rate into 16 kHz samples.
pub struct Downsampler {
    inner: Option<Fft<f32>>,
    pending: Vec<f32>,
    out: Vec<f32>,
}

impl Downsampler {
    pub fn new(input_rate: u32) -> Result<Self> {
        let inner = if input_rate == TARGET_RATE {
            None
        } else {
            Some(
                Fft::<f32>::new(
                    input_rate as usize,
                    TARGET_RATE as usize,
                    1024,
                    1,
                    FixedSync::Input,
                )
                .context("creating resampler")?,
            )
        };

        Ok(Self { inner, pending: Vec::new(), out: Vec::new() })
    }

    /// Add new samples and return all 16 kHz samples that are ready so far.
    pub fn push(&mut self, input: &[f32]) -> Result<Vec<f32>> {
        let Some(resampler) = self.inner.as_mut() else {
            return Ok(input.to_vec());
        };

        self.pending.extend_from_slice(input);

        let mut ready = Vec::new();
        let mut used = 0;

        loop {
            let need = resampler.input_frames_next();
            if self.pending.len() - used < need {
                break;
            }

            let input = InterleavedSlice::new(&self.pending[used..used + need], 1, need)?;

            self.out.resize(resampler.output_frames_max(), 0.0);
            let out_len = self.out.len();
            let mut output = InterleavedSlice::new_mut(&mut self.out, 1, out_len)?;

            let (consumed, produced) = resampler.process_into_buffer(&input, &mut output, None)?;
            ready.extend_from_slice(&self.out[..produced]);
            used += consumed;
        }

        self.pending.drain(..used);

        Ok(ready)
    }
}

/// Sent each time a chunk file is complete on disk.
#[derive(Debug, Clone)]
pub struct ClosedChunk {
    pub source: String,
    pub index: u32,
    pub path: PathBuf,
}

/// Writes 16 kHz mono samples into numbered WAV files, starting a new file every chunk.
pub struct ChunkWriter {
    source: String,
    events: Sender<ClosedChunk>,
    dir: PathBuf,
    chunk_samples: u64,
    index: u32,
    written: u64,
    current: Option<WavWriter<BufWriter<File>>>,
}

impl ChunkWriter {
    pub fn new(
        source: &str,
        dir: &Path,
        chunk_seconds: u32,
        events: Sender<ClosedChunk>,
    ) -> Result<Self> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating chunk folder {}", dir.display()))?;

        Ok(Self {
            source: source.to_string(),
            events,
            dir: dir.to_path_buf(),
            chunk_samples: u64::from(chunk_seconds) * u64::from(TARGET_RATE),
            index: 0,
            written: 0,
            current: None,
        })
    }

    /// Write samples, closing the current file and opening the next one at each chunk boundary.
    pub fn write(&mut self, samples: &[f32]) -> Result<()> {
        for &sample in samples {
            if self.current.is_none() {
                self.open_next()?;
            }

            let value = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)) as i16;
            self.current.as_mut().expect("file is open").write_sample(value)?;
            self.written += 1;

            if self.written == self.chunk_samples {
                self.close_current()?;
            }
        }

        Ok(())
    }

    /// Close the last file. Returns how many chunk files were made.
    pub fn finish(mut self) -> Result<u32> {
        self.close_current()?;
        Ok(self.index)
    }

    fn open_next(&mut self) -> Result<()> {
        self.index += 1;
        let path = self.chunk_path(self.index);

        let spec = WavSpec {
            channels: 1,
            sample_rate: TARGET_RATE,
            bits_per_sample: 16,
            sample_format: SampleFormat::Int,
        };

        self.current = Some(
            WavWriter::create(&path, spec)
                .with_context(|| format!("creating {}", path.display()))?,
        );
        self.written = 0;

        Ok(())
    }

    fn chunk_path(&self, index: u32) -> PathBuf {
        self.dir.join(format!("chunk-{index:04}.wav"))
    }

    fn close_current(&mut self) -> Result<()> {
        if let Some(file) = self.current.take() {
            file.finalize()?;
            tracing::debug!(source = %self.source, chunk = self.index, "chunk closed");
            let _ = self.events.send(ClosedChunk {
                source: self.source.clone(),
                index: self.index,
                path: self.chunk_path(self.index),
            });
        }

        Ok(())
    }
}

/// Measures loudness in 100 ms windows of 16 kHz audio.
pub struct LevelMeter {
    sum_squares: f64,
    count: usize,
}

const WINDOW_SAMPLES: usize = TARGET_RATE as usize / 10;

impl LevelMeter {
    pub fn new() -> Self {
        Self { sum_squares: 0.0, count: 0 }
    }

    /// Feed samples. Calls `on_window` with the level in dB each time a 100 ms window is full.
    pub fn feed(&mut self, samples: &[f32], mut on_window: impl FnMut(f32)) {
        for &sample in samples {
            self.sum_squares += f64::from(sample) * f64::from(sample);
            self.count += 1;

            if self.count == WINDOW_SAMPLES {
                let rms = (self.sum_squares / self.count as f64).sqrt();
                on_window(to_db(rms));
                self.sum_squares = 0.0;
                self.count = 0;
            }
        }
    }
}

/// RMS to decibels, with silence floored at -100 dB.
fn to_db(rms: f64) -> f32 {
    (20.0 * rms.max(1e-5).log10()) as f32
}
