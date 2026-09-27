"""A monotonic countdown with a daily focus count and MPP/1 views."""

import fcntl
import json
import math
import os
import signal
import sys
import time
from datetime import date
from pathlib import Path

DIGITS = {
    "0": ("███", "█ █", "█ █", "█ █", "███"),
    "1": (" █ ", "██ ", " █ ", " █ ", "███"),
    "2": ("███", "  █", "███", "█  ", "███"),
    "3": ("███", "  █", "███", "  █", "███"),
    "4": ("█ █", "█ █", "███", "  █", "  █"),
    "5": ("███", "█  ", "███", "  █", "███"),
    "6": ("███", "█  ", "███", "█ █", "███"),
    "7": ("███", "  █", "  █", "  █", "  █"),
    "8": ("███", "█ █", "███", "█ █", "███"),
    "9": ("███", "█ █", "███", "  █", "███"),
    ":": (" ", "█", " ", "█", " "),
}
stopped = False


def emit(frame):
    print(json.dumps({"api": 1, **frame}, ensure_ascii=False, allow_nan=False), flush=True)


def daily_count(increment=False):
    path = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "mira/pomodoro.json"
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("a+", encoding="utf-8") as stream:
        fcntl.flock(stream, fcntl.LOCK_EX)
        stream.seek(0)
        try:
            state = json.load(stream)
            count = max(0, int(state["count"])) if state["date"] == date.today().isoformat() else 0
        except (ValueError, KeyError, TypeError):
            count = 0
        if increment:
            count += 1
            stream.seek(0)
            stream.truncate()
            json.dump({"date": date.today().isoformat(), "count": count}, stream)
            stream.flush()
        return count


def show(label, remaining, total, count, note):
    clock = f"{remaining // 60:02}:{remaining % 60:02}"
    big = "\n".join("  ".join(DIGITS[char][row] for char in clock) for row in range(5))
    done = max(0, min(24, int(24 * (total - remaining) / total)))
    text = (f"{label}\n\n{big}\n\n{clock} remaining\n"
            f"[{'━' * done}{'─' * (24 - done)}]\n\n"
            f"{count} {'pomodoro' if count == 1 else 'pomodoros'} today.\n{note}")
    emit({"type": "view", "view_id": "clock", "op": "replace",
          "data": {"kind": "text", "format": "plain", "text": text}})
    emit({"type": "progress", "message": f"{label}: {clock}",
          "current": max(0, total - remaining), "total": total})


def stop(signum, frame):
    global stopped
    stopped = True


def main():
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    try:
        req = json.loads(sys.stdin.readline())
        is_break = req["action"] == "break"
        total = float(req["input"].get("minutes", 5 if is_break else 25)) * 60
        if not math.isfinite(total) or not 0 < total <= 14400:
            raise ValueError("Minutes must be greater than zero and at most 240.")
        label = req["input"].get("label", "Break" if is_break else "Focus")
        count = daily_count()
        deadline = time.monotonic() + total
        previous = None
        while not stopped:
            remaining = max(0, math.ceil(deadline - time.monotonic()))
            if remaining == 0:
                break
            if remaining != previous:
                show(label, remaining, total, count, "Press s on the running action to stop.")
                previous = remaining
            time.sleep(min(0.1, max(0, deadline - time.monotonic())))
        if stopped:
            show(label, max(0, math.ceil(deadline - time.monotonic())), total, count,
                 "Stopped. This session does not count.")
            emit({"type": "result", "ok": False, "summary": "Session stopped.", "data": None,
                  "error": {"code": "CANCELLED", "message": "This session does not count.", "retryable": False}})
            return 130
        count = daily_count(increment=not is_break)
        note = ("Break complete. Your keyboard missed you." if is_break else
                "Step away. Your agent can survive 5 minutes without you.")
        show(label, 0, total, count, note)
        emit({"type": "notify", "title": "Mira Pomodoro", "message": f"{label} is complete. {note}"})
        noun = "pomodoro" if count == 1 else "pomodoros"
        emit({"type": "result", "ok": True, "summary": f"{label} complete. {count} {noun} today.",
              "data": {"pomodoros_today": count}})
        return 0
    except (OSError, ValueError, KeyError, TypeError) as exc:
        emit({"type": "result", "ok": False, "summary": "Could not finish the session.", "data": None,
              "error": {"code": "SESSION_FAILED", "message": str(exc), "retryable": False}})
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
