"""Speech to English text with faster-whisper, translating any Hindi or Hinglish speech."""

import re

import numpy as np
from faster_whisper import WhisperModel

NO_SPEECH_LIMIT = 0.6
LOW_CONFIDENCE_LOGPROB = -1.0
REPEAT_LIMIT = 2.4

MADE_UP_PHRASES = {
    "thanks for watching",
    "thank you for watching",
    "thank you so much for watching",
    "thanks for watching and see you next time",
    "please subscribe",
    "please like and subscribe",
    "like and subscribe",
    "dont forget to like and subscribe",
    "subscribe to my channel",
    "see you in the next video",
    "see you next time",
    "subtitles by the amaraorg community",
    "transcribed by",
}


def load_model(model_path: str) -> WhisperModel:

    """Load the local faster-whisper model once on CPU with int8 weights."""

    return WhisperModel(model_path, device="cpu", compute_type="int8")


def normalise(text: str) -> str:

    """Lowercase and keep only letters, digits and single spaces, for phrase matching."""

    return " ".join(re.sub(r"[^a-z0-9 ]", "", text.lower()).split())


def is_made_up(segment) -> bool:

    """True if Whisper likely invented this segment on silence or noise instead of hearing speech."""

    if normalise(segment.text) in MADE_UP_PHRASES:
        return True

    if segment.no_speech_prob > NO_SPEECH_LIMIT and segment.avg_logprob < LOW_CONFIDENCE_LOGPROB:
        return True

    return segment.compression_ratio > REPEAT_LIMIT


def transcribe(
    model: WhisperModel,
    audio: np.ndarray
    ) -> list[dict]:

    """Turn one 16 kHz mono audio chunk into English segments with word timings, dropping made-up ones."""

    segments, _ = model.transcribe(
        audio,
        task="translate",
        vad_filter=True,
        word_timestamps=True,
        condition_on_previous_text=False,
        )

    return [
        {
            "start": segment.start,
            "end": segment.end,
            "text": segment.text.strip(),
            "words": [
                {"start": word.start, "end": word.end, "word": word.word.strip()}
                for word in segment.words or []
            ],
        }
        for segment in segments
        if not is_made_up(segment)
    ]
