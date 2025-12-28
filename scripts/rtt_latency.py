#!/usr/bin/env python3
"""
Measure RTT latency of a single byte over a PTY-connected client.
Spawns the given command attached to a PTY, runs an optional setup command
inside the session (e.g., `stty raw -echo; cat`), then sends/receives 1 byte
iters times. Prints median and p95 in milliseconds.
"""

import argparse
import os
import pty
import select
import shlex
import statistics
import subprocess
import sys
import time


def measure(cmd, setup_cmd, iters, timeout):
    master, slave = pty.openpty()
    env = os.environ.copy()
    # Spawn target command attached to PTY.
    proc = subprocess.Popen(
        shlex.split(cmd),
        stdin=slave,
        stdout=slave,
        stderr=slave,
        env=env,
        close_fds=True,
    )
    os.close(slave)

    # Give process a moment to start.
    time.sleep(0.1)
    if setup_cmd:
        os.write(master, setup_cmd.encode() + b"\n")
        time.sleep(0.05)

    latencies = []
    for _ in range(iters):
        t0 = time.monotonic_ns()
        os.write(master, b"x")
        r, _, _ = select.select([master], [], [], timeout)
        if not r:
            proc.terminate()
            proc.wait(timeout=1)
            raise RuntimeError("timeout waiting for echo")
        data = os.read(master, 1)
        if not data:
            break
        t1 = time.monotonic_ns()
        latencies.append((t1 - t0) / 1e6)  # ms

    proc.terminate()
    try:
        proc.wait(timeout=1)
    except subprocess.TimeoutExpired:
        proc.kill()
    if not latencies:
        raise RuntimeError("no samples collected")
    median = statistics.median(latencies)
    p95 = statistics.quantiles(latencies, n=100)[94]
    return median, p95, len(latencies)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--cmd", required=True, help="command to run (quoted)")
    ap.add_argument("--setup", default="", help="setup command sent first")
    ap.add_argument("--iters", type=int, default=500)
    ap.add_argument("--timeout", type=float, default=1.0, help="per-iteration timeout seconds")
    args = ap.parse_args()
    try:
        median, p95, n = measure(args.cmd, args.setup, args.iters, args.timeout)
    except Exception as e:
        print(f"error: {e}", file=sys.stderr)
        sys.exit(1)
    print(f"samples={n} median_ms={median:.3f} p95_ms={p95:.3f}")


if __name__ == "__main__":
    main()
