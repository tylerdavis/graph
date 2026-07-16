#!/usr/bin/env python3
"""Render a trace timeline from the workbench log.

The workbench writes a timestamped tracing stream to
`<data_dir>/workbench.log` (default ~/.local/share/graph/workbench.log).
This parses it into nested spans — session > turn > tool call, plus the
two sibling phases of an incremental draft: a one-shot outline that sketches
stages, then a separate step-by-step drafting loop that turns each stage into
a validated step (up to 3 attempts each) — and prints an indented timeline
with durations, error markers, and (truncated) tool inputs.

Usage:
    workbench_trace.py [--sessions N] [--log PATH] [--no-color] [--width N]

    --sessions N   Show the last N sessions (default 3).
    --log PATH     Log file (default ~/.local/share/graph/workbench.log).
    --width N      Truncate tool inputs/results to N chars (default 70; 0 = off).
    --no-color     Disable ANSI color.
"""
import argparse
import os
import re
import sys
from datetime import datetime

TS = re.compile(r"^(\d{4}-\d{2}-\d{2}T[\d:.]+Z)\s+(\w+)\s+workbench:\s+(.*)$")

# One matcher per event kind we care about. Each returns a dict of fields.
SESSION = re.compile(r"── session started \(graph ([\d.]+)\) ──")
CONTEXT = re.compile(r"context loaded: (\d+) tools, (\d+) shapes")
TURN_START = re.compile(r"agent turn started \((\d+) chars\)")
TURN_TOOK = re.compile(r"agent turn took ([\d.]+)s \((\d+) messages")
TURN_FAIL = re.compile(r"agent turn failed: (.+)")
INVOKED = re.compile(r"agent invoked (\S+): (.*)", re.S)
TOOL_FIN = re.compile(r"(\S+) finished in ([\d.]+)s \(is_error=(true|false)\)(?::\s*(.*))?", re.S)
DRAFT_OUTLINE = re.compile(r"draft outline: (\d+) stages")
DRAFT_STEP_START = re.compile(r"draft step (\d+) started: (.*)", re.S)
DRAFT_STEP_FIN = re.compile(r"draft step (\d+) finished \(attempt (\d+), (\d+) problem")
DRAFT_REPLACED = re.compile(r"draft replaced: '([^']*)' \((\d+) steps")
MODE = re.compile(r"mode: (\S+) → (\S+)")

C = {
    "dim": "\033[2m", "reset": "\033[0m", "bold": "\033[1m",
    "red": "\033[31m", "green": "\033[32m", "yellow": "\033[33m",
    "blue": "\033[34m", "cyan": "\033[36m", "mag": "\033[35m",
}


def parse_ts(s):
    # 2026-07-16T01:46:45.185516Z -> datetime
    return datetime.fromisoformat(s.replace("Z", "+00:00"))


def load_events(path):
    events = []
    with open(path, encoding="utf-8", errors="replace") as fh:
        for line in fh:
            m = TS.match(line.rstrip("\n"))
            if not m:
                # continuation of a multiline JSON payload — attach to prev
                if events:
                    events[-1]["raw"] += "\n" + line.rstrip("\n")
                continue
            ts, level, body = m.groups()
            events.append({"ts": parse_ts(ts), "level": level, "body": body, "raw": body})
    return events


def split_sessions(events):
    sessions, current = [], None
    for ev in events:
        if SESSION.search(ev["body"]):
            current = [ev]
            sessions.append(current)
        elif current is not None:
            current.append(ev)
    return sessions


def clip(s, width):
    s = " ".join(s.split())  # collapse whitespace/newlines
    if width and len(s) > width:
        return s[:width] + "…"
    return s


def fmt_dur(sec):
    sec = float(sec)
    if sec >= 60:
        return f"{int(sec // 60)}m{sec % 60:04.1f}s"
    return f"{sec:.1f}s"


def render(session, width, color):
    def c(name, text):
        return f"{C[name]}{text}{C['reset']}" if color else text

    out = []
    start = session[0]["ts"]

    def clk(ev_or_ts):
        # Left column is the wall-clock time the span STARTED.
        ts = ev_or_ts["ts"] if isinstance(ev_or_ts, dict) else ev_or_ts
        return ts.strftime("%H:%M:%S")

    ver = SESSION.search(session[0]["body"]).group(1)
    out.append(c("bold", f"┏━ session {clk(start)}  ·  graph {ver}"))

    pending_invoke = None  # (tool, input, ts) awaiting its finished line
    in_turn = False
    turn_start_ts = None  # start of the current turn (outline duration baseline)
    drafting_started = False  # emit the drafting-phase header once per draft
    step_open = None  # (start_ts, summary) of the draft step awaiting its finish

    for ev in session[1:]:
        b = ev["body"]
        t = clk(ev)

        m = CONTEXT.search(b)
        if m:
            out.append(f"┃ {c('dim', t)}  {c('dim', f'context: {m.group(1)} tools, {m.group(2)} shapes')}")
            continue

        m = TURN_START.search(b)
        if m:
            in_turn = True
            turn_start_ts = ev["ts"]
            out.append(f"┃ {c('dim', t)}  {c('cyan', '▸ turn')} {c('dim', f'({m.group(1)} chars in)')}")
            continue

        m = TURN_TOOK.search(b)
        if m:
            out.append(f"┃ {c('dim', t)}  {c('cyan', '◂ turn done')} {c('dim', fmt_dur(m.group(1)) + f', {m.group(2)} msgs')}")
            in_turn = False
            continue

        m = TURN_FAIL.search(b)
        if m:
            out.append(f"┃ {c('dim', t)}  {c('red', '✗ turn failed:')} {c('red', clip(m.group(1), width * 2 or 999))}")
            in_turn = False
            continue

        m = INVOKED.search(b)
        if m:
            pending_invoke = (m.group(1), m.group(2), ev["ts"])
            continue

        m = TOOL_FIN.search(b)
        if m:
            tool, dur, is_err, result = m.groups()
            err = is_err == "true"
            glyph = c("red", "✗") if err else c("green", "✓")
            name = c("mag", tool.replace("workbench__", ""))
            inp = ""
            # Left column is the invoked (start) clock when we saw it.
            start_t = t
            if pending_invoke and pending_invoke[0] == tool:
                start_t = clk(pending_invoke[2])
                if width:
                    raw_in = pending_invoke[1]
                    if raw_in.strip() not in ("{}", ""):
                        inp = c("dim", "  ← " + clip(raw_in, width))
            line = f"┃ {c('dim', start_t)}  {glyph} {name} {c('dim', fmt_dur(dur))}{inp}"
            out.append(line)
            if err and result and width:
                out.append(f"┃         {c('red', '  ' + clip(result, width * 2 or 999))}")
            pending_invoke = None
            continue

        m = DRAFT_OUTLINE.search(b)
        if m:
            # A new outline begins a fresh draft; the drafting phase (a
            # separate, sibling phase) hasn't started yet. The outline is a
            # one-shot call — its cost is the gap since the turn started.
            drafting_started = False
            out_t = clk(turn_start_ts) if turn_start_ts else t
            dur = ""
            if turn_start_ts:
                dur = c("dim", fmt_dur((ev["ts"] - turn_start_ts).total_seconds()))
            out.append(f"┃ {c('dim', out_t)}    {c('yellow', f'✎ draft outline — {m.group(1)} stages')}  {dur}")
            continue

        m = DRAFT_STEP_START.search(b)
        if m:
            if not drafting_started:
                # First drafting step: open the drafting phase as its own
                # sibling of the outline (same indent), not nested under it.
                out.append(f"┃ {c('dim', t)}    {c('blue', '✍ drafting plan — step-by-step')}")
                drafting_started = True
            # Fold start+finish into one row: stash the start ts and summary,
            # emit on the matching finish with a real duration.
            step_open = (ev["ts"], m.group(2))
            continue

        m = DRAFT_STEP_FIN.search(b)
        if m:
            step, attempt, probs = m.groups()
            ok = probs == "0"
            glyph = c("green", "✓") if ok else c("yellow", "↻")
            start_ts, summary = step_open or (ev["ts"], "")
            step_t = clk(start_ts)
            dur = c("dim", fmt_dur((ev["ts"] - start_ts).total_seconds()))
            if ok:
                label = f"{c('yellow', f'step {step}')} {c('dim', clip(summary, width))}"
                out.append(f"┃ {c('dim', step_t)}      {glyph} {label}  {dur}")
                step_open = None
            else:
                # A rejected attempt; the step keeps drafting. Show it, then
                # rebase the start clock so the retry's duration counts from
                # this finish (the next attempt began here).
                note = f"attempt {attempt}: {probs} problem(s)"
                label = f"{c('yellow', f'step {step}')} {c('dim', clip(summary, width))}"
                out.append(f"┃ {c('dim', step_t)}      {glyph} {label} {c('dim', note)}  {dur}")
                step_open = (ev["ts"], summary)
            continue

        m = DRAFT_REPLACED.search(b)
        if m:
            out.append(f"┃ {c('dim', t)}    {c('blue', f'⟳ draft → {m.group(1)} ({m.group(2)} steps)')}")
            continue

    total = (session[-1]["ts"] - start).total_seconds()
    tools = sum(1 for ev in session if TOOL_FIN.search(ev["body"]))
    errs = sum(1 for ev in session if (mm := TOOL_FIN.search(ev["body"])) and mm.group(3) == "true")
    summary = f"{fmt_dur(total)}  ·  {tools} tool calls"
    if errs:
        summary += c("red", f"  ·  {errs} errors")
    out.append(c("bold", f"┗━ {summary}"))
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sessions", type=int, default=3)
    ap.add_argument("--log", default=os.path.expanduser("~/.local/share/graph/workbench.log"))
    ap.add_argument("--width", type=int, default=70)
    ap.add_argument("--no-color", action="store_true")
    args = ap.parse_args()

    color = not args.no_color and sys.stdout.isatty()
    if not os.path.exists(args.log):
        sys.exit(f"log not found: {args.log}")

    sessions = split_sessions(load_events(args.log))
    if not sessions:
        sys.exit("no sessions found in log")

    for session in sessions[-args.sessions:]:
        print(render(session, args.width, color))
        print()


if __name__ == "__main__":
    main()
