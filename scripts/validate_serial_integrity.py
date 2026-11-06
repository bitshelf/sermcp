"""Check serial command latency and persisted sequence integrity through MCP.

Run with uv against an already running project service and an idle Linux shell.
The workload only prints text; it does not reset or reconfigure the board.
"""

import argparse
import hashlib
import http.client
import json
import math
import re
import statistics
import time
import uuid
from pathlib import Path

parser = argparse.ArgumentParser()
parser.add_argument("phase")
parser.add_argument("--inspect", action="store_true")
parser.add_argument("--project", type=Path, default=Path.cwd())
parser.add_argument("--port", type=int, required=True)
parser.add_argument("--dut", required=True)
parser.add_argument("--output", type=Path, required=True)
parser.add_argument("--samples", type=int, default=60)
parser.add_argument("--warmup", type=int, default=0)
args = parser.parse_args()
if args.samples < 1 or args.warmup < 0:
    parser.error("samples must be positive and warmup must be non-negative")
project = args.project.resolve()
port = args.port
connection = http.client.HTTPConnection("127.0.0.1", port, timeout=150)
session = None
protocol = "2025-03-26"
request_id = 0


def request(method, params=None, notification=False):
    global request_id, session
    request_id += 1
    body = {"jsonrpc": "2.0", "method": method}
    if not notification:
        body["id"] = request_id
    if params is not None:
        body["params"] = params
    headers = {
        "Content-Type": "application/json",
        "Accept": "application/json, text/event-stream",
    }
    if session:
        headers["Mcp-Session-Id"] = session
        headers["MCP-Protocol-Version"] = protocol
    connection.request("POST", "/mcp", json.dumps(body), headers)
    response = connection.getresponse()
    if response.getheader("mcp-session-id"):
        session = response.getheader("mcp-session-id")
    data = response.read().decode()
    if response.status >= 300:
        raise RuntimeError(f"HTTP {response.status}: {data[:2000]}")
    if notification:
        return None
    frames = [
        json.loads(line[5:].strip())
        for line in data.splitlines()
        if line.startswith("data:") and line[5:].strip()
    ]
    frame = (
        next((item for item in frames if item.get("id") == request_id), None)
        if frames
        else json.loads(data)
    )
    if frame is None or "error" in frame:
        raise RuntimeError(frame)
    return frame["result"]


def tool(name, **arguments):
    result = request(
        "tools/call", {"name": name, "arguments": {"dut": args.dut, **arguments}}
    )
    if result.get("isError"):
        raise RuntimeError(result)
    if "structuredContent" in result:
        return result["structuredContent"]
    return json.loads(
        next(item["text"] for item in result["content"] if item["type"] == "text")
    )


def execute(command, timeout=120):
    result = tool("serial_exec", command=command, mode="shell", timeout=timeout)
    if (
        result.get("exit_code") != 0
        or result.get("timed_out")
        or result.get("truncated")
    ):
        raise RuntimeError(result)
    return result["output"]


try:
    initialized = request(
        "initialize",
        {
            "protocolVersion": protocol,
            "capabilities": {},
            "clientInfo": {"name": "sermcp-regression", "version": "1"},
        },
    )
    protocol = initialized["protocolVersion"]
    request("notifications/initialized", notification=True)
    tools = request("tools/list")["tools"]
    state = tool("serial_get_state")
    print(
        json.dumps(
            {
                "phase": args.phase,
                "state": state,
                "tools": [entry["name"] for entry in tools],
            },
            ensure_ascii=False,
        ),
        flush=True,
    )
    if args.inspect:
        print(
            execute("uname -sr; cat /proc/device-tree/model 2>/dev/null; printf '\\n'"),
            flush=True,
        )
    else:
        if state["state"] != "active":
            raise RuntimeError("The integrity workload requires an active Linux shell")
        prefix = "SMREG_" + uuid.uuid4().hex[:12]
        log_directory = (project / state["log_path"]).parent
        paths = [
            log_directory / name
            for name in (
                "current.full.log",
                "current.serial.log",
                "full.full.log",
                "full.serial.log",
            )
        ]
        offsets = {str(path): path.stat().st_size for path in paths}
        initial_inodes = {str(path): path.stat().st_ino for path in paths}
        rtts = []
        warmup_ms = []
        for index in range(args.warmup):
            marker = f"{prefix}_WARMUP_{index:04d}"
            start = time.perf_counter()
            output = execute(f"printf '{marker}\\n'")
            warmup_ms.append((time.perf_counter() - start) * 1000)
            assert marker in output, (marker, output)
        for index in range(args.samples):
            marker = f"{prefix}_RTT_{index:04d}"
            start = time.perf_counter()
            output = execute(f"printf '{marker}\\n'")
            rtts.append((time.perf_counter() - start) * 1000)
            assert marker in output, (marker, output)
        count = 3000
        padding = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ"
        command = f"i=0; while [ $i -lt {count} ]; do printf '{prefix}_SEQ_%06d_{padding}\\n' \"$i\"; i=$((i+1)); done"
        start = time.perf_counter()
        output = execute(command)
        elapsed = time.perf_counter() - start
        pattern = re.compile(re.escape(prefix) + r"_SEQ_[0-9]{6}_" + padding)
        expected = [f"{prefix}_SEQ_{i:06d}_{padding}" for i in range(count)]
        captured = pattern.findall(output)
        assert captured == expected, (
            f"command capture: {len(captured)}/{count} lines or ordering mismatch"
        )
        persisted = {}
        integrity_errors = []
        for path in paths:
            verify_start = time.monotonic()
            deadline = time.monotonic() + 10
            while True:
                with path.open("rb") as stream:
                    stream.seek(offsets[str(path)])
                    data = stream.read()
                records = pattern.findall(data.decode(errors="replace"))
                if len(records) >= count or time.monotonic() >= deadline:
                    break
                time.sleep(0.1)
            persisted[str(path)] = {
                "records": len(records),
                "sha256": hashlib.sha256("\n".join(records).encode()).hexdigest(),
                "verification_seconds": time.monotonic() - verify_start,
                "start_offset": offsets[str(path)],
                "initial_inode": initial_inodes[str(path)],
                "final_inode": path.stat().st_ino,
            }
            if records != expected:
                integrity_errors.append(
                    f"{path}: {len(records)}/{count} lines or ordering mismatch"
                )
                persisted[str(path)]["missing"] = sorted(set(expected) - set(records))
                persisted[str(path)]["whole_file_records"] = len(
                    pattern.findall(path.read_text(errors="replace"))
                )
        ordered = sorted(rtts)
        final_state = tool("serial_get_state")
        persistence = final_state["log_persistence"]
        if (
            persistence["last_error"] is not None
            or persistence["pending_bytes"] != 0
            or persistence["received_bytes"] != persistence["durable_bytes"]
        ):
            integrity_errors.append(f"Log persistence has not completed: {persistence}")
        result = {
            "phase": args.phase,
            "protocol": protocol,
            "prefix": prefix,
            "samples_ms": rtts,
            "warmup_ms": warmup_ms,
            "p50_ms": statistics.median(rtts),
            "p95_ms": ordered[math.ceil(len(ordered) * 0.95) - 1],
            "p99_ms": ordered[math.ceil(len(ordered) * 0.99) - 1],
            "burst_lines": count,
            "burst_seconds": elapsed,
            "payload_bytes": sum(len(line) + 1 for line in expected),
            "logs": persisted,
            "final_state": final_state,
            "integrity_errors": integrity_errors,
        }
        destination = args.output
        destination.write_text(json.dumps(result, indent=2))
        if integrity_errors:
            raise RuntimeError(
                f"Integrity checks failed; evidence saved to {destination}: {integrity_errors}"
            )
        print(
            json.dumps(
                {
                    key: value
                    for key, value in result.items()
                    if key not in ("samples_ms", "final_state")
                },
                indent=2,
            ),
            flush=True,
        )
finally:
    if session:
        connection.request(
            "DELETE",
            "/mcp",
            headers={"Mcp-Session-Id": session, "MCP-Protocol-Version": protocol},
        )
        connection.getresponse().read()
    connection.close()
