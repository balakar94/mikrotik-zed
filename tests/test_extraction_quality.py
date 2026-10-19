"""Source attribution, the index requirement, and conservative parsing contracts."""

import sys
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import extract_commands as extract
from extraction_quality import load_source_index, source_contexts


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
