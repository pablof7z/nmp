from __future__ import annotations

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


SKILL_DIR = Path(__file__).resolve().parents[1]
REPO_ROOT = Path(__file__).resolve().parents[3]
VALIDATOR = Path("scripts/validate_skill.py")


class SkillValidationTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.skill = self.root / "nmp"
        shutil.copytree(SKILL_DIR, self.skill)

    def tearDown(self) -> None:
        self.temp.cleanup()

    def run_validator(
        self,
        *extra: str,
        env: dict[str, str] | None = None,
        use_official: bool = False,
    ) -> subprocess.CompletedProcess[str]:
        command = [
            "python3",
            str(self.skill / VALIDATOR),
            str(self.skill),
            "--repo-root",
            str(REPO_ROOT),
        ]
        if not use_official:
            command.append("--test-skip-official")
        command.extend(extra)
        return subprocess.run(
            command,
            text=True,
            capture_output=True,
            check=False,
            env=env,
        )

    def replace(self, relative: str, old: str, new: str) -> None:
        path = self.skill / relative
        text = path.read_text(encoding="utf-8")
        self.assertIn(old, text)
        path.write_text(text.replace(old, new, 1), encoding="utf-8")

    def assert_rejected(self, result: subprocess.CompletedProcess[str], message: str) -> None:
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn(message, result.stderr)

    def test_canonical_package_passes_bundled_and_official_validation(self) -> None:
        result = self.run_validator(use_official=True)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_invalid_name_is_rejected(self) -> None:
        self.replace("SKILL.md", "name: nmp", "name: NMP")
        self.assert_rejected(self.run_validator(), "name must be lowercase hyphen-case")

    def test_broken_local_link_is_rejected(self) -> None:
        self.replace(
            "SKILL.md",
            "references/queries.md",
            "references/does-not-exist.md",
        )
        self.assert_rejected(self.run_validator(), "broken link")

    def test_local_link_outside_skill_root_is_rejected(self) -> None:
        self.replace(
            "SKILL.md",
            "## Non-negotiable guardrails",
            "[host file](/etc/passwd)\n\n## Non-negotiable guardrails",
        )
        self.assert_rejected(self.run_validator(), "local link escapes skill root")

    def test_nonexistent_declared_source_is_rejected(self) -> None:
        self.replace(
            "references/source-map.md",
            "- Source: `README.md`",
            "- Source: `does/not/exist.rs`",
        )
        self.assert_rejected(self.run_validator(), "declared source does not exist")

    def test_declared_source_outside_repository_is_rejected(self) -> None:
        self.replace(
            "references/source-map.md",
            "- Source: `README.md`",
            "- Source: `/etc/passwd`",
        )
        self.assert_rejected(self.run_validator(), "declared source escapes repository root")

    def test_mismatched_verified_revisions_are_rejected(self) -> None:
        skill_text = (self.skill / "SKILL.md").read_text(encoding="utf-8")
        revision = re.search(r"Verified-Revision: `([0-9a-f]{40})`", skill_text)
        self.assertIsNotNone(revision)
        self.replace(
            "SKILL.md",
            revision.group(1),
            "0000000000000000000000000000000000000000",
        )
        self.assert_rejected(self.run_validator(), "Verified-Revision pins do not match")

    def test_missing_official_validator_is_fatal(self) -> None:
        empty_home = self.root / "empty-home"
        empty_codex = self.root / "empty-codex"
        empty_home.mkdir()
        empty_codex.mkdir()
        env = dict(os.environ)
        env["HOME"] = str(empty_home)
        env["CODEX_HOME"] = str(empty_codex)
        result = self.run_validator(env=env, use_official=True)
        self.assert_rejected(result, "official skill-creator quick_validate.py not found")

    def test_explicit_test_bypass_allows_missing_official_validator(self) -> None:
        empty_home = self.root / "empty-home"
        empty_codex = self.root / "empty-codex"
        empty_home.mkdir()
        empty_codex.mkdir()
        env = dict(os.environ)
        env["HOME"] = str(empty_home)
        env["CODEX_HOME"] = str(empty_codex)
        result = self.run_validator(env=env)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("explicitly bypassed for tests", result.stderr)

    def test_missing_default_prompt_is_rejected(self) -> None:
        path = self.skill / "agents/openai.yaml"
        lines = [line for line in path.read_text(encoding="utf-8").splitlines() if "default_prompt" not in line]
        path.write_text("\n".join(lines) + "\n", encoding="utf-8")
        self.assert_rejected(self.run_validator(), "must contain exactly")

    def test_default_prompt_must_name_skill(self) -> None:
        self.replace("agents/openai.yaml", "$nmp", "the skill")
        self.assert_rejected(self.run_validator(), "default_prompt must explicitly name $nmp")

    def test_short_description_length_is_rejected(self) -> None:
        self.replace(
            "agents/openai.yaml",
            "Build source-accurate NMP app integrations",
            "Too short",
        )
        self.assert_rejected(self.run_validator(), "short_description must be 25-64 characters")

    def test_prompt_specific_answer_criteria_are_rejected(self) -> None:
        path = self.skill / "references/evaluation-prompts.md"
        path.write_text(path.read_text(encoding="utf-8") + "\nPass if the answer copies this.\n", encoding="utf-8")
        self.assert_rejected(self.run_validator(), "raw evaluation prompts leak answer criteria")


if __name__ == "__main__":
    unittest.main()
