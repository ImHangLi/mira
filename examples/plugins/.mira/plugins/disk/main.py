"""Replaces the Volumes table with the free space of each mounted volume (df -k)."""
import json, subprocess, sys

sys.stdin.readline()  # the invocation; this action has no input


def out(frame):
    print(json.dumps({"api": 1, **frame}), flush=True)


rows = []
lines = subprocess.run(["df", "-k"], capture_output=True, text=True, check=True).stdout.splitlines()[1:]
for line in lines:
    parts = line.split()
    if len(parts) < 9 or not parts[0].startswith("/dev/"):
        continue
    size, used, free = (int(parts[i]) for i in (1, 2, 3))
    mount = " ".join(parts[8:])
    # Skip macOS system snapshots and helper volumes; keep the data volume and user volumes.
    if mount.startswith("/System/Volumes/") and mount != "/System/Volumes/Data":
        continue
    rows.append({"id": mount, "values": {
        "mount": mount,
        "size_gb": round(size / 1048576, 1),
        "free_gb": round(free / 1048576, 1),
        "used_pct": round(100 * used / max(size, 1)),
    }})
out({"type": "view", "view_id": "volumes", "op": "replace", "data": {"kind": "table", "columns": [
    {"id": "mount", "label": "Volume", "type": "text"},
    {"id": "size_gb", "label": "Size (GB)", "type": "number"},
    {"id": "free_gb", "label": "Free (GB)", "type": "number"},
    {"id": "used_pct", "label": "Used %", "type": "number"},
], "rows": rows}})
out({"type": "result", "ok": True, "summary": f"{len(rows)} volumes", "data": None})
