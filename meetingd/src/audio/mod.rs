mod device;
mod source;
mod writer;

pub use device::list_inputs;
pub use source::Source;
pub use writer::ClosedChunk;

/// Sample rate Whisper and pyannote expect. Every chunk is written at this rate, mono.
pub const TARGET_RATE: u32 = 16_000;
