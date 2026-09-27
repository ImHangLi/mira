#!/usr/bin/env python3
"""Reproducible latency measurements for the mira binary. Prints JSON with p50/p95/p99/max.

Usage: scripts/perf/measure.py --mira target/release/mira [--samples 50] [--flood-seconds 60]
Runs in a scratch copy of tests/fixtures/workspace with MIRA_DATA_HOME/MIRA_RUNTIME_DIR under /tmp,
so it never touches ~/Library. It stops everything it starts.
"""
import argparse, json, os, platform, shutil, statistics, subprocess, sys, tempfile, time

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

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mira", required=True)
    ap.add_argument("--samples", type=int, default=50)
    ap.add_argument("--flood-seconds", type=float, default=60)
    ap.add_argument("--flood-rounds", type=int, default=3)
    a = ap.parse_args()
    mira = os.path.abspath(a.mira)
    scratch = tempfile.mkdtemp(prefix="mira-perf-", dir="/tmp")
    env = dict(os.environ, MIRA_DATA_HOME=f"{scratch}/data", MIRA_RUNTIME_DIR=f"{scratch}/run")
    root = f"{scratch}/pg"
    shutil.copytree(os.path.join(os.path.dirname(__file__), "../../tests/fixtures/workspace"), root)

    def run(*args, check=True):
        t = time.perf_counter()
        p = subprocess.run([mira, *args, "--json"], cwd=root, env=env, capture_output=True, text=True)
        dt = time.perf_counter() - t
        if check and p.returncode != 0:
            raise SystemExit(f"{args} failed: {p.stdout} {p.stderr}")
        return dt, p

    def stop_host():
        subprocess.run(["pkill", "-f", f"mira __host --root {os.path.realpath(root)}"], capture_output=True)
        time.sleep(0.3)

    out = {"machine": {"platform": platform.platform(), "machine": platform.machine(),
                       "cpu": subprocess.run(["sysctl", "-n", "machdep.cpu.brand_string"], capture_output=True, text=True).stdout.strip()},
           "binary": mira, "samples": a.samples}
    try:
        # Cold: no host running; each sample starts a host.
        cold = []
        for _ in range(min(a.samples, 20)):
            stop_host()
            cold.append(run("status")[0])
        out["cold_status_cli"] = summary(cold)
        # Warm: host running (kept alive by a background session).
        run("up", "--background", "--ttl", "10m")
        for name, args in (("warm_status_cli", ("status",)), ("warm_catalog_cli", ("catalog",)),
                           ("warm_catalog_if_revision_cli", ("catalog", "--if-revision", "1"))):
            run(*args)
            out[name] = summary([run(*args)[0] for _ in range(a.samples)])
        # Log flood: ~5000 lines/s x ~200 bytes; control latency measured during the flood.
        rounds = []
        for _ in range(a.flood_rounds):
            run("start", "dev.flood")
            t_end = time.time() + a.flood_seconds
            during = []
            while time.time() < t_end:
                during.append(run("status")[0])
                time.sleep(0.2)
            t = time.perf_counter()
            run("stop", "dev.flood", "--wait")
            stop_s = time.perf_counter() - t
            _, rec = run("runs", "--action", "dev.flood", "--limit", "1")
            log = json.loads(rec.stdout)["data"]["runs"][0]["log"]
            rounds.append({"status_during_flood": summary(during), "stop_wait_ms": round(stop_s * 1000, 1),
                           "log_last_seq": log["last_seq"], "dropped_records": log["dropped_records"]})
        out["log_flood"] = rounds
        run("down", "--wait")
    finally:
        stop_host()
        shutil.rmtree(scratch, ignore_errors=True)
    json.dump(out, sys.stdout, indent=2)
    print()

if __name__ == "__main__":
    main()
