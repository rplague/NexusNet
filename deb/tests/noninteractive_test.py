#!/usr/bin/env python3
"""Exercise deb/config's noninteractive path without debconf or a TTY."""

import os
import subprocess
import tempfile
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


class NoninteractiveTest(unittest.TestCase):
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
        cls.marker = root / "whiptail-called"
        fake = cls.bin / "whiptail"
        fake.write_text(
            '#!/bin/sh\nprintf called >> "$WHIPTAIL_MARKER"\nexit 42\n',
            encoding="utf-8",
        )
        fake.chmod(0o755)

    @classmethod
    def tearDownClass(cls):
        cls.tmp.cleanup()

    def setUp(self):
        self.db = Path(self.tmp.name) / ("db-" + self._testMethodName)
        self.db.mkdir()
        self.marker.unlink(missing_ok=True)

    def run_config(self, config_path=None, extra_env=None):
        env = os.environ.copy()
        env.update(
            {
                "DEBIAN_FRONTEND": "noninteractive",
                "DEBCONF_RECONFIGURE": "",
                "PATH": str(self.bin) + ":" + env.get("PATH", ""),
                "TEST_DB_STORE": str(self.db),
                "WHIPTAIL_MARKER": str(self.marker),
            }
        )
        env.pop("DEBIAN_HAS_FRONTEND", None)
        if config_path is not None:
            env["TEST_CONFIG"] = str(config_path)
        if extra_env:
            env.update(extra_env)
        result = subprocess.run(
            ["sh", str(self.script)],
            env=env,
            capture_output=True,
            text=True,
            check=False,
            timeout=30,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.marker.exists(), "whiptail must not run")
        return result

    def value(self, key):
        path = self.db / ("nexusnet_" + key)
        return path.read_text(encoding="utf-8") if path.exists() else None

    def preseed(self, key, value):
        (self.db / ("nexusnet_" + key)).write_text(str(value), encoding="utf-8")
        (self.db / ("nexusnet_" + key + ".seen")).write_text(
            "true", encoding="utf-8"
        )

    def test_no_preseed_defaults_to_skip(self):
        self.run_config(FIXTURES / "does-not-exist.toml")
        self.assertEqual(self.value("setup_mode"), "skip")
        self.assertIsNone(self.value("node_name"))

    def test_preseed_wizard_prefills_existing_config(self):
        self.preseed("setup_mode", "wizard")
        self.run_config(FIXTURES / "existing.toml")
        self.assertEqual(self.value("setup_mode"), "wizard")
        self.assertEqual(self.value("node_name"), "edge-01")
        self.assertEqual(self.value("port"), "6123")
        self.assertEqual(self.value("pq_transport"), "true")
        self.assertEqual(self.value("pq_identity"), "true")
        self.assertEqual(self.value("pq_required"), "true")
        self.assertEqual(self.value("local_services_enabled"), "false")
        self.assertEqual(self.value("auth_enabled"), "false")

    def test_preseed_overrides_existing_config(self):
        self.preseed("setup_mode", "wizard")
        self.preseed("node_name", "preset-node")
        self.preseed("port", "7000")
        self.preseed("local_services", "ocr:5014:1")
        self.run_config(FIXTURES / "existing.toml")
        self.assertEqual(self.value("node_name"), "preset-node")
        self.assertEqual(self.value("port"), "7000")
        self.assertEqual(self.value("local_services"), "ocr:5014:1")
        self.assertEqual(self.value("local_services_enabled"), "true")

    def test_existing_services_enable_local_service_mode(self):
        self.preseed("setup_mode", "wizard")
        self.run_config(FIXTURES / "services.toml")
        self.assertEqual(self.value("local_services"), "ocr:5014:1,tts:5015")
        self.assertEqual(self.value("local_services_enabled"), "true")
        self.assertEqual(self.value("auth_enabled"), "false")

    def test_existing_auth_enables_auth_mode(self):
        self.preseed("setup_mode", "wizard")
        self.run_config(FIXTURES / "auth.toml")
        self.assertEqual(self.value("auth_enabled"), "true")
        self.assertEqual(self.value("auth_network"), "corp")
        self.assertEqual(
            self.value("auth_authority"),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=",
        )

    def test_bad_config_falls_back_without_blocking(self):
        self.preseed("setup_mode", "wizard")
        result = self.run_config(FIXTURES / "bad.toml")
        self.assertIn("警告", result.stderr)
        self.assertEqual(self.value("node_name"), "NexusNet")
        self.assertEqual(self.value("port"), "5000")
        self.assertEqual(self.value("pq_transport"), "true")


if __name__ == "__main__":
    unittest.main(verbosity=2)
