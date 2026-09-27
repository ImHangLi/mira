"""MPP/1 task plugin: a table of TODO markers, and a row action that shows one match."""

import json
import os
import sys
from pathlib import Path

SKIP = {".git", ".mira", "node_modules", "target", "__pycache__", ".venv"}
MAX_FILES = 5000
MAX_FILE_BYTES = 1_000_000
MAX_ROWS = 1000
CONTEXT_LINES = 3


def emit(frame):
    line = json.dumps({"api": 1, **frame}, ensure_ascii=False, allow_nan=False)
    print(line, flush=True)


def result(ok, summary, data=None, code=None, message=None):
    frame = {"type": "result", "ok": ok, "summary": summary, "data": data}
    if not ok:
        frame["error"] = {"code": code, "message": message, "retryable": False}
    emit(frame)
    return 0 if ok else 1


def text_files(root):
    seen = 0
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(d for d in dirnames if d not in SKIP)
        for name in sorted(filenames):
            if seen >= MAX_FILES:
                return
            path = Path(dirpath) / name
            try:
                if path.stat().st_size > MAX_FILE_BYTES:
                    continue
                text = path.read_text(encoding="utf-8")
            except (UnicodeDecodeError, OSError):
                continue
            seen += 1
            yield path, text


def scan(root, marker):
    rows = []
    for path, text in text_files(root):
        rel = path.relative_to(root).as_posix()
        for number, line in enumerate(text.splitlines(), 1):
            if marker in line and len(rows) < MAX_ROWS:
                values = {"path": rel, "line": number, "text": line.strip()[:200]}
                rows.append({"id": f"{rel}:{number}", "values": values})
    table = {
        "kind": "table",
        "columns": [
            {"id": "path", "label": "File", "type": "text"},
            {"id": "line", "label": "Line", "type": "number"},
            {"id": "text", "label": "Text", "type": "text"},
        ],
        "rows": rows,
    }
    emit({"type": "view", "view_id": "matches", "op": "replace", "data": table})
    files = len({r["values"]["path"] for r in rows})
    return result(True, f"{len(rows)} match(es) in {files} file(s).", {"matches": len(rows), "files": files})


def show(root, rel, number):
    path = (root / rel).resolve()
    if root.resolve() not in path.parents:
        return result(False, "The path is outside the workspace.", code="BAD_PATH", message=rel)
    lines = path.read_text(encoding="utf-8").splitlines()
    first = max(1, number - CONTEXT_LINES)
    last = min(len(lines), number + CONTEXT_LINES)
    body = "\n".join(
        f"{'>' if n == number else ' '} {n:>5}  {lines[n - 1]}" for n in range(first, last + 1)
    )
    text = f"{rel}:{number}\n\n{body}\n"
    emit({"type": "view", "view_id": "context", "op": "replace", "data": {"kind": "text", "text": text, "format": "plain"}})
    return result(True, f"{rel}:{number}", {"path": rel, "line": number})


def main():
    try:
        req = json.loads(sys.stdin.readline())
        root = Path(req["context"]["workspace_root"])
        params = req["input"]
        if req["action"] == "show":
            return show(root, params["path"], int(params["line"]))
        return scan(root, params.get("marker", "TODO"))
    except (KeyError, ValueError, TypeError, OSError, UnicodeDecodeError) as exc:
        print(f"todos failed: {exc}", file=sys.stderr)
        return result(False, "Could not read the files.", code="READ_FAILED", message=str(exc))


if __name__ == "__main__":
    raise SystemExit(main())
