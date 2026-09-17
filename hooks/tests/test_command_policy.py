"""Shared lifecycle policy and harness output contracts, without hardware."""

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[2]


class CommandPolicyTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.project = Path(self.temp.name)
        (self.project / '.target.jsonc').write_text('{}')
        (self.project / '.dut-serial').mkdir()

    def run_hook(self, command, harness='claude', hook=None):
        result = subprocess.run(
            [sys.executable, str(hook or ROOT / 'hooks' / harness / 'pre-tool-use.py')],
            input=json.dumps({'tool_name': 'Bash', 'tool_input': {'command': command},
                              'cwd': str(self.project)}),
            text=True, capture_output=True, cwd=self.project, check=True,
            env={**os.environ, 'TARGET_CONF': str(self.project / '.target.jsonc')},
        )
        return json.loads(result.stdout)

    def denied(self, output):
        return output.get('hookSpecificOutput', {}).get('permissionDecision') == 'deny'

    def test_human_only_commands_and_wrappers(self):
        for harness in ('claude', 'codex'):
            for command in (
                'dutabo stop', 'dutabo resume', 'dutabo serial', 'dutabo start',
                'env A=b /tmp/dutabo -p 3001 stop',
                'bash -lc "nohup dutabo resume"',
                'dutabo status; dutabo stop',
                'cargo run --bin dutabo -- stop',
                'cargo run --bin=dutabo -- resume',
                'dutabo --dut board serial',
                'if dutabo stop; then :; fi',
                'if true; then dutabo resume; fi',
                'while dutabo stop; do break; done',
                'dutabo stop --dut --agent',
                'rm -f .dut-serial/stopped',
                'cat .dut-serial/control-3001.token',
                'TARGET_CONF=/other/project/.target.jsonc dutabo stop --agent',
            ):
                with self.subTest(harness=harness, command=command):
                    output = self.run_hook(command, harness)
                    self.assertTrue(self.denied(output), output)
                    if harness == 'codex':
                        self.assertNotIn('continue', output)
                    else:
                        self.assertIs(output['continue'], True)

    def test_persistent_start_and_agent_stop_allowed(self):
        for harness in ('claude', 'codex'):
            for command in ('dutabo start --agent', 'dutabo stop --agent', 'sermcp --http --persistent',
                            'sermcp --http --idle-timeout 0',
                            'nohup sermcp --http --idle-timeout=0 &',
                            'cargo run --bin dutabo -- stop --agent',
                            'if dutabo stop --agent; then :; fi',
                            'kill -TERM 12345', 'echo "dutabo stop"', 'dutabo status'):
                with self.subTest(harness=harness, command=command):
                    self.assertFalse(self.denied(self.run_hook(command, harness)))

    def test_manual_stop_inhibits_all_agent_start_paths_until_human_clears(self):
        marker = self.project / '.dut-serial/stopped'
        marker.write_text('human stop')
        for harness in ('claude', 'codex'):
            for command in ('sermcp --http --idle-timeout 0', 'dutabo start', 'dutabo start --agent',
                            'cargo run --bin dutabo -- start --agent',
                            'bash -lc "dutabo -p 3001 start --agent"'):
                with self.subTest(harness=harness, command=command):
                    self.assertTrue(self.denied(self.run_hook(command, harness)))
                    self.assertEqual(marker.read_text(), 'human stop')
        marker.unlink()  # Simulate the human CLI clearing the inhibit.
        self.assertFalse(self.denied(self.run_hook('dutabo start --agent')))

    def test_copied_codex_adapter_uses_bundled_shared_policy(self):
        installed = self.project / '.codex/hooks'
        installed.mkdir(parents=True)
        shutil.copy(ROOT / 'hooks/codex/pre-tool-use.py', installed)
        shutil.copytree(ROOT / 'hooks/claude', installed / 'claude')
        output = self.run_hook('dutabo stop', hook=installed / 'pre-tool-use.py')
        self.assertTrue(self.denied(output))
        self.assertNotIn('continue', output)

if __name__ == '__main__':
    unittest.main()
