"""Worker entry point: load Whisper once, then answer transcribe, diarize and summarize requests."""

import argparse
import os
import sys
import time
import wave
import warnings

os.environ.setdefault("HF_HUB_OFFLINE", "1")
warnings.filterwarnings("ignore")

import numpy as np

from protocol import claim_stdout, requests, send
from transcribe import load_model, transcribe


def read_wav(path: str) -> np.ndarray:

    """Read a 16 kHz mono 16-bit WAV chunk into float32 samples between -1 and 1."""

    with wave.open(path) as file:
        frames = file.readframes(file.getnframes())

    return np.frombuffer(frames, dtype=np.int16).astype("float32") / 32768


class Worker:

    """Holds the loaded models. Whisper loads at start, pyannote on the first diarize request."""

    def __init__(
        self,
        whisper_path: str,
        pyannote_path: str
        ) -> None:

        """Load Whisper now and remember where pyannote lives."""

        self.whisper = load_model(whisper_path)
        self.pyannote_path = pyannote_path
        self.pipeline = None

    def transcribe(self, request: dict) -> dict:

        """Turn one chunk file into English segments with word timings."""

        return {"type": "result", "segments": transcribe(self.whisper, read_wav(request["path"]))}

    def diarize(self, request: dict) -> dict:

        """Join the meeting's system chunks on one timeline and find who spoke when."""

        from diarize import diarize, join_chunks, load_pipeline

        if self.pipeline is None:
            self.pipeline = load_pipeline(self.pyannote_path)

        chunks = [(chunk["start"], read_wav(chunk["path"])) for chunk in request["chunks"]]

        return {"type": "turns", "turns": diarize(self.pipeline, join_chunks(chunks))}

    def summarize(self, request: dict) -> dict:

        """Ask the LLM for a title, summary and to-dos from the meeting's transcript lines."""

        from summarize import summarize

        return {"type": "summary", "summary": summarize(request["model"], request["meeting"], request["lines"])}

    def handle(self, request: dict) -> dict:

        """Run one request and add its id and timing, or turn any failure into an error reply."""

        request_id = request.get("request_id")
        kind = request.get("type", "transcribe")
        started = time.time()

        try:

            if kind == "transcribe":
                reply = self.transcribe(request)

            elif kind == "diarize":
                reply = self.diarize(request)

            elif kind == "summarize":
                reply = self.summarize(request)

            else:
                raise ValueError(f"unknown request type {kind!r}")

        except Exception as exc:

            return {"type": "error", "request_id": request_id, "error": f"{type(exc).__name__}: {exc}"}

        reply["request_id"] = request_id
        reply["seconds"] = round(time.time() - started, 2)

        return reply


def main() -> None:

    """Load Whisper, say ready, then answer requests until stdin closes."""

    parser = argparse.ArgumentParser()
    parser.add_argument("--whisper-model", required=True)
    parser.add_argument("--pyannote-model", required=True)
    args = parser.parse_args()

    out = claim_stdout()

    started = time.time()
    worker = Worker(args.whisper_model, args.pyannote_model)
    send(out, {"type": "ready", "load_seconds": round(time.time() - started, 2)})

    for request in requests():
        send(out, worker.handle(request))

    print("worker: stdin closed, exiting", file=sys.stderr)


if __name__ == "__main__":
    main()
