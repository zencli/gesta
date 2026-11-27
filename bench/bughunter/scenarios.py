#!/usr/bin/env python3
"""Black-box regression scenarios; no network, real credentials, or user data."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest


BINARY = None
TIMEOUT = 60


class LocalScenarios(unittest.TestCase):
    """Each scenario owns its repository, home, and outside-boundary sentinel."""

    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="gesta-bughunter-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name)
        self.repo = self.base / "repo"
        self.repo.mkdir()
        self.home = self.base / "home"
        self.home.mkdir()
        self.outside = self.base / "outside"
        self.outside.mkdir()
        self.canary = self.outside / "canary.txt"
        self.canary.write_bytes(b"OUTSIDE-BOUNDARY-CANARY\n")
        # Do not inherit real credentials, hooks, remote endpoints, or user config.
        self.env = {
            "PATH": os.defpath,
            "HOME": str(self.home),
            "XDG_CONFIG_HOME": str(self.home / "config"),
            "XDG_DATA_HOME": str(self.home / "data"),
            "TMPDIR": str(self.base),
            "LANG": "C.UTF-8",
            "LC_ALL": "C.UTF-8",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_TERMINAL_PROMPT": "0",
            # A backup is sealed, so the backup scenarios need a passphrase (#340). It is a fixture
            # value in a throwaway HOME, not a real credential.
            "GESTA_BACKUP_PASSPHRASE": "bughunter-fixture-passphrase",
        }
        self.ok("init")
        self.tracked = self.repo / "tracked.txt"
        self.tracked.write_bytes(b"committed baseline\n")
        self.commit("baseline")

    def invoke(self, *args):
        return subprocess.run(
            [str(BINARY), *map(str, args)],
            cwd=self.repo,
            env=self.env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            timeout=TIMEOUT,
            check=False,
        )

    def ok(self, *args):
        result = self.invoke(*args)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return result.stdout

    def refused(self, *args):
        result = self.invoke(*args)
        self.assertGreater(result.returncode, 0, result.stdout + result.stderr)
        self.assertTrue((result.stdout + result.stderr).strip(), "empty refusal")
        return result

    def snapshot(self, message, *args):
        value = json.loads(self.ok("changeset", "create", message, "--json", *args))
        self.assertEqual(value["schema"], "gesta@1")
        self.assertTrue(value["changed"])
        self.assertTrue(value["changeset"])
        return value

    def commit(self, message):
        value = self.snapshot(message)
        self.ok("changeset", "commit", value["changeset"])
        return value

    def preserved(self):
        self.assertEqual(self.canary.read_bytes(), b"OUTSIDE-BOUNDARY-CANARY\n")
        self.ok("fsck")

    def test_snapshot_file_symlink_records_link_not_outside_content(self):
        (self.repo / "link").symlink_to(self.canary)
        self.commit("record symbolic link")
        stored = self.ok("show", "main:link")
        self.assertIn(str(self.canary), stored)
        self.assertNotIn("OUTSIDE-BOUNDARY-CANARY", stored)
        self.preserved()

    def test_snapshot_directory_symlink_does_not_walk_outside(self):
        (self.repo / "linked-dir").symlink_to(self.outside, target_is_directory=True)
        value = self.snapshot("directory boundary")
        paths = [entry["path"] for entry in value["files"]]
        self.assertFalse(any(path.startswith("linked-dir/") for path in paths), paths)
        diff = self.ok("changeset", "diff", value["changeset"])
        self.assertNotIn("OUTSIDE-BOUNDARY-CANARY", diff)
        self.preserved()

    def test_checkout_does_not_overwrite_external_symlink_target(self):
        self.tracked.unlink()
        self.tracked.symlink_to(self.canary)
        self.refused("checkout", "tracked.txt")
        self.preserved()

    def test_checkout_does_not_overwrite_external_hardlink(self):
        self.tracked.unlink()
        os.link(self.canary, self.tracked)
        self.refused("checkout", "tracked.txt")
        self.preserved()

    def test_checkout_parent_symlink_cannot_write_outside(self):
        folder = self.repo / "nested"
        folder.mkdir()
        (folder / "canary.txt").write_bytes(b"tracked nested file\n")
        self.commit("nested file")
        (folder / "canary.txt").unlink()
        folder.rmdir()
        folder.symlink_to(self.outside, target_is_directory=True)
        self.refused("checkout", "nested/canary.txt")
        self.preserved()

    def test_restore_parent_traversal_refuses_without_discarding_edits(self):
        self.tracked.write_bytes(b"uncommitted edit\n")
        self.refused("restore", "../outside/canary.txt")
        self.assertEqual(self.tracked.read_bytes(), b"uncommitted edit\n")
        self.preserved()

    def test_scoped_snapshot_cannot_include_parent_path(self):
        # --path is documented as a filter of walked repository paths, not a
        # file-opening API. An unmatched filter may safely return a no-op.
        self.tracked.write_bytes(b"unrelated edit must not be staged\n")
        before_log = self.ok("log", "--json")
        before_pending = self.ok("changeset", "list", "--json")
        result = self.invoke("changeset", "create", "outside path", "--path", "../outside/canary.txt")
        self.assertGreaterEqual(result.returncode, 0, "process crashed")
        self.assertEqual(self.ok("log", "--json"), before_log)
        self.assertEqual(self.ok("changeset", "list", "--json"), before_pending)
        self.assertEqual(self.tracked.read_bytes(), b"unrelated edit must not be staged\n")
        self.preserved()

    def test_unknown_checkout_revision_leaves_edit_intact(self):
        self.tracked.write_bytes(b"must survive invalid revision\n")
        before = self.ok("log", "--json")
        self.refused("checkout", ".", "--changeset", "missing-bughunter-revision")
        self.assertEqual(self.tracked.read_bytes(), b"must survive invalid revision\n")
        self.assertEqual(self.ok("log", "--json"), before)
        self.preserved()

    def test_repository_metadata_is_not_snapshotted(self):
        (self.repo / ".gesta" / "private-canary").write_bytes(b"DO-NOT-SNAPSHOT-METADATA")
        self.tracked.write_bytes(b"positive snapshot control\n")
        value = self.snapshot("metadata boundary")
        paths = [entry["path"] for entry in value["files"]]
        self.assertIn("tracked.txt", paths)
        self.assertFalse(any(path == ".gesta" or path.startswith(".gesta/") for path in paths), paths)
        self.assertNotIn("DO-NOT-SNAPSHOT-METADATA", self.ok("changeset", "diff", value["changeset"]))
        self.preserved()

    def test_invalid_backup_is_rejected_without_replacing_store(self):
        backup = self.base / "invalid.db"
        backup.write_bytes(b"not a SQLite database\n")
        before = self.ok("log", "--json")
        self.refused("verify-backup", backup)
        self.refused("restore-repo", backup, "--yes")
        self.assertEqual(self.ok("log", "--json"), before)
        self.assertEqual(self.ok("show", "main:tracked.txt"), "committed baseline\n")
        self.preserved()

    def test_valid_backup_restores_original_history(self):
        backup = self.base / "valid.db"
        before = self.ok("log", "--json")
        self.ok("backup", backup)
        self.ok("verify-backup", backup)
        self.tracked.write_bytes(b"second revision\n")
        self.commit("second revision")
        self.assertNotEqual(self.ok("log", "--json"), before)
        self.ok("restore-repo", backup, "--yes")
        self.assertEqual(self.ok("log", "--json"), before)
        self.assertEqual(self.ok("show", "main:tracked.txt"), "committed baseline\n")
        self.preserved()

    def test_shell_metacharacters_in_filename_remain_literal(self):
        name = "$(touch BUG_HUNTER_EXECUTED); literal.txt"
        file = self.repo / name
        file.write_bytes(b"literal filename content\n")
        self.commit("literal filename")
        self.assertEqual(self.ok("show", "main:" + name), "literal filename content\n")
        file.unlink()
        self.ok("checkout", name)
        self.assertEqual(file.read_bytes(), b"literal filename content\n")
        self.assertFalse((self.repo / "BUG_HUNTER_EXECUTED").exists())
        self.preserved()


def main():
    global BINARY
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("checkout", type=Path)
    args = parser.parse_args()
    checkout = args.checkout.resolve()
    override = os.environ.get("GESTA_BIN")
    BINARY = Path(override).resolve() if override else checkout / "target/release/gesta"
    if not BINARY.is_file() or not os.access(BINARY, os.X_OK):
        parser.error("executable gesta binary not found: " + str(BINARY))
    if os.name != "posix":
        parser.error("these filesystem scenarios require a POSIX test container")
    print("Scope: disposable local fixtures only; no network or real credentials", flush=True)
    print("Binary: " + str(BINARY), flush=True)
    print("Binary SHA256: " + hashlib.sha256(BINARY.read_bytes()).hexdigest(), flush=True)
    suite = unittest.defaultTestLoader.loadTestsFromTestCase(LocalScenarios)
    expected = suite.countTestCases()
    if expected == 0:
        print("FAIL: no scenarios discovered", file=sys.stderr)
        return 1
    result = unittest.TextTestRunner(verbosity=2, stream=sys.stdout).run(suite)
    passed = result.testsRun - len(result.failures) - len(result.errors) - len(result.skipped)
    print(f"{passed}/{expected} bughunter scenarios pass; skipped={len(result.skipped)}", flush=True)
    return 0 if result.wasSuccessful() and result.testsRun == expected and not result.skipped else 1


if __name__ == "__main__":
    raise SystemExit(main())
