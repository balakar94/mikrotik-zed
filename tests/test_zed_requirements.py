"""Tests for the Zed requirements checker (scripts/check_zed_requirements.py).

Why this exists: Zed deserializes extension.toml into its ExtensionManifest
struct with serde defaults — unknown keys are silently ignored. A typo like
`categories = [...]` or a field removed upstream would never fail locally and
only surface at registry review. These tests pin the local hard gate that
makes such drift fatal (`make check-manifest`, wired into
`make validate`).
"""

import importlib.util
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).parent.parent
SCRIPT_PATH = ROOT / "scripts" / "check_zed_requirements.py"


def _load_script_module():
    """Load the checker script as a module (it must be side-effect free)."""
    assert SCRIPT_PATH.is_file(), f"missing {SCRIPT_PATH}"
    spec = importlib.util.spec_from_file_location("check_zed_requirements_under_test", SCRIPT_PATH)
    assert spec is not None and spec.loader is not None, f"cannot build import spec for {SCRIPT_PATH}"
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


zed_req = _load_script_module()

# Minimal known-valid manifest + language tree used as the base for every
# negative fixture: each test mutates exactly one aspect of it.
VALID_MANIFEST = """\
id = "acme-widget"
name = "Acme Widget"
version = "0.1.0"
schema_version = 1
authors = ["someone"]
description = "Test fixture."
repository = "https://github.com/acme/widget"
languages = ["languages/wg"]

[grammars.wg]
repository = "https://github.com/acme/tree-sitter-wg"
rev = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"

[language_servers.wg-ls]
name = "Widget Language Server"
languages = ["Widget"]
"""
LANGUAGE_CONFIG = 'name = "Widget"\ngrammar = "wg"\n'


def _make_repo(tmp_path: Path, manifest_text: str = VALID_MANIFEST) -> None:
    """Materialize a minimal valid extension repo under tmp_path."""
    lang_dir = tmp_path / "languages" / "wg"
    lang_dir.mkdir(parents=True)
    (lang_dir / "config.toml").write_text(LANGUAGE_CONFIG, encoding="utf-8")
    (tmp_path / "extension.toml").write_text(manifest_text, encoding="utf-8")


def _validate_text(tmp_path: Path, manifest_text: str) -> list[str]:
    _make_repo(tmp_path, manifest_text)
    manifest = zed_req.load_manifest(tmp_path / "extension.toml")
    return zed_req.validate(manifest, tmp_path)


class TestRealManifestPasses:
    def test_real_extension_toml_fully_valid(self):
        """The shipped extension.toml must pass every offline check.

        Guards against the gate itself drifting away from the repo (renamed
        language dir, new top-level key nobody whitelisted): if this fails,
        `make validate` would block every developer at the first step.
        """
        manifest = zed_req.load_manifest(ROOT / "extension.toml")
        violations = zed_req.validate(manifest, ROOT)
        assert violations == [], f"real extension.toml has violations: {violations}"


class TestNegativeFixtures:
    """Each fixture mutates one aspect of a known-valid mini-repo and asserts
    the targeted rule fires with an identifiable message."""

    def test_unknown_top_level_key_is_rejected(self, tmp_path):
        # `categories` is not in ExtensionManifest; upstream serde would drop it silently.
        violations = _validate_text(tmp_path, VALID_MANIFEST + 'categories = ["other"]\n')
        assert any("unknown" in v and "'categories'" in v for v in violations), violations

    def test_id_containing_zed_is_rejected(self, tmp_path):
        # Zed registry policy: id must not contain "zed".
        text = VALID_MANIFEST.replace('id = "acme-widget"', 'id = "acme-zed-widget"')
        violations = _validate_text(tmp_path, text)
        assert any("'zed'" in v for v in violations), violations

    def test_non_kebab_case_id_is_rejected(self, tmp_path):
        text = VALID_MANIFEST.replace('id = "acme-widget"', 'id = "Acme Widget!"')
        violations = _validate_text(tmp_path, text)
        assert any("kebab-case" in v for v in violations), violations

    def test_schema_version_2_is_rejected(self, tmp_path):
        text = VALID_MANIFEST.replace("schema_version = 1", "schema_version = 2")
        violations = _validate_text(tmp_path, text)
        assert any("schema_version" in v for v in violations), violations

    def test_language_server_referencing_unknown_language_is_rejected(self, tmp_path):
        # R9 binds server `languages` entries to real config.toml names (R7).
        text = VALID_MANIFEST.replace('languages = ["Widget"]', 'languages = ["Not A Language"]')
        violations = _validate_text(tmp_path, text)
        assert any(
            "does not match any language" in v and "Not A Language" in v for v in violations
        ), violations

    def test_short_grammar_rev_is_rejected(self, tmp_path):
        # R8: revs are full 40-char lowercase git SHAs, never short hashes.
        text = VALID_MANIFEST.replace("a" * 40, "deadbeef")
        violations = _validate_text(tmp_path, text)
        assert any(".rev" in v and "40" in v for v in violations), violations

    def test_missing_required_key_is_rejected(self, tmp_path):
        text = VALID_MANIFEST.replace('description = "Test fixture."\n', "")
        violations = _validate_text(tmp_path, text)
        assert any("missing required" in v and "description" in v for v in violations), violations


class TestWhitelistRegressionPin:
    def test_whitelist_never_grows_homepage_or_categories(self):
        """Pin fields that historically tempted manifest drift.

        Neither `homepage` nor `categories` is part of ExtensionManifest. If
        they ever appear in KNOWN_TOP_LEVEL_KEYS, the gate stops matching
        upstream serde and typos pass silently again.
        """
        for banned in ("homepage", "categories"):
            assert banned not in zed_req.KNOWN_TOP_LEVEL_KEYS, (
                f"'{banned}' must not be accepted as a top-level manifest key"
            )

    def test_whitelist_matches_upstream_struct_exactly(self):
        """The whitelist must equal ExtensionManifest's fields — no more, no less.

        Extra entries weaken the typo gate; missing entries reject manifests
        Zed itself accepts. Update both sides together when upstream changes.
        """
        expected = {
            "id",
            "name",
            "version",
            "schema_version",
            "description",
            "repository",
            "authors",
            "lib",
            "themes",
            "icon_themes",
            "languages",
            "grammars",
            "language_servers",
            "context_servers",
            "slash_commands",
            "snippets",
            "capabilities",
            "debug_adapters",
            "debug_locators",
            "language_model_providers",
        }
        assert set(zed_req.KNOWN_TOP_LEVEL_KEYS) == expected


class TestOnlineBehavior:
    def test_offline_runs_no_upstream_check(self, tmp_path):
        """Without --online there must be no network-dependent section.

        CI and pre-commit run offline-safe gates only; api.github.com being
        down or rate-limited must never block them.
        """
        _make_repo(tmp_path)
        manifest = zed_req.load_manifest(tmp_path / "extension.toml")
        sections = zed_req.run_checks(manifest, tmp_path, online=False)
        assert all("upstream" not in name for name, _ in sections), sections


class TestCliSmoke:
    def test_cli_exits_zero_on_real_manifest(self):
        """End-to-end: the script runs standalone and exits 0 on this repo."""
        result = subprocess.run(
            [sys.executable, str(SCRIPT_PATH)],
            capture_output=True,
            text=True,
            cwd=str(ROOT),
            timeout=30,
        )
        assert result.returncode == 0, (
            f"exit {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
        assert "[ok]" in result.stdout
        assert result.stdout.strip().endswith("extension.toml: 0 error(s)")
