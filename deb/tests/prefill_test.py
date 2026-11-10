#!/usr/bin/env python3
"""Exercise the Python prefill block embedded in deb/config."""

import json
import shlex
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONFIG_SCRIPT = ROOT / "deb" / "config"
FIXTURES = Path(__file__).resolve().parent / "fixtures"
BEGIN = "# >>> nexusnet-prefill"
END = "# <<< nexusnet-prefill"


def extract_prefill() -> str:
    text = CONFIG_SCRIPT.read_text(encoding="utf-8")
    start = text.index(BEGIN) + len(BEGIN)
    stop = text.index(END)
    return text[start:stop].lstrip("\n")


class PrefillTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        cls.script = Path(cls.tmp.name) / "prefill.py"
        cls.script.write_text(extract_prefill(), encoding="utf-8")

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def run_prefill(self, path=None, as_json=True):
        args = [sys.executable, str(self.script)]
        if as_json:
            args.append("--json")
        if path is not None:
            args.append(str(path))
        return subprocess.run(args, capture_output=True, text=True, check=True)

    def prefill(self, path=None):
        result = self.run_prefill(path)
        return json.loads(result.stdout)

    def test_no_config_uses_defaults(self):
        data = self.prefill(FIXTURES / "does-not-exist.toml")
        self.assertEqual(data["CONFIG_FOUND"], "false")
        self.assertEqual(data["NODE_NAME"], "NexusNet")
        self.assertEqual(data["NODE_DESCRIPTION"], "NexusNet P2P node")
        self.assertEqual(data["PORT"], "5000")
        self.assertEqual(data["LOCAL_SERVICES"], "")
        self.assertEqual(data["AUTH_NETWORK"], "none")
        self.assertEqual(data["AUTH_AUTHORITY"], "")
        self.assertEqual(data["BOOTSTRAP"], "")
        self.assertEqual(data["PQ_REQUIRED"], "false")

    def test_existing_config_is_prefilled(self):
        data = self.prefill(FIXTURES / "existing.toml")
        self.assertEqual(data["CONFIG_FOUND"], "true")
        self.assertEqual(data["NODE_NAME"], "edge-01")
        self.assertEqual(data["NODE_DESCRIPTION"], "Edge node")
        self.assertEqual(data["PORT"], "6123")
        self.assertEqual(
            data["BOOTSTRAP"],
            "/ip4/1.2.3.4/tcp/5000/ws/p2p/12D3KooWExample",
        )
        self.assertEqual(data["PQ_TRANSPORT"], "true")
        self.assertEqual(data["PQ_IDENTITY"], "true")
        self.assertEqual(data["PQ_REQUIRED"], "true")

    def test_local_services_and_auth_flags(self):
        data = self.prefill(FIXTURES / "services.toml")
        self.assertEqual(data["LOCAL_SERVICES"], "ocr:5014:1:1,tts:5015:0:0")

    def test_auth_network_is_prefilled(self):
        data = self.prefill(FIXTURES / "auth.toml")
        self.assertEqual(data["AUTH_NETWORK"], "corp")
        self.assertEqual(
            data["AUTH_AUTHORITY"],
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        )

    def test_bad_toml_warns_and_falls_back(self):
        result = self.run_prefill(FIXTURES / "bad.toml")
        data = json.loads(result.stdout)
        self.assertIn("警告", result.stderr)
        self.assertEqual(data["CONFIG_FOUND"], "false")
        self.assertEqual(data["NODE_NAME"], "NexusNet")
        self.assertEqual(data["PORT"], "5000")

    def test_shell_assignment_output_is_quoted(self):
        result = self.run_prefill(FIXTURES / "auth.toml", as_json=False)
        assignments = {}
        for line in result.stdout.splitlines():
            key, value = line.split("=", 1)
            parts = shlex.split(value)
            self.assertEqual(len(parts), 1)
            assignments[key] = parts[0]
        self.assertEqual(assignments["AUTH_NETWORK"], "corp")
        self.assertEqual(assignments["NODE_NAME"], "NexusNet")


if __name__ == "__main__":
    unittest.main(verbosity=2)
