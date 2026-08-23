"""Tests for release coherence: version sync, rev pinning, release.yml matrix, bump target."""

import re
import json
import subprocess
from pathlib import Path

import pytest

ROOT = Path(__file__).parent.parent
SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")
HEX40_RE = re.compile(r"^[0-9a-f]{40}$")


def _read_version_cargo(path: Path) -> str | None:
    if not path.exists():
        return None
    text = path.read_text(encoding="utf-8")
    m = re.search(r'^version\s*=\s*"([^"]+)"', text, re.MULTILINE)
    return m.group(1) if m else None


def _read_version_package_json(path: Path) -> str | None:
    if not path.exists():
        return None
    data = json.loads(path.read_text(encoding="utf-8"))
    return data.get("version")


def _read_extension_version_and_rev(path: Path) -> tuple[str | None, str | None]:
    if not path.exists():
        return None, None
    text = path.read_text(encoding="utf-8")
    v = re.search(r'^version\s*=\s*"([^"]+)"', text, re.MULTILINE)
    r = re.search(r'rev\s*=\s*"([^"]+)"', text)
    return (v.group(1) if v else None, r.group(1) if r else None)


def _git_rev(submodule_path: str = "grammars/rsc") -> str | None:
    """Try git rev-parse HEAD:grammars/rsc then grammars/rsc HEAD."""
    for cmd in [
        ["git", "rev-parse", f"HEAD:{submodule_path}"],
        ["git", "-C", submodule_path, "rev-parse", "HEAD"],
    ]:
        try:
            result = subprocess.run(cmd, capture_output=True, text=True, cwd=str(ROOT), timeout=5)
            if result.returncode == 0:
                rev = result.stdout.strip()
                if HEX40_RE.match(rev):
                    return rev
        except Exception:
            continue
    return None


# ── Version coherence ────────────────────────────────────────────────

class TestVersionCoherence:
    def test_cargo_toml_exists_and_has_version(self):
        v = _read_version_cargo(ROOT / "Cargo.toml")
        assert v is not None, "Cargo.toml missing version"
        assert SEMVER_RE.match(v), f"Cargo.toml version not semver: {v!r}"

    def test_lsp_cargo_toml_has_same_version(self):
        root_v = _read_version_cargo(ROOT / "Cargo.toml")
        lsp_v = _read_version_cargo(ROOT / "lsp" / "Cargo.toml")
        assert lsp_v is not None, "lsp/Cargo.toml missing version"
        assert SEMVER_RE.match(lsp_v), f"lsp/Cargo.toml version not semver: {lsp_v!r}"
        assert root_v == lsp_v, f"version drift: Cargo.toml={root_v!r} vs lsp/Cargo.toml={lsp_v!r}"

    def test_grammars_rsc_cargo_version_if_exists(self):
        grammar_cargo = ROOT / "grammars" / "rsc" / "Cargo.toml"
        if not grammar_cargo.exists():
            pytest.skip("grammars/rsc/Cargo.toml not present (submodule not initialized)")
        root_v = _read_version_cargo(ROOT / "Cargo.toml")
        g_v = _read_version_cargo(grammar_cargo)
        assert g_v is not None, "grammars/rsc/Cargo.toml missing version"
        assert SEMVER_RE.match(g_v), f"grammars/rsc/Cargo.toml version not semver: {g_v!r}"
        assert root_v == g_v, f"version drift: Cargo.toml={root_v!r} vs grammars/rsc/Cargo.toml={g_v!r}"

    def test_extension_toml_version_matches_cargo(self):
        root_v = _read_version_cargo(ROOT / "Cargo.toml")
        ext_v, _ = _read_extension_version_and_rev(ROOT / "extension.toml")
        assert ext_v is not None, "extension.toml missing version"
        assert SEMVER_RE.match(ext_v), f"extension.toml version not semver: {ext_v!r}"
        assert root_v == ext_v, f"version drift: Cargo.toml={root_v!r} vs extension.toml={ext_v!r}"

    def test_package_json_version_matches_cargo_if_exists(self):
        pkg = ROOT / "grammars" / "rsc" / "package.json"
        if not pkg.exists():
            pytest.skip("grammars/rsc/package.json not present")
        root_v = _read_version_cargo(ROOT / "Cargo.toml")
        pkg_v = _read_version_package_json(pkg)
        assert pkg_v is not None, "grammars/rsc/package.json missing version"
        assert SEMVER_RE.match(pkg_v), f"package.json version not semver: {pkg_v!r}"
        assert root_v == pkg_v, f"version drift: Cargo.toml={root_v!r} vs package.json={pkg_v!r}"

    def test_all_versions_same_coherence(self):
        """All present version files must agree."""
        versions: dict[str, str] = {}
        cargo_v = _read_version_cargo(ROOT / "Cargo.toml")
        if cargo_v:
            versions["Cargo.toml"] = cargo_v
        lsp_v = _read_version_cargo(ROOT / "lsp" / "Cargo.toml")
        if lsp_v:
            versions["lsp/Cargo.toml"] = lsp_v
        grammar_cargo = ROOT / "grammars" / "rsc" / "Cargo.toml"
        if grammar_cargo.exists():
            gv = _read_version_cargo(grammar_cargo)
            if gv:
                versions["grammars/rsc/Cargo.toml"] = gv
        ext_v, _ = _read_extension_version_and_rev(ROOT / "extension.toml")
        if ext_v:
            versions["extension.toml"] = ext_v
        pkg = ROOT / "grammars" / "rsc" / "package.json"
        if pkg.exists():
            pv = _read_version_package_json(pkg)
            if pv:
                versions["grammars/rsc/package.json"] = pv
        assert len(versions) >= 3, f"expected at least 3 version files, found {versions}"
        unique = set(versions.values())
        assert len(unique) == 1, f"version drift across files: {versions}"

    def test_valid_semver_all_files(self):
        """Every version file that exists must be valid semver."""
        paths = [
            (ROOT / "Cargo.toml", _read_version_cargo(ROOT / "Cargo.toml")),
            (ROOT / "lsp" / "Cargo.toml", _read_version_cargo(ROOT / "lsp" / "Cargo.toml")),
        ]
        ext_v, _ = _read_extension_version_and_rev(ROOT / "extension.toml")
        paths.append((ROOT / "extension.toml", ext_v))
        pkg = ROOT / "grammars" / "rsc" / "package.json"
        if pkg.exists():
            paths.append((pkg, _read_version_package_json(pkg)))
        grammar_cargo = ROOT / "grammars" / "rsc" / "Cargo.toml"
        if grammar_cargo.exists():
            paths.append((grammar_cargo, _read_version_cargo(grammar_cargo)))
        for p, v in paths:
            assert v is not None, f"{p} missing version"
            assert SEMVER_RE.match(v), f"{p} version {v!r} not semver"


# ── extension.toml rev ───────────────────────────────────────────────

class TestExtensionRev:
    def test_rev_exists(self):
        _, rev = _read_extension_version_and_rev(ROOT / "extension.toml")
        assert rev is not None, "extension.toml missing [grammars.rsc] rev"
        assert rev.strip() != "", "extension.toml rev is empty"

    def test_rev_is_40_char_hex(self):
        _, rev = _read_extension_version_and_rev(ROOT / "extension.toml")
        assert rev is not None
        assert HEX40_RE.match(rev), f"rev not 40-char hex: {rev!r}"

    def test_rev_not_placeholder(self):
        _, rev = _read_extension_version_and_rev(ROOT / "extension.toml")
        assert rev is not None
        assert not rev.startswith("000"), f"rev is placeholder 000...: {rev!r}"
        assert rev != "0" * 40, "rev is all zeros placeholder"

    def test_rev_matches_git_submodule(self):
        _, rev = _read_extension_version_and_rev(ROOT / "extension.toml")
        assert rev is not None
        git_rev = _git_rev("grammars/rsc")
        if git_rev is None:
            pytest.skip("cannot determine git rev (no git or submodule not initialized)")
        assert rev == git_rev, f"extension.toml rev {rev!r} != git HEAD {git_rev!r} (run scripts/publish_grammar.py)"

    def test_rev_lowercase_hex(self):
        _, rev = _read_extension_version_and_rev(ROOT / "extension.toml")
        assert rev is not None
        assert rev == rev.lower(), f"rev should be lowercase hex: {rev!r}"


# ── release.yml ──────────────────────────────────────────────────────

class TestReleaseYml:
    @pytest.fixture(autouse=True)
    def _load(self):
        self.path = ROOT / ".github" / "workflows" / "release.yml"
        assert self.path.exists(), ".github/workflows/release.yml missing"
        self.text = self.path.read_text(encoding="utf-8")

    def test_matrix_has_4_targets(self):
        expected = [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
        ]
        for target in expected:
            assert target in self.text, f"release.yml missing target {target!r}"
        # Count occurrences — each target should appear at least once in matrix
        found = sum(1 for t in expected if t in self.text)
        assert found == 4, f"expected 4 targets, found {found}"

    def test_release_yml_has_wasm_target(self):
        assert "wasm32-wasip2" in self.text, "release.yml missing wasm32-wasip2 target"
        # Should also have a wasm job
        assert re.search(r"^\s*wasm:", self.text, re.MULTILINE), "release.yml missing wasm job"

    def test_release_yml_has_attestations(self):
        assert "attest-build-provenance" in self.text, "release.yml missing attest-build-provenance"
        assert "attestations: write" in self.text, "release.yml missing attestations: write permission"
        # At least 2 attest steps (wasm + binaries)
        assert self.text.count("attest-build-provenance") >= 2, "expected at least 2 attest steps (wasm + binaries)"

    def test_release_yml_matrix_includes_binary_names(self):
        # Each matrix entry should have binary name rsc-ls-<target>
        for target in [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
        ]:
            binary = f"rsc-ls-{target}"
            assert binary in self.text, f"release.yml missing binary {binary!r}"

    def test_release_yml_has_id_token_permission(self):
        assert "id-token: write" in self.text, "release.yml missing id-token: write for attestations"


# ── Makefile bump target ─────────────────────────────────────────────

class TestMakefileBump:
    @pytest.fixture(autouse=True)
    def _load(self):
        self.path = ROOT / "Makefile"
        assert self.path.exists(), "Makefile missing"
        self.text = self.path.read_text(encoding="utf-8")

    def test_makefile_has_bump_target(self):
        assert re.search(r"^bump\s*:", self.text, re.MULTILINE), "Makefile missing bump target"
        assert "VERSION" in self.text, "Makefile bump target should reference VERSION"

    def test_bump_updates_cargo_toml(self):
        # Bump rewrites the parent workspace manifests only.
        assert "Cargo.toml" in self.text, "Makefile bump should mention Cargo.toml"
        assert "lsp/Cargo.toml" in self.text, "Makefile bump should update lsp/Cargo.toml"
        # Grammar versions live in the separate tree-sitter-rsc repo (the
        # grammars/rsc submodule) and must never be bumped from the parent.
        assert "grammars/rsc/Cargo.toml" not in self.text, "Makefile bump must not edit the grammar submodule"

    def test_bump_updates_extension_but_not_submodule(self):
        # publish_grammar.py auto-commits any dirty submodule edits on --push;
        # parent-side bumps must never leak into the grammar repo.
        assert "package.json" not in self.text, "Makefile bump must not edit grammars/rsc/package.json"
        assert "extension.toml" in self.text, "Makefile bump should update extension.toml"

    def test_bump_uses_version_variable(self):
        # Ensure VERSION is checked / substituted
        assert "$(VERSION)" in self.text, "Makefile bump should use $(VERSION)"

    def test_no_hardcoded_version_drift_in_makefile(self):
        # Makefile should not hardcode a version string like 0.1.0 outside of comments
        # Allow version in comments / help but not as assignment
        hardcoded = re.findall(r'version\s*=\s*"0\.\d+\.\d+"', self.text)
        # The bump target uses sed with version pattern, not a hardcoded value
        # So no literal version assignment should exist in Makefile
        assert len(hardcoded) == 0, f"Makefile has hardcoded version: {hardcoded}"
