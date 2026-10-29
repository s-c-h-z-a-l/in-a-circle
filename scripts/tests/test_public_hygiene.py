"""scripts/check-public-hygiene.sh: it fails on internal labels, passes clean."""
import pathlib, shutil, subprocess, tempfile, unittest

SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "check-public-hygiene.sh"


def run(repo):
    return subprocess.run(
        ["bash", "scripts/check-public-hygiene.sh"],
        cwd=repo, capture_output=True, text=True, timeout=30,
    )


class Hygiene(unittest.TestCase):
    def setUp(self):
        self.d = pathlib.Path(tempfile.mkdtemp(prefix="hygiene-"))
        self.addCleanup(shutil.rmtree, self.d, True)
        (self.d / "scripts").mkdir()
        shutil.copy(SCRIPT, self.d / "scripts")
        (self.d / "crates").mkdir()
        subprocess.run(["git", "init", "-q"], cwd=self.d, check=True)

    def add(self, text):
        (self.d / "crates" / "a.rs").write_text(text)
        subprocess.run(["git", "add", "-A"], cwd=self.d, check=True)

    def test_clean_tree_passes(self):
        self.add("// the budget is 5 MB because of D-Bus and UTF-8\n")
        self.assertEqual(run(self.d).returncode, 0)

    def test_each_internal_label_fails(self):
        for bad in ("// decision D-29\n", "// the T13a helper\n", "// U-01 open\n",
                    "// G0-P row\n", "// ~/inacircle-teszt/x\n", "// T16 A-I1\n"):
            with self.subTest(bad=bad):
                self.add(bad)
                r = run(self.d)
                self.assertEqual(r.returncode, 1, r.stdout + r.stderr)
                self.assertIn("a.rs", r.stderr)

    def test_self_test_passes(self):
        r = subprocess.run(["bash", str(SCRIPT), "--self-test"], capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stderr)


if __name__ == "__main__":
    unittest.main()
