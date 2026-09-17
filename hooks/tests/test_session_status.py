"""Session identity and server-rendered status integration checks."""

import importlib.util
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

HOOKS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HOOKS / "claude"))
import inventory
import owner


def load_hook(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


codex = load_hook("codex_status", HOOKS / "codex" / "serial-status.py")
claude = load_hook("claude_status", HOOKS / "claude" / "statusline.py")


class SessionStatusTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.inv = {
            "schema_version": 1,
            "mcp_pid": os.getpid(),
            "duts": [
                {
                    "dut_name": "board-a", "state": "active",
                    "state_text": "ANSI_ACTIVE", "state_plain": "board-a:active",
                    "guest_display": {"state": "busy", "text": "ANSI_BUSY", "plain": "board-a:busy"},
                },
                {
                    "dut_name": "board-b", "state": "dutabo",
                    "state_text": "ANSI_DUTABO", "state_plain": "board-b:dutabo",
                    "guest_display": {"state": "dutabo", "text": "ANSI_DUTABO", "plain": "board-b:dutabo"},
                },
            ],
        }
        owner.claim_owner(str(self.root), os.getpid(), "first", os.getpid())
        (self.root / ".dut-serial" / "inventory.json").write_text(json.dumps(self.inv))

    def test_second_session_cannot_replace_first(self):
        record = owner.claim_owner(str(self.root), os.getpid(), "second", os.getpid())
        self.assertEqual(record["owner_session_id"], "first")
        self.assertEqual(inventory.session_state_texts(str(self.root), self.inv, "first"),
                         ["ANSI_ACTIVE", "ANSI_DUTABO"])
        for session in ["second", None]:
            self.assertEqual(inventory.session_state_texts(str(self.root), self.inv, session),
                             ["ANSI_BUSY", "ANSI_DUTABO"])

    def test_claude_and_codex_use_identical_server_views(self):
        with patch.object(claude, "inventory_is_live", return_value=True), \
             patch.object(codex, "inventory_is_live", return_value=True):
            for session in ["first", "second", None]:
                _, ansi, plain = codex.inventory_states(self.root, session)
                self.assertEqual(claude._serial_text(str(self.root), session), " ".join(ansi))
                self.assertEqual(plain[-1], "board-b:dutabo")
            self.assertEqual(codex.inventory_states(self.root, "second")[2][0], "board-a:busy")

    def test_dead_server_never_renders_cached_busy_or_dutabo(self):
        with patch.object(claude, "inventory_is_live", return_value=False), \
             patch.object(codex, "inventory_is_live", return_value=False):
            self.assertEqual(claude._serial_text(str(self.root), "second"), "")
            self.assertEqual(codex.inventory_states(self.root, "second")[1:], ([], []))

    def test_release_is_session_scoped_and_next_session_can_claim(self):
        owner.release_owner_for_session(str(self.root), "second")
        self.assertEqual(owner.load_owner(str(self.root))["owner_session_id"], "first")
        owner.release_owner_for_session(str(self.root), "first")
        record = owner.claim_owner(str(self.root), os.getpid(), "second", os.getpid())
        self.assertEqual(record["owner_session_id"], "second")


if __name__ == "__main__":
    unittest.main()
