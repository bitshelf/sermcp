#!/usr/bin/env python3
"""dutabo init [Test] stability driver.

Drives the SAME tool sequence the [Test] button runs, against a real MCP
server it spawns/stops per round (each round == one `dutabo init` lifetime).
Injects target-board states between rounds. Emits one JSONL line per round.

Run:  python3 scripts/init_test_stability.py \
        --project-dir /media/loh/rockchip/lr3576_v2.1 \
        --jsonl /tmp/init-stability.jsonl
Exit: prints STABLE-PASS when coverage + 30-round streak are all green.
"""
import argparse, json, os, signal, socket, subprocess, sys, time

TOOLS = {
    "reset": "serial_reset",
    "state": "serial_get_state",
    "baud": "serial_test_baud",
    "verify": "serial_verify_relay",
    "uboot": "serial_enter_uboot",
    "learn": "serial_learn_connection",
}

def raw_post(port, path, body, headers, timeout=150):
    """Blocking full-read POST (Connection: close): immune to the chunked
    SSE stream that breaks http.client on 60s+ tool calls."""
    import socket
    payload = json.dumps(body)
    req = (f"POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n"
           + "".join(f"{k}: {v}\r\n" for k, v in headers.items())
           + f"Content-Length: {len(payload.encode())}\r\nConnection: close\r\n\r\n")
    s = socket.create_connection(("127.0.0.1", port), timeout=10)
    s.settimeout(timeout)
    s.sendall((req + payload).encode())
    buf = b""
    try:
        while True:
            chunk = s.recv(65536)
            if not chunk:
                break
            buf += chunk
    except socket.timeout:
        pass
    s.close()
    head, _, body = buf.partition(b"\r\n\r\n")
    if b"chunked" in head.lower():
        out, rest = b"", body
        while True:
            i = rest.find(b"\r\n")
            if i < 0:
                break
            try:
                size = int(rest[:i].split(b";")[0], 16)
            except ValueError:
                break
            rest = rest[i + 2:]
            if size == 0:
                break
            out += rest[:size]
            rest = rest[size + 2:]
        body = out
    return head.decode("utf-8", "replace") + "\r\n\r\n" + body.decode("utf-8", "replace")


class Mcp:
    def __init__(self, port):
        self.port = port
        self.sid = None
        self.next_id = 1
        r = self._post("initialize", {"protocolVersion": "2024-11-05",
                                      "capabilities": {},
                                      "clientInfo": {"name": "init-stability", "version": "0"}})
        self._post("notifications/initialized", None, notify=True)

    def _post(self, method, params, notify=False):
        body = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            body["params"] = params
        if not notify:
            body["id"] = self.next_id
            self.next_id += 1
        headers = {"Content-Type": "application/json",
                   "Accept": "application/json, text/event-stream"}
        if self.sid:
            headers["mcp-session-id"] = self.sid
        raw = raw_post(self.port, "/mcp", body, headers, timeout=150)
        if self.sid is None:
            for line in raw.splitlines():
                if line.lower().startswith("mcp-session-id:"):
                    self.sid = line.split(":", 1)[1].strip()
        if notify:
            return None
        # Streamable HTTP answers as SSE: take the LAST data: line with a
        # jsonrpc payload (raw socket read sees chunk framing too).
        import re
        cands = [l[5:].strip() for l in raw.splitlines() if l.startswith("data:")]
        for cand in reversed(cands):
            try:
                return json.loads(cand)
            except json.JSONDecodeError:
                continue
        return {"success": False, "error": f"unparseable response: {raw[:200]}"}

    def call(self, name, arguments):
        r = self._post("tools/call", {"name": name, "arguments": arguments})
        try:
            payload = r["result"]["content"][0]["text"]
            return json.loads(payload)
        except Exception:
            return {"success": False, "error": f"unparseable: {r}"}

def wait_state(mcp, ok_states, timeout):
    deadline = time.time() + timeout
    last = None
    while time.time() < deadline:
        st = mcp.call(TOOLS["state"], {})
        last = st.get("state")
        if last in ok_states:
            return last
        time.sleep(2)
    return last

def inject(mcp, state, baud):
    if state == "booted":
        mcp.call(TOOLS["reset"], {"wait_boot": False})
        return wait_state(mcp, {"active"}, 60) == "active"
    if state == "booting":
        mcp.call(TOOLS["reset"], {"wait_boot": False})
        time.sleep(8)
        return True
    if state == "uboot":
        mcp.call(TOOLS["reset"], {"wait_boot": False})
        time.sleep(5)
        r = mcp.call(TOOLS["uboot"], {"interrupt_char": "ctrl_c"})
        return bool(r.get("success"))
    if state == "loader":
        mcp.call(TOOLS["reset"], {"wait_boot": False})
        time.sleep(5)
        mcp.call(TOOLS["uboot"], {"interrupt_char": "ctrl_c"})
        r = mcp.call("serial_uboot_command", {"command": "reboot loader", "timeout": 5})
        time.sleep(7)  # loader re-enumeration
        return True
    return False

def run_round(args, state):
    proc = subprocess.Popen([args.mcp_bin, "--http", f"127.0.0.1:{args.port}"],
                            cwd=args.project_dir,
                            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                            start_new_session=True)
    try:
        for _ in range(30):
            try:
                mcp = Mcp(args.port)
                break
            except OSError:
                time.sleep(1)
        else:
            return {"pass": False, "error": "MCP did not start"}

        items = {}
        # Pre-TEST reset (user rule): known board state before probing.
        mcp.call(TOOLS["reset"], {"wait_boot": False})
        st = wait_state(mcp, {"active", "booting", "uboot"}, 45)
        items["pre_ready"] = st

        if inject_state := (state if state != "none" else None):
            items["inject_ok"] = inject(mcp, inject_state, args.baud)
            # The TEST's own pre-reset then pulls the board back out of the
            # injected state — exactly the robustness under test.
            mcp.call(TOOLS["reset"], {"wait_boot": False})
            wait_state(mcp, {"active", "booting", "uboot"}, 45)

        items["baud"] = bool(mcp.call(TOOLS["baud"], {
            "baud": args.baud, "capture_secs": 3, "use_reset": True}).get("success"))
        items["relay"] = bool(mcp.call(TOOLS["verify"], {}).get("success"))
        items["uboot"] = bool(mcp.call(TOOLS["uboot"], {"interrupt_char": "ctrl_c"}).get("success"))
        learn = mcp.call(TOOLS["learn"], {
            "method": "hardware",
            "reference_log_path": args.reference_log,
            "cycles": 2,
        })
        items["learn"] = bool(learn.get("success"))

        # flash.devices: the list command must produce a device list (not
        # usage text) — run on the dev host over ssh like the probe does.
        try:
            out = subprocess.run(["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5",
                                  f"{args.ssh_user}@{args.dev_host}", args.list_cmd],
                                 capture_output=True, text=True, timeout=35)
            txt = out.stdout
            items["flash_list"] = ("connected(" in txt or txt.strip() != "") and "Usage" not in txt
        except Exception as e:
            items["flash_list"] = False

        items["pass"] = all(v is True for k, v in items.items()
                            if k not in ("inject_ok", "pre_ready"))
        return items
    finally:
        # USER RULE: after every round the spawned MCP must be GONE.
        try:
            proc.terminate()
            proc.wait(timeout=5)
        except Exception:
            proc.kill()

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--project-dir", default=os.getcwd())
    ap.add_argument("--mcp-bin", default=os.path.expanduser("~/.local/bin/sermcp"))
    ap.add_argument("--port", type=int, default=3065)
    ap.add_argument("--dev-host", default="192.168.1.105")
    ap.add_argument("--ssh-user", default="linaro")
    ap.add_argument("--baud", type=int, default=115200)
    ap.add_argument("--list-cmd", default="upgrade_tool ld")
    ap.add_argument("--reference-log", default="")
    ap.add_argument("--jsonl", default="/tmp/init-stability.jsonl")
    ap.add_argument("--states", default="none,booted,booting,uboot,loader")
    ap.add_argument("--coverage-per-state", type=int, default=5)
    ap.add_argument("--streak", type=int, default=30)
    ap.add_argument("--max-rounds", type=int, default=120)
    args = ap.parse_args()

    if not args.reference_log:
        args.reference_log = os.path.join(
            args.project_dir, ".dut-serial", "rk3576-ubuntu", "reference-boot.log")

    states = args.states.split(",")
    done, streak = 0, 0
    coverage = {s: 0 for s in states}
    with open(args.jsonl, "a") as log:
        while done < args.max_rounds:
            done += 1
            state = "none"
            for st in states:
                if coverage[st] < args.coverage_per_state:
                    state = st
                    break
            t0 = time.time()
            items = run_round(args, state)
            if state in coverage:
                coverage[state] += 1
            ok = items.get("pass", False)
            streak = streak + 1 if ok else 0
            rec = {"round": done, "state": state, "secs": round(time.time() - t0, 1),
                   "items": items, "streak": streak}
            log.write(json.dumps(rec, ensure_ascii=False) + "\n")
            log.flush()
            print(json.dumps(rec, ensure_ascii=False), flush=True)
            if done >= args.coverage_per_state * len(states) and streak >= args.streak:
                print("STABLE-PASS", flush=True)
                return
    print("NOT-STABLE (max rounds reached)", flush=True)

if __name__ == "__main__":
    main()
