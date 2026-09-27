#!/usr/bin/env python3
"""TUI latency in a real PTY: first operable frame (cold and warm host) and key→screen
change, idle and during a log flood. Needs `pyte` (pip install pyte). Prints JSON.

Usage: scripts/perf/tui.py --mira target/release/mira [--starts 20] [--keys 300]
Runs in a scratch copy of tests/fixtures/workspace under /tmp and stops everything it starts.
Latency is measured from writing a key to the PTY until the parsed screen changes, so it
includes PTY and parser overhead on top of the TUI's own time.
"""
import argparse, sys, fcntl, json, os, pty, select, shutil, struct, subprocess, tempfile, termios, time

import pyte


def pct(xs, p):
    xs = sorted(xs)
    if not xs:
        return None
    k = (len(xs) - 1) * p
    lo, hi = int(k), min(int(k) + 1, len(xs) - 1)
    return round((xs[lo] + (xs[hi] - xs[lo]) * (k - lo)) * 1000, 2)


def summary(xs):
    return {"n": len(xs), "p50_ms": pct(xs, .5), "p95_ms": pct(xs, .95), "p99_ms": pct(xs, .99),
            "max_ms": round(max(xs) * 1000, 2) if xs else None}


class Tui:
    def __init__(self, mira, cwd, env, cols=120, rows=40):
        self.screen = pyte.Screen(cols, rows)
        self.stream = pyte.ByteStream(self.screen)
        self.t0 = time.perf_counter()
        self.pid, self.fd = pty.fork()
        if self.pid == 0:
            # Never return into the parent's code from the child.
            try:
                os.chdir(cwd)
                os.execve(mira, [mira], env)
            except BaseException as e:  # noqa: BLE001
                os.write(2, f"exec failed: {e!r}\n".encode())
            finally:
                os._exit(127)
        fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))
        os.set_blocking(self.fd, False)

    def pump(self, timeout):
        r, _, _ = select.select([self.fd], [], [], timeout)
        if r:
            try:
                self.stream.feed(os.read(self.fd, 1 << 16))
                return True
            except (BlockingIOError, OSError):
                return False
        return False

    def text(self):
        return "\n".join("".join(self.screen.buffer[y][x].data for x in range(self.screen.columns)) for y in range(self.screen.lines))

    def wait_for(self, needle, limit=10):
        end = time.perf_counter() + limit
        while time.perf_counter() < end:
            self.pump(0.005)
            if needle in self.text():
                return time.perf_counter() - self.t0
        return None

    def key_latency(self, key, limit=2):
        self.pump(0.05)
        while self.pump(0):
            pass
        before = self.text()
        t = time.perf_counter()
        os.write(self.fd, key)
        end = t + limit
        while time.perf_counter() < end:
            if self.pump(0.001) and self.text() != before:
                return time.perf_counter() - t
        return None

    def close(self):
        # Keep draining the PTY while the TUI exits: macOS blocks a tty close until its
        # pending output (the terminal-restore sequences) is read.
        try:
            os.write(self.fd, b"q")
        except OSError:
            pass
        for sig in (None, 9):
            if sig and self.pid > 0:
                try:
                    os.kill(self.pid, sig)
                except ProcessLookupError:
                    return
            end = time.perf_counter() + 3
            while time.perf_counter() < end:
                self.pump(0.02)
                pid, _ = os.waitpid(self.pid, os.WNOHANG)
                if pid:
                    return


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mira", required=True)
    ap.add_argument("--starts", type=int, default=20)
    ap.add_argument("--keys", type=int, default=300)
    a = ap.parse_args()
    mira = os.path.abspath(a.mira)
    scratch = tempfile.mkdtemp(prefix="mira-tp-", dir="/tmp")
    env = dict(os.environ, MIRA_DATA_HOME=f"{scratch}/data", MIRA_RUNTIME_DIR=f"{scratch}/run", TERM="xterm-256color")
    root = f"{scratch}/pg"
    shutil.copytree(os.path.join(os.path.dirname(__file__), "../../tests/fixtures/workspace"), root)
    cli = lambda *args: subprocess.run([mira, *args, "--json"], cwd=root, env=env, capture_output=True)
    def stop_host():
        # Stop the host by the PID in its owner record; pattern kills could hit this script.
        for f in __import__("glob").glob(f"{scratch}/run/*.owner"):
            try:
                os.kill(json.load(open(f))["pid"], 15)
            except (OSError, ValueError, KeyError):
                pass
        time.sleep(0.4)
    out = {"binary": mira, "terminal": "PTY + pyte, 120x40"}
    try:
        cold, warm = [], []
        for _ in range(a.starts):
            stop_host()
            t = Tui(mira, root, env)
            v = t.wait_for("DEV")
            if v:
                cold.append(v)
            t.close()
        cli("up", "--background", "--ttl", "10m")
        for _ in range(a.starts):
            t = Tui(mira, root, env)
            v = t.wait_for("DEV")
            if v:
                warm.append(v)
            t.close()
        print(f"starts done: cold={len(cold)} warm={len(warm)}", file=sys.stderr)
        out["first_frame_cold_host"] = summary(cold)
        out["first_frame_warm_host"] = summary(warm)
        t = Tui(mira, root, env)
        t.wait_for("DEV")
        # Idle resources of Core + TUI: 10 s of samples with no plugin running.
        time.sleep(1)
        host = [str(json.load(open(f))["pid"]) for f in __import__("glob").glob(f"{scratch}/run/*.owner")]
        pids = ",".join([str(t.pid)] + host[:1])
        cpu0 = subprocess.run(["ps", "-o", "cputime=", "-p", pids], capture_output=True, text=True).stdout.split()
        time.sleep(10)
        ps = subprocess.run(["ps", "-o", "cputime=,rss=", "-p", pids], capture_output=True, text=True).stdout.split()
        def secs(v):
            parts = [float(x) for x in v.replace("-", ":").split(":")]
            return sum(x * 60 ** i for i, x in enumerate(reversed(parts)))
        cpu1, rss = ps[0::2], [int(x) for x in ps[1::2]]
        used = sum(secs(b) - secs(a) for a, b in zip(cpu0, cpu1))
        out["idle_core_plus_tui"] = {"cpu_percent_of_one_core": round(used / 10 * 100, 2), "rss_mib": round(sum(rss) / 1024, 1), "processes": len(rss)}
        idle = [x for x in (t.key_latency(b"j" if i % 2 == 0 else b"k") for i in range(a.keys)) if x is not None]
        out["navigation_idle"] = summary(idle)
        print(f"idle keys done: {len(idle)}", file=sys.stderr)
        cli("start", "dev.flood")
        time.sleep(2)
        flood = [x for x in (t.key_latency(b"j" if i % 2 == 0 else b"k") for i in range(a.keys)) if x is not None]
        out["navigation_during_flood"] = summary(flood)
        t.close()
        cli("down", "--wait")
    finally:
        stop_host()
        shutil.rmtree(scratch, ignore_errors=True)
    print(json.dumps(out, indent=2))


if __name__ == "__main__":
    main()
