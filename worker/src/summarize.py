"""Turn a meeting transcript into a title, a structured summary and my to-dos, using OpenAI."""

import json
import os

from openai import OpenAI
from pydantic import BaseModel, Field

INSTRUCTIONS = """You turn a meeting transcript into clear notes for one person, called "Me".

About the transcript:
- It was recorded on Me's Mac. "Me" is the owner. "Person 1", "Person 2"... are others.
- People often spoke Hindi or Hinglish. Everything was machine-translated to English,
  so some words may be wrong. Use the meaning, not the exact words.
- Lines are in time order. Times are seconds from the start.

Rules:
- Use simple, plain English. Short sentences.
- Only write what the transcript supports. Never invent names, dates or numbers.
- Leave a list empty if there is nothing for it.
- To-dos are only tasks for Me: things Me agreed to do, or was asked to do.
  Tasks for other people go in key points, not in to-dos.
- For each to-do, quote the transcript line it came from, so Me can check it."""


class Todo(BaseModel):

    """One task for Me pulled from the meeting."""

    task: str = Field(description="What Me has to do, as a short action.")
    due: str | None = Field(description="Deadline exactly as said in the meeting, or null.")
    quote: str = Field(description="The transcript line this task came from.")


class MeetingNotes(BaseModel):

    """Structured notes for one meeting."""

    title: str = Field(description="Short meeting title, at most 8 words.")
    topics: list[str] = Field(description="Main subjects discussed, a few words each.")
    summary: str = Field(description="What happened in the meeting, in 2 to 5 sentences.")
    key_points: list[str] = Field(description="Important facts, updates and decisions.")
    todos: list[Todo] = Field(description="Tasks for Me only.")


def transcript_text(lines: list[dict]) -> str:

    """Format transcript lines as '[mm:ss] Speaker: text', one per line."""

    return "\n".join(
        f"[{int(line['start']) // 60:02}:{int(line['start']) % 60:02}] {line['speaker']}: {line['text']}"
        for line in lines
    )


def summarize(
    model: str,
    meeting: dict,
    lines: list[dict]
    ) -> dict:

    """Ask OpenAI for structured notes on one meeting and return them as a plain dict."""

    if not os.environ.get("OPENAI_API_KEY"):
        raise RuntimeError("OPENAI_API_KEY is not set. Add it to the .env file in the repo root.")

    client = OpenAI()
    header = json.dumps({"started_at": meeting.get("started_at"), "ended_at": meeting.get("ended_at")})

    response = client.responses.parse(
        model=model,
        instructions=INSTRUCTIONS,
        input=f"Meeting: {header}\n\nTranscript:\n{transcript_text(lines)}",
        text_format=MeetingNotes,
        )

    if response.output_parsed is None:
        raise RuntimeError("the model returned no notes")

    return response.output_parsed.model_dump()
