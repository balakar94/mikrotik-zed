"""Tests for upstream-docs provenance: manifest builder, committed snapshot, drift watchdog, check-mode write policy."""

import hashlib
import re
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).parent.parent
sys.path.insert(0, str(ROOT / "scripts"))

from sync_llms import build_manifest_text, main as sync_main

SHA256_RE = re.compile(r"^[0-9a-f]{64}$")
ISO_Z_RE = re.compile(r"^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z$")

FAKE_SOURCES = [
    {"name": "index", "path": "llms.txt", "url": "https://manual.mikrotik.com/llms.txt", "sha256": "a" * 64},
    {"name": "full", "path": "llms-full.txt", "url": "https://manual.mikrotik.com/llms-full.txt", "sha256": "b" * 64},
]

# Minimal upstream-like payloads (RouterOS version hint must appear in the first 8 KiB).
PAYLOADS = {
    "llms.txt": b"# MikroTik RouterOS Manual\n\n- [Docs](https://manual.mikrotik.com/docs.md)\n",
    "llms-full.txt": b"# MikroTik RouterOS Manual\n\nRouterOS v7.22 documentation\n\n## /interface\n\ncorpus body\n",
}


def _toml_loads(text: str) -> dict:
    import tomllib

    return tomllib.loads(text)


def _fake_fetch(payloads: dict[str, bytes]):
    """Return (fetch, calls): a network-free stand-in for fetch_url keyed by file name."""
    calls = []

    def fetch(url: str) -> bytes:
        calls.append(url)
        for fname, data in payloads.items():
            if url.endswith(fname):
                return data
        raise AssertionError(f"unexpected URL passed to fake fetcher: {url}")

    return fetch, calls


# ── Deliverable B: docs-drift.yml workflow ───────────────────────────

class TestDriftWorkflow:
    @pytest.fixture(autouse=True)
    def _load(self):
        self.path = ROOT / ".github" / "workflows" / "docs-drift.yml"
        assert self.path.exists(), ".github/workflows/docs-drift.yml missing"
        self.text = self.path.read_text(encoding="utf-8")

    @staticmethod
    def _assert_valid_cron(expr: str) -> list[str]:
        fields = expr.split()
        assert len(fields) == 5, f"cron must have 5 fields: {expr!r}"
        ranges = [(0, 59), (0, 23), (1, 31), (1, 12), (0, 6)]  # min hour dom month dow
        for field, (lo, hi) in zip(fields, ranges):
            step = field.split("/", 1)[0]
            for atom in step.split("-"):
                if atom == "*":
                    continue
                assert atom.isdigit(), f"cron field {field!r}: invalid atom {atom!r}"
                assert lo <= int(atom) <= hi, f"cron field {field!r}: {atom!r} outside [{lo},{hi}]"
        return fields

    def test_name(self):
        assert re.search(r"^name:\s*Upstream Watchdog\s*$", self.text, re.MULTILINE), "workflow name must be 'Upstream Watchdog'"

    def test_schedule_cron_is_valid_and_weekly(self):
        crons = re.findall(r'cron:\s*"([^"]+)"', self.text)
        assert crons, "no cron expression found under on:.schedule"
        dow_fields = set()
        for cron in crons:
            fields = self._assert_valid_cron(cron)
            # Weekly cadence: day-of-week is Monday ("1") or every day ("*").
            assert fields[4] in {"1", "*"}, f"cron {cron!r} is not weekly (dow={fields[4]!r})"
            dow_fields.add(fields[4])
        assert '"43 5 * * 1"' in self.text, "expected the documented cron '43 5 * * 1' (Mondays 05:43 UTC)"

    def test_has_workflow_dispatch_trigger(self):
        assert re.search(r"^  workflow_dispatch:", self.text, re.MULTILINE), "missing workflow_dispatch trigger"

    def test_permissions_allow_issue_management_only(self):
        m = re.search(r"^permissions:\n((?:[ \t]+\w+:\s*\w+\n?)+)", self.text, re.MULTILINE)
        assert m, "permissions block missing"
        block = m.group(1)
        assert re.search(r"\bcontents:\s*read\b", block), "contents: read required"
        assert re.search(r"\bissues:\s*write\b", block), "issues: write required"

    def test_step_invokes_sync_check_with_result_mapping(self):
        assert "sync_llms.py --check" in self.text, "watchdog must run scripts/sync_llms.py --check"
        assert "$GITHUB_OUTPUT" in self.text, "check result must be exported via $GITHUB_OUTPUT"
        for branch in ("drift", "ok", "error"):
            assert f"steps.check.outputs.result == '{branch}'" in self.text, f"missing '{branch}' branch condition"

    def test_drift_branch_uses_gh_cli_and_labeled_issue(self):
        assert "gh issue create" in self.text, "drift branch must be able to create an issue"
        assert "gh issue comment" in self.text, "existing issue must be commented, not duplicated"
        assert "gh issue close" in self.text, "ok branch must close the resolved issue"
        assert "--reason completed" in self.text, "issue must be closed as completed"
        assert "upstream-docs" in self.text, "issues must carry the upstream-docs label"
        assert "Upstream RouterOS docs drifted" in self.text, "issue title prefix must match spec"
        assert "extract_commands.py" in self.text, "issue body must include maintainer fix instructions"


# ── Deliverable A: manifest builder unit tests ───────────────────────

class TestFullVersionHint:
    def test_picks_highest_feature_gate_mention(self):
        from sync_llms import full_version_hint

        text = "available since RouterOS v7.14 ... Starting from RouterOS version 7.23 ... RouterOS 6.49"
        assert full_version_hint(text) == "7.23"

    def test_handles_three_component_versions(self):
        from sync_llms import full_version_hint

        assert full_version_hint("requires RouterOS v7.18.2 or newer") == "7.18.2"

    def test_empty_when_no_mention(self):
        from sync_llms import full_version_hint

        assert full_version_hint("# MikroTik RouterOS Manual\n\n## Certificates\n") == ""

    def test_ignores_ros_prefix_and_ips(self):
        from sync_llms import full_version_hint

        # "ROS 802.11" must not be read as a version; only "RouterOS" counts.
        assert full_version_hint("ROS 802.11 wireless, see RouterOS 7.16 notes") == "7.16"

    def test_ignores_pre_release_tags(self):
        from sync_llms import full_version_hint

        # "7.24beta1" is not stable 7.24 — the highest STABLE mention wins.
        text = "Starting with RouterOS 7.24beta1 ... Starting from RouterOS version 7.23 ..."
        assert full_version_hint(text) == "7.23"


class TestManifestBuilder:
    def test_parses_back_with_expected_schema(self):
        text = build_manifest_text("7.22", "2026-08-26T12:34:56Z", FAKE_SOURCES)
        data = _toml_loads(text)
        assert data["schema"] == 1
        assert data["routeros_version"] == "7.22"
        assert data["synced_at_utc"] == "2026-08-26T12:34:56Z"
        sources = data["sources"]
        assert len(sources) == 2
        assert [s["name"] for s in sources] == ["index", "full"], "ordering must be index before full"
        for src, expected in zip(sources, FAKE_SOURCES):
            assert SHA256_RE.match(src["sha256"]), f"sha256 not 64 lowercase hex: {src['sha256']!r}"
            assert src["path"] == expected["path"]
            assert src["url"] == expected["url"]

    def test_timestamp_format_is_iso_z(self):
        text = build_manifest_text("7.22", "2026-01-02T03:04:05Z", FAKE_SOURCES)
        data = _toml_loads(text)
        assert ISO_Z_RE.match(data["synced_at_utc"]), f"timestamp not ISO-Z: {data['synced_at_utc']!r}"

    def test_deterministic_output(self):
        one = build_manifest_text("7.22", "2026-08-26T12:34:56Z", FAKE_SOURCES)
        two = build_manifest_text("7.22", "2026-08-26T12:34:56Z", FAKE_SOURCES)
        assert one == two, "identical inputs must produce byte-identical output"
        assert one.index('name = "index"') < one.index('name = "full"'), "default order must be index before full"

    def test_source_order_is_caller_owned(self):
        text = build_manifest_text("7.22", "2026-08-26T12:34:56Z", list(reversed(FAKE_SOURCES)))
        data = _toml_loads(text)
        assert [s["name"] for s in data["sources"]] == ["full", "index"], "builder renders sources in input order"

    def test_unknown_version_rendered_as_empty_string(self):
        text = build_manifest_text("", "2026-08-26T12:34:56Z", FAKE_SOURCES)
        data = _toml_loads(text)
        assert data["routeros_version"] == ""


# ── Bootstrap artifact: committed provenance baseline ────────────────

class TestCommittedManifest:
    @pytest.fixture(autouse=True)
    def _load(self):
        self.path = ROOT / "data" / "upstream-docs.toml"
        assert self.path.exists(), (
            "data/upstream-docs.toml missing — bootstrap it once with 'python3 scripts/sync_llms.py --force'"
        )
        self.data = _toml_loads(self.path.read_text(encoding="utf-8"))

    def test_schema_and_version_shape(self):
        assert self.data["schema"] == 1
        version = self.data["routeros_version"]
        assert isinstance(version, str) and version != "", "routeros_version must be non-empty"
        assert re.match(r"^\d+\.\d+", version), f"routeros_version not a version hint: {version!r}"
        assert ISO_Z_RE.match(self.data["synced_at_utc"]), "synced_at_utc must be ISO-8601 Z format"

    def test_two_sources_index_then_full(self):
        sources = self.data["sources"]
        assert [s["name"] for s in sources] == ["index", "full"], "manifest must record exactly index then full"
        assert [s["path"] for s in sources] == ["llms.txt", "llms-full.txt"]

    def test_recorded_hashes_are_well_formed(self):
        """Unconditional: recorded digests must be valid SHA256 hex."""
        for src in self.data["sources"]:
            assert SHA256_RE.match(src["sha256"]), f"{src['path']}: recorded sha256 malformed"

    def test_recorded_hashes_match_local_bytes(self):
        """Core provenance guarantee: recomputing catches accidental hand-edits
        of either side.

        Skipped per-source when the local file is absent: llms.txt and
        llms-full.txt are gitignored, so a clean CI checkout does not have
        them (the fetch step runs after this suite). The check stays active
        for every developer run where the files exist.
        """
        for src in self.data["sources"]:
            local = ROOT / src["path"]
            if not local.is_file():
                pytest.skip(f"{src['path']} absent on clean checkout (gitignored) — nothing to recompute")
            actual = hashlib.sha256(local.read_bytes()).hexdigest()
            assert actual == src["sha256"], (
                f"{src['path']} drifted from recorded snapshot ({actual[:16]} != {src['sha256'][:16]}) — "
                "run 'make sync && make extract' and commit both generated files"
            )


# ── Check-mode write policy: --check must never produce the manifest ──

class TestCheckModeNeverWrites:
    """Drives main() decision points directly via its injectable root/fetch
    parameters (no network, no monkeypatching of internals needed)."""

    def test_check_mode_writes_nothing_when_files_missing(self, tmp_path):
        fetch, _ = _fake_fetch(PAYLOADS)
        rc = sync_main(argv=["--check"], project_root=tmp_path, fetch=fetch)
        assert rc == 2, "missing local files mean drift (--check must report exit 2)"
        assert not (tmp_path / "llms.txt").exists()
        assert not (tmp_path / "llms-full.txt").exists()
        assert not (tmp_path / "data" / "upstream-docs.toml").exists(), "--check must never create the manifest"

    def test_check_mode_leaves_existing_files_and_manifest_alone(self, tmp_path):
        (tmp_path / "llms.txt").write_bytes(b"stale index\n")  # differs from PAYLOADS -> drift
        (tmp_path / "llms-full.txt").write_bytes(PAYLOADS["llms-full.txt"])
        manifest = tmp_path / "data" / "upstream-docs.toml"
        manifest.parent.mkdir()
        manifest.write_text("# stale pre-existing content\n")

        fetch, _ = _fake_fetch(PAYLOADS)
        rc = sync_main(argv=["--check"], project_root=tmp_path, fetch=fetch)
        assert rc == 2
        assert (tmp_path / "llms.txt").read_bytes() == b"stale index\n", "--check must not rewrite local files"
        assert manifest.read_text() == "# stale pre-existing content\n", "--check must not touch the manifest"

    def test_real_run_bootstraps_missing_manifest(self, tmp_path):
        fetch, calls = _fake_fetch(PAYLOADS)
        rc = sync_main(argv=[], project_root=tmp_path, fetch=fetch)
        assert rc == 0
        assert len(calls) == len(PAYLOADS), "both upstream files must be fetched"
        assert (tmp_path / "llms-full.txt").read_bytes() == PAYLOADS["llms-full.txt"]

        data = _toml_loads((tmp_path / "data" / "upstream-docs.toml").read_text(encoding="utf-8"))
        by_name = {s["name"]: s for s in data["sources"]}
        assert by_name["full"]["sha256"] == hashlib.sha256(PAYLOADS["llms-full.txt"]).hexdigest()
        assert by_name["index"]["sha256"] == hashlib.sha256(PAYLOADS["llms.txt"]).hexdigest()
        assert data["routeros_version"] == "7.22", "version hint extracted from payload header"
        assert ISO_Z_RE.match(data["synced_at_utc"])

    def test_real_run_no_changes_keeps_existing_manifest_untouched(self, tmp_path):
        for fname, payload in PAYLOADS.items():
            (tmp_path / fname).write_bytes(payload)  # identical to upstream -> no change
        manifest = tmp_path / "data" / "upstream-docs.toml"
        manifest.parent.mkdir()
        marker = "# old timestamp baseline — must survive\n"
        manifest.write_text(marker)

        fetch, _ = _fake_fetch(PAYLOADS)
        rc = sync_main(argv=[], project_root=tmp_path, fetch=fetch)
        assert rc == 0
        assert manifest.read_text() == marker, "zero-churn guarantee: unchanged sync must not rewrite the manifest"

    def test_real_run_change_rewrites_manifest_with_new_hashes(self, tmp_path):
        (tmp_path / "llms.txt").write_bytes(PAYLOADS["llms.txt"])
        (tmp_path / "llms-full.txt").write_bytes(b"# stale corpus, upstream has moved on\n")
        manifest = tmp_path / "data" / "upstream-docs.toml"

        fetch, _ = _fake_fetch(PAYLOADS)
        rc = sync_main(argv=[], project_root=tmp_path, fetch=fetch)
        assert rc == 0

        data = _toml_loads(manifest.read_text(encoding="utf-8"))
        by_name = {s["name"]: s for s in data["sources"]}
        assert by_name["full"]["sha256"] == hashlib.sha256(PAYLOADS["llms-full.txt"]).hexdigest()
        assert ISO_Z_RE.match(data["synced_at_utc"])
