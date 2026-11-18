"""JSON-lines messages between the Rust daemon and this worker, one object per line."""

import json
import os
import sys
from typing import Iterator, TextIO


def claim_stdout() -> TextIO:

    """Keep the real stdout for protocol messages and send everything else that prints to stderr."""

    protocol_out = os.fdopen(os.dup(sys.stdout.fileno()), "w", buffering=1)
    os.dup2(sys.stderr.fileno(), sys.stdout.fileno())
    sys.stdout = sys.stderr

    return protocol_out


def send(
    out: TextIO,
    message: dict
    ) -> None:

    """Write one message as a single JSON line and flush it right away."""

    out.write(json.dumps(message, ensure_ascii=False) + "\n")
    out.flush()


def requests() -> Iterator[dict]:

    """Yield each request from stdin until the daemon closes it, skipping blank or broken lines."""

    for line in sys.stdin:

        line = line.strip()

        if not line:
            continue

        try:
            yield json.loads(line)

        except json.JSONDecodeError as exc:

            print(f"worker: bad request line: {exc}", file=sys.stderr)
