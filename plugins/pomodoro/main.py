"""A one-screen Pomodoro timer: focus or break, a length to pick, and a daily count."""

import curses
import fcntl
import json
import os
import subprocess
import time
from datetime import date
from pathlib import Path

MODES = ("focus", "break")
PRESETS = {"focus": [15, 25, 50], "break": [5, 10, 15]}
DEFAULT = {"focus": 1, "break": 0}  # 25 and 5 minutes
DONE_LINE = {
    "focus": "Step away. Your agent can survive 5 minutes without you.",
    "break": "Break over. Your keyboard missed you.",
}
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


def daily_count(increment=False):
    """Pomodoros finished today, kept per user so every project shares one count."""
    path = Path(os.environ.get("XDG_STATE_HOME") or Path.home() / ".local/state") / "mira/pomodoro.json"
    try:
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
    except OSError:
        return 0


def notify(message):
    """A desktop notification through Mira, so the user sees it in another app too."""
    mira = os.environ.get("MIRA_BIN") or "mira"
    try:
        subprocess.run([mira, "notify", "--title", "Mira Pomodoro", message],
                       stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                       timeout=5, check=False)
    except (OSError, subprocess.TimeoutExpired):
        pass


class Timer:
    def __init__(self):
        self.mode = "focus"
        self.choice = dict(DEFAULT)
        self.minutes = {m: list(PRESETS[m]) for m in MODES}
        self.state = "idle"  # idle, running, paused
        self.left = self.total()
        self.deadline = 0.0
        self.note = "Pick a length, then press Enter."
        self.count = daily_count()

    def total(self):
        return self.minutes[self.mode][self.choice[self.mode]] * 60

    def remaining(self):
        if self.state == "running":
            return max(0.0, self.deadline - time.monotonic())
        return self.left

    def start(self):
        self.deadline = time.monotonic() + self.left
        self.state = "running"
        self.note = "Focusing." if self.mode == "focus" else "On a break."

    def pause(self):
        self.left = self.remaining()
        self.state = "paused"
        self.note = "Paused."

    def reset(self, note="Reset. This one does not count."):
        self.state = "idle"
        self.left = self.total()
        self.note = note

    def switch(self, mode):
        self.mode = mode
        self.reset(note="Pick a length, then press Enter.")

    def pick(self, step):
        n = len(self.minutes[self.mode])
        self.choice[self.mode] = (self.choice[self.mode] + step) % n
        self.left = self.total()

    def adjust(self, step):
        i = self.choice[self.mode]
        self.minutes[self.mode][i] = max(1, min(240, self.minutes[self.mode][i] + step))
        self.left = self.total()

    def tick(self):
        """Ends a finished session: counts a focus, notifies, and offers the other mode."""
        if self.state != "running" or self.remaining() > 0:
            return
        finished = self.mode
        if finished == "focus":
            self.count = daily_count(increment=True)
        notify(DONE_LINE[finished])
        self.switch("break" if finished == "focus" else "focus")
        self.note = DONE_LINE[finished]


def big_clock(text, scale):
    """Rows of block digits, each cell doubled across so the digits look square."""
    rows = []
    for r in range(5):
        row = " ".join(DIGITS[c][r] for c in text)
        wide = "".join(ch * 2 * scale for ch in row)
        rows.extend([wide] * scale)
    return rows


def put(screen, y, x, text, style=0):
    height, width = screen.getmaxyx()
    if 0 <= y < height and 0 <= x < width - 1:
        try:
            screen.addnstr(y, x, text, width - x - 1, style)
        except curses.error:
            pass


def draw(screen, t, colors):
    screen.erase()
    height, width = screen.getmaxyx()
    accent = colors[t.mode]
    left = t.remaining()
    secs = int(left + 0.999) if t.state == "running" else int(left)
    clock = f"{secs // 60:02}:{secs % 60:02}"
    today = f"{t.count} {'pomodoro' if t.count == 1 else 'pomodoros'} today"
    if width < 30 or height < 12:
        put(screen, 0, 0, f"{t.mode.upper()}  {clock}", accent | curses.A_BOLD)
        put(screen, 1, 0, today)
        put(screen, 2, 0, "Enter start  Space pause  r reset")
        screen.refresh()
        return
    put(screen, 0, 2, "POMODORO", curses.A_BOLD)
    put(screen, 0, max(12, width - len(today) - 3), today, curses.A_DIM)
    # Mode switch, like two tabs.
    x = 2
    for m in MODES:
        label = f"  {m.capitalize()}  "
        style = (colors[m] | curses.A_REVERSE | curses.A_BOLD) if m == t.mode else curses.A_DIM
        put(screen, 2, x, label, style)
        x += len(label) + 1
    # From the bottom up: keys, note, lengths, progress. The clock gets the rest.
    keys_y, note_y = height - 1, height - 3
    chips_y = height - 5
    bar_y = chips_y - 2 if height >= 16 else chips_y - 1
    top, bottom = 4, bar_y - 1
    room_h, room_w = bottom - top, width - 4
    scale = 0
    for s in range(6, 0, -1):
        rows = big_clock(clock, s)
        if len(rows) <= room_h and len(rows[0]) <= room_w:
            scale = s
            break
    rows = big_clock(clock, scale) if scale else [clock]
    cx = (width - len(rows[0])) // 2
    cy = top + max(0, (room_h - len(rows)) // 2)
    for i, row in enumerate(rows):
        put(screen, cy + i, cx, row, accent | curses.A_BOLD)
    # Progress across the full width.
    bar_w = width - 4
    done = 0 if t.total() == 0 else int(bar_w * (1 - left / t.total()))
    put(screen, bar_y, 2, "━" * done, accent)
    put(screen, bar_y, 2 + done, "─" * (bar_w - done), curses.A_DIM)
    # Lengths to pick, like buttons.
    if t.state == "idle":
        x = 2
        for i, m in enumerate(t.minutes[t.mode]):
            label = f" {m} min "
            style = (accent | curses.A_REVERSE) if i == t.choice[t.mode] else 0
            put(screen, chips_y, x, label, style)
            x += len(label) + 2
        put(screen, chips_y, x, "←/→ pick   +/- change", curses.A_DIM)
    put(screen, note_y, 2, t.note)
    keys = {
        "idle": "Enter start   Tab focus/break   q quit",
        "running": "Space pause   r reset   q quit",
        "paused": "Space resume   r reset   q quit",
    }[t.state]
    put(screen, keys_y, 2, keys, curses.A_DIM)
    screen.refresh()


def main(screen):
    try:
        curses.curs_set(0)
    except curses.error:
        pass
    screen.keypad(True)
    screen.timeout(200)
    curses.set_escdelay(25)
    colors = {"focus": curses.A_BOLD, "break": curses.A_BOLD}
    if curses.has_colors():
        try:
            curses.start_color()
            curses.use_default_colors()
            curses.init_pair(1, curses.COLOR_RED, -1)
            curses.init_pair(2, curses.COLOR_GREEN, -1)
            colors = {"focus": curses.color_pair(1), "break": curses.color_pair(2)}
        except curses.error:
            pass
    t = Timer()
    while True:
        t.tick()
        draw(screen, t, colors)
        key = screen.getch()
        if key == ord("q"):
            return
        if key in (10, 13, curses.KEY_ENTER) and t.state == "idle":
            t.start()
        elif key == ord(" "):
            if t.state == "running":
                t.pause()
            elif t.state in ("idle", "paused"):
                t.start()
        elif key == ord("r") and t.state != "idle":
            t.reset()
        elif t.state == "idle":
            if key in (9, curses.KEY_BTAB, ord("m")):
                t.switch("break" if t.mode == "focus" else "focus")
            elif key in (curses.KEY_RIGHT, ord("l")):
                t.pick(1)
            elif key in (curses.KEY_LEFT, ord("h")):
                t.pick(-1)
            elif key in (ord("+"), ord("=")):
                t.adjust(1)
            elif key in (ord("-"), ord("_")):
                t.adjust(-1)


if __name__ == "__main__":
    curses.wrapper(main)
