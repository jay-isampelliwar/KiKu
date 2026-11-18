"""Who spoke when, using the local pyannote community-1 pipeline on the meeting's system audio."""

import numpy as np
import torch
from pyannote.audio import Pipeline

SAMPLE_RATE = 16000


def load_pipeline(model_path: str) -> Pipeline:

    """Load the local pyannote pipeline once, on the Mac GPU when it is available."""

    pipeline = Pipeline.from_pretrained(model_path)
    device = "mps" if torch.backends.mps.is_available() else "cpu"
    pipeline.to(torch.device(device))

    return pipeline


def join_chunks(
    chunks: list[tuple[float, np.ndarray]]
    ) -> np.ndarray:

    """Place each chunk at its start time on one timeline, leaving silence where a chunk is missing."""

    total = max(int(start * SAMPLE_RATE) + len(audio) for start, audio in chunks)
    timeline = np.zeros(total, dtype=np.float32)

    for start, audio in chunks:

        offset = int(start * SAMPLE_RATE)
        timeline[offset:offset + len(audio)] = audio

    return timeline


def diarize(
    pipeline: Pipeline,
    audio: np.ndarray
    ) -> list[dict]:

    """Return speaker turns for the whole audio, with no two turns overlapping."""

    waveform = torch.from_numpy(audio).unsqueeze(0)
    output = pipeline({"waveform": waveform, "sample_rate": SAMPLE_RATE})
    annotation = getattr(output, "exclusive_speaker_diarization", None) or getattr(output, "speaker_diarization", output)

    return [
        {"start": round(turn.start, 3), "end": round(turn.end, 3), "speaker": speaker}
        for turn, _, speaker in annotation.itertracks(yield_label=True)
    ]
