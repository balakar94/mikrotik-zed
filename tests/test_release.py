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


def _grammar_git_rev(grammar_dir: str = "grammars/rsc") -> str | None:
    """Resolve the grammar working copy's HEAD rev, if git metadata is present."""
    for cmd in [
        ["git", "rev-parse", f"HEAD:{grammar_dir}"],
        ["git", "-C", grammar_dir, "rev-parse", "HEAD"],
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
        """Grammar crate version must exist and be valid semver — nothing more.

        It is deliberately NOT compared against the extension release version:
        the grammar lives in the separate tree-sitter-rsc repo ("own repo, own
        lifecycle", see AGENTS.md) and `make bump` never touches the submodule,
        so cross-group drift (e.g. extension 0.2.0 vs grammar 0.1.5) is expected.
        """
        grammar_cargo = ROOT / "grammars" / "rsc" / "Cargo.toml"
        if not grammar_cargo.exists():
            pytest.skip("grammars/rsc/Cargo.toml not present (run 'make grammar-clone')")
        g_v = _read_version_cargo(grammar_cargo)
        assert g_v is not None, "grammars/rsc/Cargo.toml missing version"
        assert SEMVER_RE.match(g_v), f"grammars/rsc/Cargo.toml version not semver: {g_v!r}"

    def test_extension_toml_version_matches_cargo(self):
        root_v = _read_version_cargo(ROOT / "Cargo.toml")
        ext_v, _ = _read_extension_version_and_rev(ROOT / "extension.toml")
        assert ext_v is not None, "extension.toml missing version"
        assert SEMVER_RE.match(ext_v), f"extension.toml version not semver: {ext_v!r}"
        assert root_v == ext_v, f"version drift: Cargo.toml={root_v!r} vs extension.toml={ext_v!r}"

    def test_grammar_package_json_matches_grammar_cargo(self):
        """Grammar package.json and grammar Cargo.toml describe the same
        tree-sitter-rsc release and must agree with EACH OTHER.

        They are NOT required to match the extension release version: grammar
        metadata keeps its own lifecycle in a separate repo (AGENTS.md "own
        repo, own lifecycle"; `make bump` never edits the submodule), so drift
        vs Cargo.toml/extension.toml at the repo root is expected and fine.
        """
        pkg = ROOT / "grammars" / "rsc" / "package.json"
        grammar_cargo = ROOT / "grammars" / "rsc" / "Cargo.toml"
        if not pkg.exists() or not grammar_cargo.exists():
            pytest.skip("grammar metadata not present (run 'make grammar-clone')")
        g_v = _read_version_cargo(grammar_cargo)
        pkg_v = _read_version_package_json(pkg)
        assert g_v is not None, "grammars/rsc/Cargo.toml missing version"
        assert pkg_v is not None, "grammars/rsc/package.json missing version"
        assert SEMVER_RE.match(pkg_v), f"package.json version not semver: {pkg_v!r}"
        assert pkg_v == g_v, (
            f"grammar metadata drift: grammars/rsc/Cargo.toml={g_v!r} vs grammars/rsc/package.json={pkg_v!r}"
        )

    def test_all_versions_same_coherence(self):
        """Versions must be coherent WITHIN each lifecycle group, never across groups.

        Group A (extension release): Cargo.toml, lsp/Cargo.toml and
        extension.toml are bumped together by `make bump` and must all exist
        and agree.

        Group B (grammar metadata): grammars/rsc/Cargo.toml and
        grammars/rsc/package.json live in the separate tree-sitter-rsc repo
        ("own repo, own lifecycle", see AGENTS.md; `make bump` never touches
        the submodule). When present they must agree with each other, but
        cross-group drift vs group A is EXPECTED and fine (e.g. extension
        0.2.0 alongside grammar 0.1.5) — do not restore strict equality here.
        """
        # Group A: extension release files — bumped together via `make bump`.
        ext_v, _ = _read_extension_version_and_rev(ROOT / "extension.toml")
        group_a = {
            "Cargo.toml": _read_version_cargo(ROOT / "Cargo.toml"),
            "lsp/Cargo.toml": _read_version_cargo(ROOT / "lsp" / "Cargo.toml"),
            "extension.toml": ext_v,
        }
        missing = sorted(name for name, v in group_a.items() if v is None)
        assert not missing, f"extension release files missing version: {missing}"
        unique_a = set(group_a.values())
        assert len(unique_a) == 1, f"version drift within extension release files: {group_a}"

        # Group B: grammar metadata — own lifecycle; coherent only among themselves.
        group_b: dict[str, str] = {}
        gv = _read_version_cargo(ROOT / "grammars" / "rsc" / "Cargo.toml")
        if gv is not None:
            group_b["grammars/rsc/Cargo.toml"] = gv
        pv = _read_version_package_json(ROOT / "grammars" / "rsc" / "package.json")
        if pv is not None:
            group_b["grammars/rsc/package.json"] = pv
        if group_b:
            unique_b = set(group_b.values())
            assert len(unique_b) == 1, f"version drift within grammar metadata files: {group_b}"

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

    def test_rev_matches_grammar_checkout(self):
        _, rev = _read_extension_version_and_rev(ROOT / "extension.toml")
        assert rev is not None
        git_rev = _grammar_git_rev("grammars/rsc")
        if git_rev is None:
            pytest.skip("cannot determine grammar rev (no git metadata — run 'make grammar-clone')")
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

    def test_matrix_has_all_six_targets(self):
        """release.yml must build every triple the extension can request.

        Mirrors src/platform.rs asset_triple(): two darwin, two linux-gnu and
        two windows-msvc triples. A missing entry means auto-download breaks
        on that platform at install time.
        """
        expected = [
            "x86_64-unknown-linux-gnu",
            "aarch64-unknown-linux-gnu",
            "aarch64-apple-darwin",
            "x86_64-apple-darwin",
            "x86_64-pc-windows-msvc",
            "aarch64-pc-windows-msvc",
        ]
        for target in expected:
            assert target in self.text, f"release.yml missing target {target!r}"
        found = sum(1 for t in expected if t in self.text)
        assert found == 6, f"expected 6 targets, found {found}"

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

    def test_each_binary_generates_and_uploads_sha256(self):
        """Every rsc-ls triple + extension.wasm needs a same-named .sha256.

        The WASM shim (src/verify.rs) fetches `<asset>.sha256` over HTTP and
        fails closed without it — a missing companion breaks auto-download.
        Each companion must appear at least twice: once in the Prepare
        (generation) step and once in the upload-artifact path.
        """
        binaries = [
            "rsc-ls-x86_64-unknown-linux-gnu",
            "rsc-ls-aarch64-unknown-linux-gnu",
            "rsc-ls-aarch64-apple-darwin",
            "rsc-ls-x86_64-apple-darwin",
            "rsc-ls-x86_64-pc-windows-msvc",
            "rsc-ls-aarch64-pc-windows-msvc",
            "extension.wasm",
        ]
        for binary in binaries:
            companion = f"{binary}.sha256"
            assert companion in self.text, f"release.yml missing companion {companion!r}"
            count = self.text.count(companion)
            assert count >= 2, (
                f"{companion!r} appears {count}x — expected generation + upload (>=2)"
            )
        # sha256sum with shasum fallback (macOS runners lack sha256sum).
        assert "sha256sum" in self.text, "release.yml missing sha256sum generation"
        assert "shasum -a 256" in self.text, "release.yml missing shasum fallback generation"

    def test_release_job_rebuilds_combined_sha256sums(self):
        """The release job must rebuild a combined SHA256SUMS from dist/*.sha256."""
        assert "Build combined SHA256SUMS" in self.text, (
            "release.yml missing 'Build combined SHA256SUMS' step"
        )
        assert "dist/*.sha256" in self.text, "release job must aggregate dist/*.sha256"
        assert "dist/SHA256SUMS" in self.text, "release job must write dist/SHA256SUMS"
        assert "fail_on_unmatched_files: true" in self.text, (
            "release.yml must keep fail_on_unmatched_files: true"
        )

    def test_preflight_exists_and_build_jobs_need_it(self):
        """Preflight gates all builds on tests, clippy, manifest and grammar."""
        assert re.search(r"^\s*preflight:", self.text, re.MULTILINE), (
            "release.yml missing preflight job"
        )
        for cmd in [
            "cargo test --workspace --locked",
            "cargo clippy -p rsc-ls --all-targets -- -D warnings",
            "make check-manifest",
            "make generate-check",
        ]:
            assert cmd in self.text, f"preflight missing command {cmd!r}"
        build_jobs = [
            "wasm",
            "build-linux-x86",
            "build-linux-arm64",
            "build-macos-arm64",
            "build-macos-x86",
            "build-windows-x86",
            "build-windows-arm64",
        ]
        for job in build_jobs:
            m = re.search(rf"^\s*{re.escape(job)}:\s*\n.*?needs:\s*\[([^\]]+)\]", self.text, re.MULTILINE | re.DOTALL)
            assert m, f"job {job!r} missing needs: [...]"
            needs = m.group(1)
            assert "preflight" in needs, f"job {job!r} must need preflight (needs: [{needs}])"

    def test_postflight_verifies_companions(self):
        """Postflight must prove the download contract from the live release."""
        assert re.search(r"^\s*postflight:", self.text, re.MULTILINE), (
            "release.yml missing postflight job"
        )
        m = re.search(r"^\s*postflight:\s*\n.*?needs:\s*\[([^\]]+)\]", self.text, re.MULTILINE | re.DOTALL)
        assert m, "postflight missing needs: [...]"
        assert "release" in m.group(1), (
            f"postflight must need release (needs: [{m.group(1)}])"
        )
        assert "gh release download" in self.text, (
            "postflight must download published assets via gh release download"
        )
        assert "sha256sum -c" in self.text, (
            "postflight must verify companions via sha256sum -c"
        )
        # The exact contract the shim relies on: every asset has a companion.
        assert ".sha256" in self.text, "postflight must reference .sha256 companions"


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
