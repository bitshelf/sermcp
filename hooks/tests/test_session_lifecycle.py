"""Session-bound startup and persistent endpoint-fault regression checks."""

import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

HOOKS = Path(__file__).resolve().parents[1] / "claude"
sys.path.insert(0, str(HOOKS))
import lib

spec = importlib.util.spec_from_file_location("session_prompt", HOOKS / "user-prompt-submit.py")
prompt = importlib.util.module_from_spec(spec)
spec.loader.exec_module(prompt)


class SessionLifecycleTests(unittest.TestCase):
    def test_stdio_restart_never_spawns_http_twin(self):
        with tempfile.TemporaryDirectory() as td:
            (Path(td) / ".mcp.json").write_text(json.dumps({
                "mcpServers": {"sermcp": {"command": "sermcp"}}
            }))
            with patch.object(prompt, "_mcp_http_port") as port, patch.object(prompt, "spawn_http_mcp") as spawn:
                prompt._restart_mcp(td)
                port.assert_not_called()
                spawn.assert_not_called()

    def test_unknown_session_never_kills_or_replaces_http(self):
        with patch.object(prompt, "_mcp_http_port", return_value=3000), \
             patch.object(prompt, "http_health_ok", return_value=False), \
             patch.object(prompt, "agent_session_process", return_value=None), \
             patch.object(prompt, "terminate_pid") as terminate, \
             patch.object(prompt, "spawn_http_mcp") as spawn:
            prompt._restart_mcp("/unused/project")
            terminate.assert_not_called()
            spawn.assert_not_called()

    def test_session_pid_reuse_is_rejected(self):
        with patch.dict(os.environ, {"SERMCP_SESSION_PID": "42", "SERMCP_SESSION_START_TICKS": "100"}), \
             patch.object(lib, "_session_process_identity", return_value=("101", 1)):
            self.assertIsNone(lib.agent_session_process())

    def test_engine_fault_remains_visible_without_competing_socket(self):
        with patch("http.client.HTTPConnection") as connection:
            response = connection.return_value.getresponse.return_value
            response.status = 200
            response.read.return_value = json.dumps({
                "server": {"pid": 42},
                "serial_error": {"code": "serial_endpoint_shared", "message": "endpoint shared"},
            }).encode()
            alerts = lib._engine_endpoint_alert({"mcp_pid": 42, "mcp_http_port": 3000})
            self.assertEqual(len(alerts), 1)
            self.assertIn("serial_endpoint_shared", alerts[0])
            self.assertEqual(lib._engine_endpoint_alert({"mcp_pid": 43, "mcp_http_port": 3000}), [])


if __name__ == "__main__":
    unittest.main()
