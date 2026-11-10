#!/usr/bin/env python3
"""Exercise deb/postinst and deb/postrm with a fake environment."""

import json
import os
import subprocess
import tempfile
import tomllib
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
POSTINST = ROOT / "deb" / "postinst"
POSTRM = ROOT / "deb" / "postrm"
SCHEMA_FILE = ROOT / "config-schema.toml"

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
db_input() { return 0; }
db_subst() { return 0; }
db_go() { return 0; }
db_purge() { : > "${TEST_DB_PURGED:?}"; return 0; }
'''

FAKE_BINARY = r'''#!/bin/sh
printf '%s\n' "$*" >> "${TEST_BIN_LOG:?}"
case "$1" in
  --write-config)
    if [ -n "${TEST_ANSWERS_OUT:-}" ]; then
      cp "$2" "${TEST_ANSWERS_OUT}"
    fi
    _dir=""
    _prev=""
    for _arg in "$@"; do
      [ "$_prev" = "--config-dir" ] && _dir="$_arg"
      _prev="$_arg"
    done
    mkdir -p "$_dir"
    printf '[node]\nname = "written"\n' > "$_dir/config.toml"
    exit "${TEST_WRITE_EXIT:-0}"
    ;;
  --defaults)
    _dir=""
    for _arg in "$@"; do :; done
    _prev=""
    for _arg in "$@"; do
      [ "$_prev" = "--config-dir" ] && _dir="$_arg"
      _prev="$_arg"
    done
    mkdir -p "$_dir"
    printf '[node]\nname = "defaults"\n' > "$_dir/config.toml"
    exit "${TEST_DEFAULTS_EXIT:-0}"
    ;;
  --check-config)
    exit "${TEST_CHECK_EXIT:-0}"
    ;;
esac
exit 0
'''

FAKE_SYSTEMCTL = r'''#!/bin/sh
if [ "$1" = "is-enabled" ]; then
  printf '%s\n' "${TEST_WAS_ENABLED:-disabled}"
fi
exit 0
'''

FAKE_INSTALL = r'''#!/bin/sh
_last=""
for _arg in "$@"; do _last="$_arg"; done
mkdir -p "$_last"
'''

FAKE_NOOP = '#!/bin/sh\nexit 0\n'


class InstallerTest(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.root = Path(self.tmp.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.db = self.root / "db"
        self.db.mkdir()
        self.bin_log = self.root / "bin.log"
        self.db_purged = self.root / "db.purged"
        self.etc = self.root / "etc" / "nexusnet"
        self.var = self.root / "var" / "lib" / "nexusnet"
        (self.root / "var" / "log" / "journal").mkdir(parents=True)
        self.write_fakes()

    def tearDown(self):
        self.tmp.cleanup()

    def write_fakes(self):
        for name, body in {
            "NexusNet": FAKE_BINARY,
            "systemctl": FAKE_SYSTEMCTL,
            "install": FAKE_INSTALL,
            "journalctl": FAKE_NOOP,
            "chown": FAKE_NOOP,
            "chmod": FAKE_NOOP,
            "id": FAKE_NOOP,
            "useradd": FAKE_NOOP,
        }.items():
            path = self.bin / name
            path.write_text(body, encoding="utf-8")
            path.chmod(0o755)

    def render(self, script, current=1, breaking=0):
        text = script.read_text(encoding="utf-8")
        text = text.replace(". /usr/share/debconf/confmodule", DEBCONF_STUB)
        text = text.replace("@USER@", "nexusnet")
        text = text.replace("@PKG@", "nexusnet")
        text = text.replace("@BIN@", "NexusNet")
        text = text.replace("@CONFIG_SCHEMA@", str(current))
        text = text.replace("@BREAKING_BELOW@", str(breaking))
        text = text.replace(
            'CONFIG="/etc/${PKG}/config.toml"',
            f'CONFIG="{self.etc}/config.toml"',
        )
        text = text.replace(
            'CONFIG_SCHEMA_FILE="/etc/${PKG}/config.schema"',
            f'CONFIG_SCHEMA_FILE="{self.etc}/config.schema"',
        )
        text = text.replace(
            'CONFIG_DIR="/etc/${PKG}"',
            f'CONFIG_DIR="{self.etc}"',
        )
        text = text.replace('/etc/"${PKG}"', f'"{self.etc}"')
        text = text.replace('/var/lib/"${PKG}"', f'"{self.var}"')
        text = text.replace('/usr/bin/"${BIN_NAME}"', f'"{self.bin / "NexusNet"}"')
        text = text.replace("/var/log/journal", str(self.root / "var" / "log" / "journal"))
        text = text.replace(
            "command -v systemctl >/dev/null 2>&1 && [ -d /run/systemd/system ]",
            "command -v systemctl >/dev/null 2>&1",
        )
        path = self.root / ("rendered-" + script.name)
        path.write_text(text, encoding="utf-8")
        path.chmod(0o755)
        return path

    def preseed(self, key, value, seen=True):
        (self.db / ("nexusnet_" + key)).write_text(str(value), encoding="utf-8")
        if seen:
            (self.db / ("nexusnet_" + key + ".seen")).write_text("true", encoding="utf-8")

    def env(self, **extra):
        env = os.environ.copy()
        env.update(
            {
                "PATH": str(self.bin) + ":" + env.get("PATH", ""),
                "TEST_DB_STORE": str(self.db),
                "TEST_DB_PURGED": str(self.db_purged),
                "TEST_BIN_LOG": str(self.bin_log),
            }
        )
        env.update(extra)
        return env

    def run_script(self, script, args=None, **extra):
        return subprocess.run(
            ["sh", str(script)] + list(args or []),
            env=self.env(**extra),
            capture_output=True,
            text=True,
            check=False,
            timeout=30,
        )

    def bin_calls(self):
        return self.bin_log.read_text(encoding="utf-8").splitlines() if self.bin_log.exists() else []

    def test_wizard_writes_config_and_stamps_schema(self):
        self.preseed("setup_mode", "wizard")
        self.preseed("node_name", "node-a")
        self.preseed("port", "5000")
        result = self.run_script(self.render(POSTINST))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any("--write-config" in line for line in self.bin_calls()))
        self.assertEqual((self.etc / "config.schema").read_text().strip(), "1")

    def test_wizard_keeps_service_flags_when_auth_disabled(self):
        self.preseed("setup_mode", "wizard")
        self.preseed("auth_enabled", "false")
        self.preseed("local_services_enabled", "true")
        self.preseed("local_services", "ocr:5014:1:0")
        answers = self.root / "answers.json"
        result = self.run_script(self.render(POSTINST), TEST_ANSWERS_OUT=str(answers))
        self.assertEqual(result.returncode, 0, result.stderr)
        data = json.loads(answers.read_text(encoding="utf-8"))
        service = data["services"]["dispatcher"]["local_services"][0]
        self.assertTrue(service["require_auth"])
        self.assertFalse(service["publish"])
        self.assertEqual(data["auth"]["network"], "none")

    def test_compatible_skip_checks_and_stamps_without_rewriting(self):
        self.etc.mkdir(parents=True)
        (self.etc / "config.toml").write_text('[node]\nname = "old"\n', encoding="utf-8")
        (self.etc / "config.schema").write_text("1\n", encoding="utf-8")
        self.preseed("setup_mode", "skip")
        result = self.run_script(self.render(POSTINST, current=1, breaking=1))
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.bin_calls()
        self.assertTrue(any("--check-config" in line for line in calls))
        self.assertFalse(any("--write-config" in line for line in calls))

    def test_breaking_skip_disables_without_writing(self):
        self.etc.mkdir(parents=True)
        (self.etc / "config.toml").write_text('[node]\nname = "old"\n', encoding="utf-8")
        (self.etc / "config.schema").write_text("1\n", encoding="utf-8")
        self.preseed("setup_mode", "skip")
        result = self.run_script(self.render(POSTINST, current=2, breaking=2))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any("--write-config" in line for line in self.bin_calls()))
        self.assertEqual((self.etc / "config.schema").read_text().strip(), "1")

    def test_downgrade_schema_is_blocked(self):
        self.etc.mkdir(parents=True)
        (self.etc / "config.toml").write_text('[node]\nname = "new"\n', encoding="utf-8")
        (self.etc / "config.schema").write_text("3\n", encoding="utf-8")
        self.preseed("setup_mode", "skip")
        result = self.run_script(self.render(POSTINST, current=2, breaking=0))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(any("--check-config" in line for line in self.bin_calls()))

    def test_breaking_wizard_backs_up_and_stamps_new_schema(self):
        self.etc.mkdir(parents=True)
        (self.etc / "config.toml").write_text('[node]\nname = "old"\n', encoding="utf-8")
        (self.etc / "config.schema").write_text("1\n", encoding="utf-8")
        self.preseed("setup_mode", "wizard")
        result = self.run_script(self.render(POSTINST, current=2, breaking=2))
        self.assertEqual(result.returncode, 0, result.stderr)
        calls = self.bin_calls()
        self.assertTrue(any("--write-config" in line and "--backup" in line for line in calls))
        self.assertEqual((self.etc / "config.schema").read_text().strip(), "2")

    def test_fresh_skip_writes_defaults_and_stamps_schema(self):
        self.preseed("setup_mode", "skip")
        result = self.run_script(self.render(POSTINST))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(any("--defaults" in line for line in self.bin_calls()))
        self.assertEqual((self.etc / "config.schema").read_text().strip(), "1")

    def test_write_failure_aborts(self):
        self.preseed("setup_mode", "wizard")
        result = self.run_script(self.render(POSTINST), TEST_WRITE_EXIT="2")
        self.assertNotEqual(result.returncode, 0)

    def test_purge_removes_config_and_purges_debconf_but_keeps_keypair(self):
        self.etc.mkdir(parents=True)
        (self.etc / "config.toml").write_text("[node]\n", encoding="utf-8")
        (self.etc / "config.schema").write_text("1\n", encoding="utf-8")
        (self.etc / "config.toml.bak.1").write_text("old", encoding="utf-8")
        self.var.mkdir(parents=True)
        (self.var / "keypair.bin").write_text("secret", encoding="utf-8")
        result = self.run_script(self.render(POSTRM), ["purge"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(self.db_purged.exists())
        self.assertFalse((self.etc / "config.toml").exists())
        self.assertFalse((self.etc / "config.schema").exists())
        self.assertFalse((self.etc / "config.toml.bak.1").exists())
        self.assertTrue((self.var / "keypair.bin").exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
