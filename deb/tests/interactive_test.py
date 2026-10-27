#!/usr/bin/env python3
"""Drive deb/config's whiptail flow with a scripted fake frontend."""

import fcntl
import json
import os
import pty
import struct
import tempfile
import termios
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
CONFIG_SCRIPT = ROOT / "deb" / "config"
FIXTURES = Path(__file__).resolve().parent / "fixtures"

DEBCONF_STUB = r'''
DB_STORE="${TEST_DB_STORE:?}"
mkdir -p "$DB_STORE"
_db_file() { printf '%s/%s' "$DB_STORE" "$(printf '%s' "$1" | tr '/' '_')"; }
db_version() { return 0; }
db_get() {
    _f="$(_db_file "$1")"
    if [ -f "$_f" ]; then RET="$(cat "$_f")"; else RET=""; fi
    return 0
}
db_set() {
    _f="$(_db_file "$1")"
    printf '%s' "$2" > "$_f"
    return 0
}
db_fget() {
    _f="$(_db_file "$1").$2"
    if [ -f "$_f" ]; then RET="$(cat "$_f")"; else RET="false"; fi
    return 0
}
db_fset() {
    _f="$(_db_file "$1").$2"
    printf '%s' "$3" > "$_f"
    return 0
}
db_stop() { return 0; }
'''

FAKE_WHIPTAIL = r'''#!/usr/bin/env python3
import json
import os
import sys

args = sys.argv[1:]
kind = None
for option in ("--menu", "--inputbox", "--yesno", "--msgbox", "--checklist", "--editbox"):
    if option in args:
        kind = option[2:]
        break

responses = [json.loads(line) for line in open(os.environ["FAKE_WT_RESPONSES"]) if line.strip()]
counter_path = os.environ["FAKE_WT_COUNTER"]
try:
    index = int(open(counter_path).read())
except FileNotFoundError:
    index = 0
open(counter_path, "w").write(str(index + 1))

if index >= len(responses):
    sys.stderr.write(f"unexpected {kind} call {index}\n")
    sys.exit(90)
response = responses[index]
if response["kind"] != kind:
    sys.stderr.write(f"expected {response['kind']} got {kind} at {index}\n")
    sys.exit(91)

if kind == "editbox":
    path = args[args.index("--editbox") + 1]
    open(path, "w", encoding="utf-8").write(response.get("write", ""))
    sys.exit(0)
if kind in ("msgbox", "yesno"):
    sys.exit(int(response.get("status", 0)))

fd = 8
if "--output-fd" in args:
    fd = int(args[args.index("--output-fd") + 1])
os.write(fd, response.get("value", "").encode())
sys.exit(int(response.get("status", 0)))
'''


class InteractiveTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tmp = tempfile.TemporaryDirectory()
        root = Path(cls.tmp.name)
        text = CONFIG_SCRIPT.read_text(encoding="utf-8")
        text = text.replace('PKG="@PKG@"', 'PKG="nexusnet"')
        text = text.replace(
            'CONFIG="/etc/${PKG}/config.toml"',
            'CONFIG="${TEST_CONFIG:-/etc/nexusnet/config.toml}"',
        )
        text = text.replace(". /usr/share/debconf/confmodule", DEBCONF_STUB)
        cls.script = root / "config.sh"
        cls.script.write_text(text, encoding="utf-8")

        cls.bin = root / "bin"
        cls.bin.mkdir()
        fake = cls.bin / "whiptail"
        fake.write_text(FAKE_WHIPTAIL, encoding="utf-8")
        fake.chmod(0o755)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def setUp(self):
        suffix = self._testMethodName
        self.dir = Path(self.tmp.name) / suffix
        self.dir.mkdir()
        self.db = self.dir / "db"
        self.db.mkdir()
        self.counter = self.dir / "counter"
        self.responses = self.dir / "responses.jsonl"

    def value(self, key):
        path = self.db / ("nexusnet_" + key)
        return path.read_text(encoding="utf-8") if path.exists() else None

    def set_responses(self, responses):
        self.responses.write_text(
            "\n".join(json.dumps(item) for item in responses) + "\n",
            encoding="utf-8",
        )

    def run_config(self):
        env = os.environ.copy()
        env.update(
            {
                "DEBIAN_FRONTEND": "dialog",
                "DEBCONF_RECONFIGURE": "",
                "PATH": str(self.bin) + ":" + env.get("PATH", ""),
                "TEST_DB_STORE": str(self.db),
                "TEST_CONFIG": str(self.dir / "config.toml"),
                "FAKE_WT_RESPONSES": str(self.responses),
                "FAKE_WT_COUNTER": str(self.counter),
            }
        )
        env.pop("DEBIAN_HAS_FRONTEND", None)

        pid, master = pty.fork()
        if pid == 0:
            try:
                fcntl.ioctl(
                    0,
                    termios.TIOCSWINSZ,
                    struct.pack("HHHH", 24, 80, 0, 0),
                )
            except OSError:
                pass
            os.execvpe("sh", ["sh", str(self.script)], env)

        output = bytearray()
        while True:
            try:
                chunk = os.read(master, 65536)
            except OSError:
                break
            if not chunk:
                break
            output.extend(chunk)
        _, status = os.waitpid(pid, 0)
        os.close(master)
        return os.waitstatus_to_exitcode(status), output.decode("utf-8", "replace")

    def calls(self):
        return int(self.counter.read_text()) if self.counter.exists() else 0

    def test_full_wizard_and_second_run_is_ignored(self):
        self.set_responses(
            [
                {"kind": "menu", "value": "wizard"},
                {"kind": "inputbox", "value": "node-a"},
                {"kind": "inputbox", "value": "desc"},
                {"kind": "menu", "value": "services"},
                {"kind": "menu", "value": "__add__"},
                {"kind": "inputbox", "value": "ocr"},
                {"kind": "inputbox", "value": "7000"},
                {"kind": "menu", "value": "__save__"},
                {"kind": "menu", "value": "auth"},
                {"kind": "inputbox", "value": "corp"},
                {
                    "kind": "inputbox",
                    "value": "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
                },
                {"kind": "checklist", "value": '"ocr"'},
                {"kind": "menu", "value": "next"},
                {
                    "kind": "editbox",
                    "write": "/ip4/1.2.3.4/tcp/5000/ws/p2p/12D3KooWExample\n",
                },
                {"kind": "yesno", "status": 0},
            ]
        )

        status, output = self.run_config()
        self.assertEqual(status, 0, output)
        self.assertEqual(self.calls(), 15)
        self.assertEqual(self.value("setup_mode"), "wizard")
        self.assertEqual(self.value("node_name"), "node-a")
        self.assertEqual(self.value("node_description"), "desc")
        self.assertEqual(self.value("port"), "5000")
        self.assertEqual(self.value("local_services_enabled"), "true")
        self.assertEqual(self.value("local_services"), "ocr:7000:1")
        self.assertEqual(self.value("auth_enabled"), "true")
        self.assertEqual(self.value("auth_network"), "corp")
        self.assertEqual(
            self.value("auth_authority"),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        )
        self.assertEqual(
            self.value("bootstrap"),
            "/ip4/1.2.3.4/tcp/5000/ws/p2p/12D3KooWExample",
        )

        status, output = self.run_config()
        self.assertEqual(status, 0, output)
        self.assertEqual(self.calls(), 15)
        self.assertEqual(self.value("node_name"), "node-a")
        self.assertEqual(self.value("local_services"), "ocr:7000:1")

    def test_empty_bootstrap_and_no_services(self):
        self.set_responses(
            [
                {"kind": "menu", "value": "wizard"},
                {"kind": "inputbox", "value": "node-b"},
                {"kind": "inputbox", "value": "desc"},
                {"kind": "menu", "value": "next"},
                {"kind": "editbox", "write": ""},
                {"kind": "yesno", "status": 0},
            ]
        )

        status, output = self.run_config()
        self.assertEqual(status, 0, output)
        self.assertEqual(self.calls(), 6)
        self.assertEqual(self.value("setup_mode"), "wizard")
        self.assertEqual(self.value("node_name"), "node-b")
        self.assertEqual(self.value("local_services_enabled"), "false")
        self.assertEqual(self.value("local_services"), "")
        self.assertEqual(self.value("auth_enabled"), "false")
        self.assertEqual(self.value("bootstrap"), "")


if __name__ == "__main__":
    unittest.main(verbosity=2)
