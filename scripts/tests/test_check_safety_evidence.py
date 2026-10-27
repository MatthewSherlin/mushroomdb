"""Actual checker subprocesses against small page/workflow fixtures."""
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

CHECKER = Path(__file__).resolve().parents[1] / "check-safety-evidence.py"
REFERENCES = [
    ("crash-recovery", "ci.yml", "test"),
    ("format-compatibility", "ci.yml", "format-compat"),
    ("acknowledged-writes", "ci.yml", "test"),
    ("backup-restore", "hosting-templates.yml", "s3-backup"),
    ("snapshot-interruption", "ci.yml", "test"),
]


class SafetyEvidenceTests(unittest.TestCase):
    def check(
        self,
        workflow: str | None = None,
        references: list[tuple[str, str, str]] | None = None,
    ) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / "docs/site").mkdir(parents=True)
            (root / ".github/workflows").mkdir(parents=True)
            page = "\n".join(
                f"<!-- ci-evidence: {claim} .github/workflows/{file}#{job} -->"
                for claim, file, job in (REFERENCES if references is None else references)
            )
            (root / "docs/site/data-safety.md").write_text(page)
            (root / ".github/workflows/ci.yml").write_text(
                "jobs:\n  test:\n    runs-on: ubuntu-latest\n  format-compat:\n    runs-on: ubuntu-latest\n"
                if workflow is None else workflow
            )
            (root / ".github/workflows/hosting-templates.yml").write_text(
                "jobs:\n  s3-backup:\n    runs-on: ubuntu-latest\n"
            )
            result = subprocess.run(
                [sys.executable, str(CHECKER), "--root", str(root)],
                text=True, capture_output=True, check=False,
            )
            return result.returncode, result.stdout + result.stderr

    def test_actual_references_pass(self):
        code, output = self.check()
        self.assertEqual(code, 0, output)
        self.assertIn("5 claims", output)

    def test_removed_or_renamed_job_fails(self):
        for replacement in ["", "  test-renamed:\n    runs-on: ubuntu-latest\n"]:
            with self.subTest(replacement=replacement):
                code, output = self.check("jobs:\n" + replacement + "  format-compat:\n    runs-on: ubuntu-latest\n")
                self.assertEqual(code, 1, output)
                self.assertIn("missing job", output)

    def test_comments_and_step_names_cannot_rescue_missing_job(self):
        for rescue in ["  # test:\n", "    steps:\n      - name: test\n", "    note: |\n      test:\n"]:
            with self.subTest(rescue=rescue):
                code, output = self.check("jobs:\n  format-compat:\n    runs-on: ubuntu-latest\n" + rescue)
                self.assertEqual(code, 1, output)
                self.assertIn("missing job", output)

    def test_missing_claim_or_duplicate_reference_fails(self):
        for references in [REFERENCES[:-1], REFERENCES + [REFERENCES[0]]]:
            with self.subTest(references=references):
                code, output = self.check(references=references)
                self.assertEqual(code, 1, output)

    def test_unknown_workflow_or_malformed_reference_fails(self):
        for file in ["other.yml", "../outside.yml", "ci.yml#extra"]:
            with self.subTest(file=file):
                references = [("crash-recovery", file, "test"), *REFERENCES[1:]]
                code, output = self.check(references=references)
                self.assertEqual(code, 1, output)

    def test_same_job_name_outside_jobs_does_not_count(self):
        code, output = self.check("name: test\n# test:\nnotes:\n  test:\njobs:\n  format-compat:\n    runs-on: ubuntu-latest\n")
        self.assertEqual(code, 1, output)
        self.assertIn("missing job", output)


if __name__ == "__main__":
    unittest.main()
