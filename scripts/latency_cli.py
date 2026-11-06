#!/usr/bin/env python3
"""Measure keystroke->echo RTT through the REAL `dutabo serial` CLI.

Spawns the CLI in a PTY (full VT interactive path: raw mode, alternate
screen, terminal render), types single characters and measures the time
until the expected text is rendered in the emulated terminal. Compare
against the board and ws probes in
sermcp/tests/latency_probe.rs to attribute tail latency.

Usage:
    uv run --no-project --with pyte python scripts/latency_cli.py [samples] [burst_samples]

Environment:
    DUTABO_BIN  path to the dutabo binary
    DUT         DUT name (default yr3588)
"""

import os
import pty
import secrets
import select
import subprocess
import sys
import time

import pyte


def percentile(sorted_samples, q):
    if not sorted_samples:
        return float("nan")
    idx = round(q * (len(sorted_samples) - 1))
    return sorted_samples[min(idx, len(sorted_samples) - 1)]


def report(label, samples):
    s = sorted(samples)
    print(
        f"{label:<24} samples={len(s):>3} p50={percentile(s,.5):>8.2f} "
        f"p90={percentile(s,.9):>8.2f} p99={percentile(s,.99):>8.2f} "
        f"max={s[-1]:>9.2f} (ms)"
    )


def read_available(master, timeout):
    """Read whatever is pending on the master within `timeout` seconds."""
    ready, _, _ = select.select([master], [], [], timeout)
    if not ready:
        return b""
    return os.read(master, 65536)


def wait_for_row(master, screen, stream, predicate, deadline):
    while True:
        for row in screen.display:
            if predicate(row):
                return row
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise TimeoutError("expected echo did not render")
        data = read_available(master, remaining)
        if not data:
            raise TimeoutError("PTY produced no echo")
        stream.feed(data)


def write_input(master, payload):
    if os.write(master, payload) != len(payload):
        raise OSError("short PTY input write")


def measure(master, screen, stream, label, count, width):
    nonce = secrets.token_hex(4)
    rtts = []
    for i in range(count):
        prefix = f"# {nonce}{i:08x} "
        payload = "".join(chr(ord("a") + (i * 7 + j) % 26) for j in range(width))
        try:
            write_input(master, b"\x15" + prefix.encode())
            row = wait_for_row(
                master, screen, stream, lambda row: row.rstrip().endswith(prefix.rstrip()),
                time.monotonic() + 5,
            )
            column = row.index(prefix)
            expected = prefix + payload
            begin = time.monotonic()
            write_input(master, payload.encode())
            wait_for_row(
                master, screen, stream,
                lambda row: row[column:].rstrip() == expected,
                begin + 5,
            )
        except (OSError, TimeoutError) as error:
            raise SystemExit(
                f"{label}: attempted={i + 1}, complete={len(rtts)}, requested={count}; {error}"
            ) from error
        rtts.append((time.monotonic() - begin) * 1000)
        time.sleep(0.06)
    report(label, rtts)


def main():
    samples = int(sys.argv[1]) if len(sys.argv) > 1 else 200
    burst_samples = int(sys.argv[2]) if len(sys.argv) > 2 else 100
    if min(samples, burst_samples) <= 0:
        raise SystemExit("sample counts must be positive")
    dut = os.environ.get("DUT", "yr3588")
    binpath = os.environ.get(
        "DUTABO_BIN",
        os.path.join(
            os.path.dirname(__file__),
            "..",
            "target",
            "x86_64-unknown-linux-musl",
            "release",
            "dutabo",
        ),
    )

    master, slave = pty.openpty()
    # A PTY starts at 0x0; give it a real window size (the CLI also clamps,
    # but a sane grid keeps the render path realistic).
    import fcntl
    import struct
    import termios

    fcntl.ioctl(master, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 120, 0, 0))
    # The CLI checks stdin for TTY-ness; a PTY slave is a TTY, so the full
    # interactive VT session (history replay + render) runs.
    proc = subprocess.Popen(
        [binpath, "serial", "--dut", dut],
        stdin=slave,
        stdout=slave,
        stderr=subprocess.DEVNULL,
        close_fds=True,
    )
    os.close(slave)
    try:
        # Consume initial history and the CLI's prompt nudge before typing.
        screen = pyte.Screen(120, 40)
        stream = pyte.ByteStream(screen)
        wait_for_row(
            master, screen, stream,
            lambda row: row.rstrip().endswith(("#", "$")),
            time.monotonic() + 10,
        )
        measure(master, screen, stream, "cli single-char", samples, 1)
        measure(master, screen, stream, "cli burst-10", burst_samples, 10)
    finally:
        # Quit the session: Ctrl-T q (the CLI's own escape sequence).
        try:
            os.write(master, b"\x15\x14q")
            time.sleep(0.8)
        except OSError:
            pass
        try:
            proc.terminate()
            proc.wait(timeout=3)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.wait()
        os.close(master)


if __name__ == "__main__":
    main()
