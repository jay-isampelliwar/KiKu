"""Speech to English text with faster-whisper, translating any Hindi or Hinglish speech."""

import numpy as np
from faster_whisper import WhisperModel


def load_model(model_path: str) -> WhisperModel:

    """Load the local faster-whisper model once on CPU with int8 weights."""

    return WhisperModel(model_path, device="cpu", compute_type="int8")


def transcribe(
    model: WhisperModel,
    audio: np.ndarray
    ) -> list[dict]:

    """Turn one 16 kHz mono audio chunk into English text segments with start and end times."""

    segments, _ = model.transcribe(
        audio,
        task="translate",
        vad_filter=True,
        )

    return [
        {"start": segment.start, "end": segment.end, "text": segment.text.strip()}
        for segment in segments
    ]
