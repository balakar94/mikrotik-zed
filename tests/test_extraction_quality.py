"""Source attribution, the index requirement, and conservative parsing contracts."""

import json
import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import extract_commands as extract
import sync_llms as sync
from extraction_quality import (
    COVERAGE_VALUES, load_report, load_source_index, make_report, report_regressions, source_contexts,
)


def test_apps_fragments_preserve_source_and_settings_boundary(tmp_path):
    source = tmp_path / "llms-full.txt"
    source.write_bytes((ROOT / "tests/fixtures/extraction/apps.md").read_bytes())
    (tmp_path / "llms.txt").write_text(
        "- [Apps](https://manual.mikrotik.com/docs/containers/apps.md)\n"
        "- [/app](https://manual.mikrotik.com/docs/cli-reference/app.md)\n"
        "- [/app/settings](https://manual.mikrotik.com/docs/cli-reference/app/settings.md)\n"
    )
    tables = []
    menus = {m["path"]: m for m in extract.parse_llms_full(str(source), table_report=tables)}
    app = menus["/app"]
    readonly = next(row for row in app["read_only"] if row["name"] == "app-size")
    assert readonly["source_kind"] == "narrative"
    assert readonly["source_url"].endswith("/containers/apps.md")
    assert readonly["unset"] is None
    assert not {row["name"] for row in app["arguments"]} & {"disk", "app-store-urls"}
    assert next(row for row in menus["/app/settings"]["arguments"] if row["name"] == "disk")["description"] == "Settings storage disk."
    assert all(table["status"] == "resolved" for table in tables)
    assert app["source_url"].endswith("/cli-reference/app.md")


def test_every_argument_states_its_unset_capability(tmp_path, monkeypatch):
    """The marker is written for every argument, known or not.

    A missing key must never be readable as "known to be non-unsettable", so
    the generator states the capability even when it is unknown.
    """
    monkeypatch.setattr(extract, "__file__", str(tmp_path / "scripts/extract_commands.py"))
    source = tmp_path / "llms-full.txt"
    source.write_text(
        '## quality/items\n**Type:** Directory\n<ArgTable c1="Argument">\n'
        '<ArgTableRow arg="alpha" typ="string"></ArgTableRow>\n</ArgTable>\n'
    )
    (tmp_path / "llms.txt").write_text(
        "- [/quality/items](https://manual.mikrotik.com/docs/cli-reference/quality/items.md)\n"
    )
    assert extract.main([]) == 0
    text = (tmp_path / "data/commands.toml").read_text()
    blocks = text.split("[[menus.arguments]]")[1:]
    assert blocks
    assert all("unset_known = " in block for block in blocks)
    documented = next(block for block in blocks if 'name = "alpha"' in block)
    assert "unset_known = true" in documented
    generic = next(block for block in blocks if 'name = "comment"' in block)
    assert "unset_known = false" in generic


def test_missing_index_aborts_unless_recovery_is_explicit(tmp_path, monkeypatch, capsys):
    monkeypatch.setattr(extract, "__file__", str(tmp_path / "scripts/extract_commands.py"))
    (tmp_path / "llms-full.txt").write_text('## quality/items\n**Type:** Directory\n')
    with pytest.raises(SystemExit) as exit_info:
        extract.main([])
    assert exit_info.value.code == 1
    assert "llms.txt not found" in capsys.readouterr().err
    assert not (tmp_path / "data/commands.toml").exists()
    assert extract.main(["--allow-missing-index"]) == 0
    assert 'path = "/quality/items"' in (tmp_path / "data/commands.toml").read_text()


def test_index_duplicates_and_internal_titles_cannot_change_page_identity(tmp_path):
    index = tmp_path / "llms.txt"
    index.write_text(
        "- [Apps](https://manual.mikrotik.com/docs/containers/apps.md)\n"
        "- [Other](https://manual.mikrotik.com/docs/other.md)\n"
        "- [Duplicate](https://manual.mikrotik.com/docs/a.md)\n"
        "- [Duplicate](https://manual.mikrotik.com/docs/b.md)\n"
    )
    lookup = load_source_index(index)
    assert "duplicate" not in lookup
    contexts = source_contexts("## Apps\n## Other\n~~~\n---\n## Other\n~~~\n---\n## Other".splitlines(), lookup)
    assert contexts[1]["url"].endswith("/containers/apps.md")
    assert contexts[4]["code"]
    assert contexts[-1]["url"].endswith("/other.md")


def test_fenced_examples_cannot_invent_menus_or_properties(tmp_path):
    source = tmp_path / "llms-full.txt"
    source.write_text(
        '## ip/address\n**Type:** Directory\n'
        '~~~~rsc\n## invented/menu\n**Type:** Directory\n'
        '<ArgTable c1="Argument">\n<ArgTableRow arg="fake" typ="string"></ArgTableRow>\n</ArgTable>\n~~~~\n'
        '<ArgTable c1="Argument">\n<ArgTableRow arg="address" typ="ipAddr"></ArgTableRow>\n</ArgTable>\n'
    )
    menus = extract.parse_llms_full(str(source))
    assert [m["path"] for m in menus] == ["/ip/address"]
    assert [a["name"] for a in menus[0]["arguments"]] == ["address"]


def test_unrelated_property_names_do_not_hide_a_coverage_gap():
    table = {"context": ["/ip/address"], "rows": [{"name": "new-property"}]}
    names = {"/ip/address": {"address"}, "/ip/other": {"new-property"}}
    assert extract._markdown_table_has_real_delta(table, names)
    table["context"] = ["/ip/other"]
    assert not extract._markdown_table_has_real_delta(table, names)


def test_unknown_table_target_is_reported_without_inventing_a_menu(tmp_path, capsys):
    source = tmp_path / "llms-full.txt"
    source.write_text(
        '## ip/address\n**Type:** Directory\n## Topic\n'
        '**Sub-menu:** `/unknown/menu`\n### Properties\n'
        '| Property | Description |\n| :-- | :-- |\n| **new-property** | Description. |\n'
    )
    tables = []
    menus = extract.parse_llms_full(str(source), table_report=tables)
    assert [m["path"] for m in menus] == ["/ip/address"]
    assert tables[0]["status"] == "unresolved"
    assert tables[0]["reason"] == "unknown-explicit-menu"
    assert "names unknown menu(s)" in capsys.readouterr().err


def test_truncated_raw_enums_are_never_embedded_as_complete_members():
    assert extract.extract_enum_values("enum (one | two | ...)") == []
    assert extract.extract_enum_values("enum (one | two | …)") == []


def test_enum_member_cases_mirror_the_rust_fallback():
    """Shared case table with `lsp/src/menus.rs::parse_enum_values`.

    Only the cases both sides must agree on are pinned. This side reads the RAW
    type, so it also recovers members from a bit-map suffix or a body the
    display cap would cut; the Rust fallback reads the capped display type and
    refuses those, because an incomplete list could reject a valid value.
    Complete members reach the LSP through `enum_values = [...]` instead.
    """
    assert extract.extract_enum_values("enum (none)") == ["none"]
    assert extract.extract_enum_values("enum (one | two)") == ["one", "two"]
    assert extract.extract_enum_values("enum (one | two | ...)") == []
    assert extract.extract_enum_values("enum (one | two | …)") == []
    assert extract.extract_enum_values("enum ()") == []
    assert extract.extract_enum_values("enum") == []
    assert extract.extract_enum_values("ubit (1Mbps, 2Mbps)") == []
    # Deliberate divergence, safe direction: the raw type still has the members
    # behind its bit-map suffix; the capped display type cannot be trusted for
    # a suffix that may in fact be a cut.
    assert extract.extract_enum_values("enum (a | b) { a:0, b:1 }") == ["a", "b"]
    assert extract.extract_enum_values("enum (a | b | c") == []


def test_report_records_per_menu_provenance_and_coverage(tmp_path):
    source = tmp_path / "llms-full.txt"
    source.write_text("fixture")
    (tmp_path / "llms.txt").write_text(
        "- [/ip](https://manual.mikrotik.com/docs/cli-reference/ip.md)\n"
    )
    report = make_report(
        [
            {
                "path": "/ip/address",
                "coverage": "partial",
                "source_url": "https://manual.mikrotik.com/docs/cli-reference/ip/address.md",
                "arguments": [
                    {"name": "address", "source_kind": "cli-reference"},
                    {"name": "note", "source_kind": "narrative",
                     "source_url": "https://manual.mikrotik.com/docs/ip.md"},
                    {"name": "orphan", "source_kind": "narrative"},
                ],
            }
        ],
        [],
        {"menus": 1},
        source,
    )
    assert report["schema_version"] == 2
    assert report["index_sha256"]
    menu = report["menus"]["/ip/address"]
    assert menu["coverage"] in COVERAGE_VALUES
    assert menu["properties"] == ["address", "note", "orphan"]
    assert menu["unattributed_properties"] == 1


def _report_menu(path, *, coverage="reference", source_url="", properties=()):
    return {
        "path": path,
        "coverage": coverage,
        "source_url": source_url,
        "arguments": [{"name": name, "source_kind": "cli-reference"} for name in properties],
    }


def test_report_comparison_ignores_line_shifts_and_detects_provenance_loss(tmp_path):
    source = tmp_path / "llms-full.txt"
    source.write_text("fixture")
    table = {"source_url": "official-page", "heading": "Properties", "properties": ["alpha"],
             "line": 10, "status": "unresolved"}
    menus = [_report_menu(
        "/ip/address",
        source_url="https://manual.mikrotik.com/docs/cli-reference/ip/address.md",
        properties=["address"],
    )]
    old = make_report(menus, [table], {}, source)

    shifted = json.loads(json.dumps(old))
    shifted["tables"][0]["line"] = 300
    assert report_regressions(old, shifted) == []

    lost = json.loads(json.dumps(old))
    menu = lost["menus"]["/ip/address"]
    menu["source_url"] = ""
    menu["coverage"] = "partial"
    menu["unattributed_properties"] = 2
    menu["properties"] = []
    lost["tables"][0]["properties"] = ["new-property"]
    errors = report_regressions(old, lost)
    assert any("removed properties" in error for error in errors)
    assert any("lost its source URL" in error for error in errors)
    assert any("coverage downgraded" in error for error in errors)
    assert any("lost their source" in error for error in errors)
    assert any("new unresolved table" in error for error in errors)

    removed = json.loads(json.dumps(old))
    del removed["menus"]["/ip/address"]
    assert any("removed menu" in error for error in report_regressions(old, removed))


def _valid_report_payload():
    return {
        "schema_version": 2,
        "menus": {"/ip": {"properties": ["a"], "coverage": "reference", "source_url": "",
                          "unattributed_properties": 0}},
        "tables": [],
    }


@pytest.mark.parametrize(
    "case",
    ["not-an-object", "old-schema", "menu-not-object", "properties-not-strings", "unknown-coverage",
     "source-url-not-string", "unattributed-not-int", "malformed-table"],
)
def test_malformed_baselines_fail_cleanly(tmp_path, case):
    payload = _valid_report_payload()
    if case == "not-an-object":
        payload = []
    elif case == "old-schema":
        payload["schema_version"] = 1
    elif case == "menu-not-object":
        payload["menus"] = {"/ip": 1}
    elif case == "properties-not-strings":
        payload["menus"]["/ip"]["properties"] = [1]
    elif case == "unknown-coverage":
        payload["menus"]["/ip"]["coverage"] = "curated-ish"
    elif case == "source-url-not-string":
        payload["menus"]["/ip"]["source_url"] = None
    elif case == "unattributed-not-int":
        payload["menus"]["/ip"]["unattributed_properties"] = "0"
    else:
        payload["tables"] = [{"status": "unknown", "source_url": "", "heading": "", "properties": []}]
    path = tmp_path / "baseline.json"
    path.write_text(json.dumps(payload))
    with pytest.raises(ValueError):
        load_report(path)


def test_valid_baseline_round_trips(tmp_path):
    path = tmp_path / "baseline.json"
    payload = _valid_report_payload()
    path.write_text(json.dumps(payload))
    assert load_report(path) == payload


def test_regression_gate_keeps_command_data_and_baseline_and_check_writes_nothing(tmp_path, monkeypatch):
    monkeypatch.setattr(extract, "__file__", str(tmp_path / "scripts/extract_commands.py"))
    source = tmp_path / "llms-full.txt"
    source.write_text(
        '## quality/items\n**Type:** Directory\n<ArgTable c1="Argument">\n'
        '<ArgTableRow arg="alpha" typ="string"></ArgTableRow>\n</ArgTable>\n'
    )
    (tmp_path / "llms.txt").write_text(
        "- [/quality/items](https://manual.mikrotik.com/docs/cli-reference/quality/items.md)\n"
    )
    baseline = tmp_path / "baseline.json"
    assert extract.main(["--report", str(baseline)]) == 0
    output = tmp_path / "data/commands.toml"
    original, previous_report = output.read_bytes(), baseline.read_bytes()
    before = {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in tmp_path.rglob("*") if p.is_file()}
    assert extract.main(["--check"]) == 0
    after = {p: (p.read_bytes(), p.stat().st_mtime_ns) for p in tmp_path.rglob("*") if p.is_file()}
    assert before == after
    source.write_text(source.read_text().replace('arg="alpha"', 'arg="beta"'))
    assert extract.main(["--baseline", str(baseline), "--fail-on-regression"]) == 1
    assert output.read_bytes() == original
    assert baseline.read_bytes() == previous_report
    assert extract.main(["--check"]) == 1


@pytest.mark.parametrize("data", [b"", b"<html>error</html>", b"# RouterOS\n\xff", b"# RouterOS\nNo sections", b"# RouterOS\n\x00\n## menu"])
def test_unusable_downloads_leave_both_sources_and_manifest_untouched(tmp_path, data):
    before = {
        "llms.txt": b"original index", "llms-full.txt": b"original full",
        "data/upstream-docs.toml": b"original provenance",
    }
    for name, body in before.items():
        path = tmp_path / name
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(body)
    valid_index = b"# RouterOS\n- [Docs](https://manual.mikrotik.com/docs.md)\n"
    assert sync.main([], project_root=tmp_path, fetch=lambda url: valid_index if url.endswith("/llms.txt") else data) == 1
    assert all((tmp_path / name).read_bytes() == body for name, body in before.items())


def test_large_document_section_loss_is_rejected():
    old = ("# RouterOS\n" + "## menu\n" * 40).encode()
    with pytest.raises(ValueError, match="coverage drop"):
        sync.validate_payload("llms-full.txt", b"# RouterOS\n## menu\n", old)


def test_snapshot_commit_rolls_back_if_later_rename_fails(tmp_path, monkeypatch):
    first, second = tmp_path / "index", tmp_path / "full"
    first.write_bytes(b"old index")
    second.write_bytes(b"old full")
    replace = sync.os.replace

    def fail_second(src, dst):
        if dst == second:
            raise OSError("simulated rename failure")
        return replace(src, dst)

    monkeypatch.setattr(sync.os, "replace", fail_second)
    with pytest.raises(OSError):
        sync.replace_snapshots([(first, b"new index"), (second, b"new full")])
    assert first.read_bytes() == b"old index"
    assert second.read_bytes() == b"old full"
    assert not list(tmp_path.glob("*.tmp"))


def test_invalid_http_success_is_retried_before_sync_accepts_it(monkeypatch):
    valid = b"# RouterOS\n## menu\n"
    responses = iter([b"<html>temporary error</html>", valid])
    calls = []

    class Response:
        status = 200

        def __enter__(self):
            return self

        def __exit__(self, *_):
            return False

        def read(self, limit):
            calls.append(limit)
            return next(responses)

        def getheader(self, _):
            return None

    monkeypatch.setattr(sync.urllib.request, "urlopen", lambda *_args, **_kwargs: Response())
    monkeypatch.setattr(sync.time, "sleep", lambda _: None)
    assert sync.fetch_url("https://manual.mikrotik.com/llms-full.txt") == valid
    assert len(calls) == 2
    assert all(limit == sync.MAX_DOWNLOAD_BYTES + 1 for limit in calls)
