"""List TCP listeners and stop a selected process with SIGTERM."""

import json
import os
import pwd
import signal
import subprocess
import sys
import time

EMPTY = "No one is squatting on a port. Suspicious."


def emit(frame):
    print(json.dumps({"api": 1, **frame}, ensure_ascii=False, allow_nan=False), flush=True)


def command(argv):
    reply = subprocess.run(argv, capture_output=True, text=True, timeout=5)
    if reply.returncode:
        raise RuntimeError(f"{argv[0]} failed: {reply.stderr.strip() or 'no process data returned'}")
    return reply.stdout


def listeners():
    reply = subprocess.run(["lsof", "-nP", "-iTCP", "-sTCP:LISTEN", "-Fpcun"],
                           capture_output=True, text=True, timeout=5)
    # lsof returns 1 with no output when there are no matches.
    if reply.returncode and not (reply.returncode == 1 and not reply.stdout and not reply.stderr):
        raise RuntimeError(f"lsof failed: {reply.stderr.strip() or reply.returncode}")
    found = {}
    process = {}
    for line in reply.stdout.splitlines():
        field, value = line[:1], line[1:]
        if field == "p":
            process = {"pid": int(value), "process": "?", "user": "?"}
        elif field == "c":
            process["process"] = value
        elif field == "u":
            try:
                process["user"] = pwd.getpwuid(int(value)).pw_name
            except KeyError:
                process["user"] = value
        elif field == "n" and "pid" in process:
            port = int(value.rsplit(":", 1)[-1])
            found[(port, process["pid"])] = {"port": port, **process}
    if not found:
        return []
    ages = {}
    # One ps call for all processes avoids a subprocess for every listener.
    for line in command(["ps", "-axo", "pid=,etime="]).splitlines():
        parts = line.split()
        if len(parts) == 2:
            ages[int(parts[0])] = parts[1]
    return [{**value, "age": ages.get(value["pid"], "exited")} for _, value in sorted(found.items())]


def protected_pids():
    parents = {}
    for line in command(["ps", "-axo", "pid=,ppid="]).splitlines():
        pid, parent = map(int, line.split())
        parents[pid] = parent
    protected = {0, 1, os.getpid()}
    pid = os.getpid()
    while pid in parents and parents[pid] not in protected:
        pid = parents[pid]
        protected.add(pid)
    descendants = {os.getpid()}
    while True:
        children = {pid for pid, parent in parents.items() if parent in descendants}
        if children <= descendants:
            break
        descendants |= children
    return protected | descendants


def publish(rows, message):
    columns = [("port", "Port", "number"), ("process", "Process", "text"),
               ("pid", "PID", "number"), ("user", "User", "text"), ("age", "Age", "text")]
    emit({"type": "view", "view_id": "listeners", "op": "replace", "data": {
        "kind": "table", "columns": [{"id": id, "label": label, "type": kind} for id, label, kind in columns],
        "rows": [{"id": f"{row['port']}:{row['pid']}", "values": row} for row in rows]}})
    emit({"type": "view", "view_id": "summary", "op": "replace", "data": {
        "kind": "text", "format": "plain", "text": message if rows else EMPTY}})


def main():
    try:
        req = json.loads(sys.stdin.readline())
        message = "Select a row and press Enter to send SIGTERM."
        rows = listeners()
        if req["action"] == "kill":
            pid = req["input"]["pid"]
            if type(pid) is not int or pid < 2 or pid in protected_pids():
                raise ValueError("Cannot stop PID 1, this plugin, its parents, or its children.")
            if pid not in {row["pid"] for row in rows}:
                raise ValueError(f"PID {pid} is no longer listening. Refresh the table.")
            os.kill(pid, signal.SIGTERM)
            deadline = time.monotonic() + 2
            while True:
                rows = listeners()
                if pid not in {row["pid"] for row in rows} or time.monotonic() >= deadline:
                    break
                time.sleep(0.1)
            message = f"SIGTERM sent to PID {pid}."
            if pid in {row["pid"] for row in rows}:
                message += " It is still listening; refresh later. No SIGKILL was sent."
        publish(rows, message)
        emit({"type": "result", "ok": True, "summary": message if req["action"] == "kill" else
              (f"{len(rows)} listening port/process pairs." if rows else EMPTY), "data": {"listeners": len(rows)}})
        return 0
    except (OSError, ValueError, KeyError, TypeError, RuntimeError, subprocess.TimeoutExpired) as exc:
        emit({"type": "result", "ok": False, "summary": "Could not refresh or stop the process.", "data": None,
              "error": {"code": "PORTS_FAILED", "message": str(exc), "retryable": False}})
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
