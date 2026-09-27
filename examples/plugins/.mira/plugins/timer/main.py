"""Counts down in the Timer view, then shows a macOS notification."""
import json, subprocess, sys, time

inv = json.loads(sys.stdin.readline())
secs = int(inv["input"].get("seconds", 300))
label = inv["input"].get("label", "Break")


def out(frame):
    print(json.dumps({"api": 1, **frame}), flush=True)


def show(text):
    out({"type": "view", "view_id": "clock", "op": "replace", "data": {"kind": "text", "format": "plain", "text": text}})


for left in range(secs, 0, -1):
    show(f"{label}: {left // 60}:{left % 60:02d} left")
    out({"type": "progress", "message": f"{left}s left", "current": secs - left, "total": secs})
    time.sleep(1)
show(f"{label}: done")
# The label cannot contain quotes or backslashes (see the input schema), so it is safe in the script.
subprocess.run(["osascript", "-e", f'display notification "{label} is over" with title "Mira timer"'], check=False)
out({"type": "result", "ok": True, "summary": f"{label}: {secs}s done", "data": None})
