"""The cargo test runner (scripts/test-runner.sh) isolates a test binary's
home and fails a run that wrote to the honeypot or changed the real state
or download folder. Each case runs a fake "test binary" (a shell script
under a `deps/` folder) through a copy of the runner, with a fake real
home, and checks the verdict."""

import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

RUNNER = pathlib.Path(__file__).resolve().parents[1] / "test-runner.sh"


class TestRunner(unittest.TestCase):
    def setUp(self):
        self.d = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.d, True)
        # A tree with its own copy of the runner: its honeypot is
        # <tree>/target/test-home, never this repository's.
        (self.d / "tree/scripts").mkdir(parents=True)
        shutil.copy(RUNNER, self.d / "tree/scripts/test-runner.sh")
        self.pots = self.d / "tree/target/test-home"
        # The user's "real" home: a state folder with a history, Downloads.
        self.real = self.d / "real"
        (self.real / ".local/state/inacircle").mkdir(parents=True)
        (self.real / ".local/state/inacircle/history").write_text("inacircle-history 1\n")
        (self.real / "Downloads").mkdir()
        (self.d / "bin/deps").mkdir(parents=True)

    def run_fake(self, body, path="bin/deps/fake-0123"):
        fake = self.d / path
        fake.parent.mkdir(parents=True, exist_ok=True)
        fake.write_text("#!/bin/sh\nset -e\n" + body + "\n")
        fake.chmod(0o755)
        env = {k: v for k, v in os.environ.items() if not k.startswith("XDG_")}
        env.update(HOME=str(self.real), REAL=str(self.real), OUT=str(self.d / "out"))
        return subprocess.run(
            ["bash", str(self.d / "tree/scripts/test-runner.sh"), str(fake)],
            env=env, capture_output=True, text=True, timeout=60,
        )

    def pots_left(self):
        return list(self.pots.iterdir()) if self.pots.exists() else []

    def test_clean_binary_passes_and_sees_only_the_honeypot(self):
        r = self.run_fake('echo "$HOME $XDG_STATE_HOME $XDG_CONFIG_HOME $XDG_RUNTIME_DIR" > "$OUT"')
        self.assertEqual(r.returncode, 0, r.stderr)
        seen = (self.d / "out").read_text().split()
        for p in seen:
            self.assertTrue(p.startswith(str(self.pots)), p)
        self.assertEqual(self.pots_left(), [], "a clean honeypot is removed")

    def test_exit_status_passes_through(self):
        self.assertEqual(self.run_fake("exit 3").returncode, 3)

    def test_history_in_the_default_state_home_fails(self):
        r = self.run_fake('mkdir -p "$XDG_STATE_HOME/inacircle"; echo x > "$XDG_STATE_HOME/inacircle/history"')
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("wrote to the default home", r.stderr)
        self.assertIn("inacircle/history", r.stderr)
        self.assertEqual(len(self.pots_left()), 1, "a dirty honeypot is kept")

    def test_download_fallback_into_the_default_home_fails(self):
        r = self.run_fake('mkdir "$HOME/Downloads/a"; printf "a\\n" > "$HOME/Downloads/a/a.md"')
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Downloads/a/a.md", r.stderr)

    def test_a_removed_skeleton_path_fails(self):
        r = self.run_fake('rmdir "$HOME/Downloads"')
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Downloads", r.stderr)

    def test_a_change_to_the_real_download_folder_fails(self):
        r = self.run_fake('mkdir "$REAL/Downloads/a"')
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("real state or download folder changed", r.stderr)

    def test_a_change_to_the_real_state_folder_fails(self):
        for body in ['chmod 7055 "$REAL/.local/state/inacircle"',
                     'echo "job-start\tjob=5-7" >> "$REAL/.local/state/inacircle/history"']:
            with self.subTest(body=body):
                r = self.run_fake(body)
                os.chmod(self.real / ".local/state/inacircle", 0o700)
                self.assertNotEqual(r.returncode, 0)
                self.assertIn("real state or download folder changed", r.stderr)

    def test_a_failing_binary_keeps_its_own_status(self):
        r = self.run_fake('mkdir "$REAL/Downloads/b"; exit 4')
        self.assertEqual(r.returncode, 4)
        self.assertIn("real state or download folder changed", r.stderr)

    def test_user_dirs_download_dir_is_watched(self):
        (self.real / ".config").mkdir()
        (self.real / ".config/user-dirs.dirs").write_text('XDG_DOWNLOAD_DIR="$HOME/Letoltesek"\n')
        (self.real / "Letoltesek").mkdir()
        r = self.run_fake('touch "$REAL/Letoltesek/x"')
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("Letoltesek", r.stderr)

    def test_other_binaries_run_untouched(self):
        r = self.run_fake('echo "$HOME" > "$OUT"', path="bin/debug/inacircle")
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual((self.d / "out").read_text().strip(), str(self.real))
        self.assertFalse(self.pots.exists())


if __name__ == "__main__":
    unittest.main()
