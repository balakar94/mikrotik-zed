"""Tests for the llms-full.txt command extraction script."""
import sys
import os
import tempfile
from pathlib import Path

# Add scripts to path
sys.path.insert(0, str(Path(__file__).parent.parent / "scripts"))

from extract_commands import (
    should_include,
    escape_toml_string,
    clean_type,
    generate_toml,
    parse_llms_full,
    extract_enum_values,
    write_if_changed,
    synthesize_directories,
    finalize_menus,
    load_overrides,
    apply_overrides,
    OverrideError,
    _extract_heading_path,
    _extract_bare_cli_root,
    _strip_generated_line,
    _MAX_COVERED_ROOTS,
)


class TestShouldInclude:
    """Tests for menu path filtering."""

    def test_ip_address(self):
        assert should_include("/ip/address") is True

    def test_ip_route(self):
        assert should_include("/ip/route") is True

    def test_ip_firewall_filter(self):
        assert should_include("/ip/firewall/filter") is True

    def test_ip_firewall_nat(self):
        assert should_include("/ip/firewall/nat") is True

    def test_ip_dhcp_server(self):
        assert should_include("/ip/dhcp-server") is True

    def test_ip_dns(self):
        assert should_include("/ip/dns") is True

    def test_ip_service(self):
        assert should_include("/ip/service") is True

    def test_ipv6_address(self):
        assert should_include("/ipv6/address") is True

    def test_ipv6_dhcp_client(self):
        assert should_include("/ipv6/dhcp-client") is True

    def test_ipv6_nd(self):
        assert should_include("/ipv6/nd") is True

    def test_ipv6_firewall(self):
        assert should_include("/ipv6/firewall/filter") is True

    def test_ipv6_route(self):
        assert should_include("/ipv6/route") is True

    def test_interface_bridge(self):
        assert should_include("/interface/bridge") is True

    def test_interface_vlan(self):
        assert should_include("/interface/vlan") is True

    def test_interface_pppoe_client(self):
        assert should_include("/interface/pppoe-client") is True

    def test_interface_ethernet(self):
        assert should_include("/interface/ethernet") is True

    def test_routing_ospf(self):
        assert should_include("/routing/ospf") is True

    def test_routing_bgp(self):
        assert should_include("/routing/bgp") is True

    def test_routing_table(self):
        assert should_include("/routing/table") is True

    def test_routing_rule(self):
        assert should_include("/routing/rule") is True

    # ── Previously excluded menus — now INCLUDED under complete coverage ──

    def test_certificate_menu_included(self):
        """/certificate is a real RouterOS menu — must be extracted."""
        assert should_include("/certificate") is True

    # ── Now included under full extraction ────────────────────

    def test_now_included_system(self):
        """System is now included under full extraction."""
        assert should_include("/system/identity") is True

    def test_now_included_tool(self):
        """Tool is now included under full extraction."""
        assert should_include("/tool/ping") is True

    def test_now_included_user(self):
        """User is now included under full extraction."""
        assert should_include("/user") is True

    def test_now_included_queue(self):
        """Queue is now included under full extraction."""
        assert should_include("/queue/simple") is True

    def test_now_included_ip_arp(self):
        """ARP is now included under full /ip extraction."""
        assert should_include("/ip/arp") is True

    def test_now_included_ip_pool(self):
        """Pool is now included under full /ip extraction."""
        assert should_include("/ip/pool") is True

    # ── Edge cases ───────────────────────────────────────────

    def test_empty_path(self):
        assert should_include("") is False

    def test_root_only(self):
        """Root-only paths are included if the path looks like a CLI root."""
        assert should_include("/ip") is True
        assert should_include("/certificate") is True  # Now included under complete coverage

    def test_deeply_nested_firewall(self):
        assert should_include("/ip/firewall/filter/reset-counters") is True

    def test_deeply_nested_bridge(self):
        assert should_include("/interface/bridge/port/monitor") is True


class TestEscapeTomlString:
    """Tests for TOML string escaping."""

    def test_simple_string(self):
        assert escape_toml_string("hello") == "hello"

    def test_backslash(self):
        assert escape_toml_string("a\\b") == "a\\\\b"

    def test_quote(self):
        assert escape_toml_string('say "hi"') == 'say \\"hi\\"'

    def test_newline(self):
        assert escape_toml_string("line1\nline2") == "line1 line2"

    def test_carriage_return(self):
        assert escape_toml_string("a\rb") == "ab"  # \r is stripped

    def test_empty(self):
        assert escape_toml_string("") == ""


class TestCleanType:
    """Tests for type string cleaning."""

    def test_simple_type(self):
        assert clean_type("bool") == "bool"

    def test_multiline_type(self):
        result = clean_type("alt { ipAddr\n, string\n }")
        assert "alt" in result
        assert "ipAddr" in result

    def test_long_type(self):
        long_type = "x" * 200
        result = clean_type(long_type)
        assert len(result) <= 153  # 150 + "..."
        assert result.endswith("...")

    def test_enum_type(self):
        result = clean_type("enum (disabled | enabled | proxy-arp)")
        assert "enum" in result
        assert "disabled" in result


class TestExtractEnumValues:
    """Tests for enum member extraction from RAW type strings."""

    def test_multi_member_enum(self):
        assert extract_enum_values("enum (input | forward | output)") == [
            "input",
            "forward",
            "output",
        ]

    def test_single_member_enum(self):
        assert extract_enum_values("enum (none)") == ["none"]

    def test_colon_containing_members(self):
        assert extract_enum_values("enum (mac:ssid | mac | ssid)") == [
            "mac:ssid",
            "mac",
            "ssid",
        ]

    def test_bitmap_suffix_ignored(self):
        typ = "enum (as-username | as-username-and-password) { as-username:0, as-username-and-password:1 }"
        assert extract_enum_values(typ) == ["as-username", "as-username-and-password"]

    def test_non_enum_type_yields_empty(self):
        assert extract_enum_values("ipPrefix") == []
        assert extract_enum_values("bool") == []
        assert extract_enum_values("") == []

    def test_empty_enum_body(self):
        assert extract_enum_values("enum ()") == []

    def test_whitespace_around_members_stripped(self):
        assert extract_enum_values("enum (  a  |  b |c)") == ["a", "b", "c"]

    def test_long_enum_survives_beyond_display_truncation_limit(self):
        # Members must be extracted from the raw string even when the type
        # display value would be truncated by clean_type() at >100 chars.
        members = [f"value-{i:02d}" for i in range(30)]
        typ = "enum (" + " | ".join(members) + ")"
        assert len(typ) > 100
        assert extract_enum_values(typ) == members


class TestEnumValuesInPipeline:
    """enum_values flows from parse_llms_full into generate_toml output."""

    def _write_temp(self, content: str) -> str:
        tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".txt", delete=False, encoding="utf-8")
        tmp.write(content)
        tmp.flush()
        tmp.close()
        return tmp.name

    def test_parsed_argument_gets_enum_values(self):
        content = """
## ip/firewall/filter

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="chain" typ="enum (input | forward | output)">Chain</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            arg = menus[0]["arguments"][0]
            assert arg["enum_values"] == ["input", "forward", "output"]
        finally:
            os.unlink(path)

    def test_non_enum_argument_has_no_enum_values_field(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="address" typ="ipPrefix">Addr</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            arg = parse_llms_full(path)[0]["arguments"][0]
            assert "enum_values" not in arg
        finally:
            os.unlink(path)

    def test_generate_toml_emits_enum_values_for_arguments(self):
        menus = [
            {
                "path": "/ip/firewall/filter",
                "type": "Directory",
                "flags": [],
                "arguments": [
                    {
                        "name": "chain",
                        "type": "enum (input | forward | output)",
                        "required": False,
                        "unset": False,
                        "description": "Chain name",
                        "enum_values": ["input", "forward", "output"],
                    }
                ],
                "read_only": [],
            }
        ]
        result = generate_toml(menus)
        assert 'enum_values = ["input", "forward", "output"]' in result
        # Emission order: enum_values belongs to the argument block, after type.
        chain_block = result.split("[[menus.arguments]]")[1].split("[[menus]]")[0]
        assert 'name = "chain"' in chain_block
        assert chain_block.index('type = "') < chain_block.index("enum_values = ")

    def test_generate_toml_skips_enum_values_for_flags_and_read_only(self):
        menus = [
            {
                "path": "/ip/firewall/filter",
                "type": "Directory",
                "flags": [
                    {
                        "name": "D",
                        "type": "enum (dynamic | static)",
                        "required": False,
                        "unset": False,
                        "description": "",
                        "enum_values": ["dynamic", "static"],
                    }
                ],
                "arguments": [],
                "read_only": [
                    {
                        "name": "mode",
                        "type": "enum (a | b)",
                        "required": False,
                        "unset": False,
                        "description": "",
                        "enum_values": ["a", "b"],
                    }
                ],
            }
        ]
        result = generate_toml(menus)
        assert "enum_values" not in result, "flags/read_only must not carry enum_values"

    def test_generate_toml_omits_field_when_no_members(self):
        menus = [
            {
                "path": "/ip/address",
                "type": "Directory",
                "flags": [],
                "arguments": [
                    {
                        "name": "address",
                        "type": "ipPrefix",
                        "required": True,
                        "unset": False,
                        "description": "",
                    }
                ],
                "read_only": [],
            }
        ]
        result = generate_toml(menus)
        assert "enum_values" not in result

    def test_generated_toml_is_valid_and_roundtrips_members(self):
        import tomllib

        menus = [
            {
                "path": "/interface/wireless/security-modes",
                "type": "Directory",
                "flags": [],
                "arguments": [
                    {
                        "name": "security-profile-mode",
                        "type": "enum (mac:ssid | mac | ssid) { mac:ssid:0 }",
                        "required": False,
                        "unset": False,
                        "description": 'Mode with "quotes"',
                        "enum_values": ["mac:ssid", "mac", "ssid"],
                    }
                ],
                "read_only": [],
            }
        ]
        parsed = tomllib.loads(generate_toml(menus))
        arg = parsed["menus"][0]["arguments"][0]
        assert arg["enum_values"] == ["mac:ssid", "mac", "ssid"]


class TestCoversHeader:
    """The `# Covers:` header is computed from included menus."""

    @staticmethod
    def _menus_for_roots(roots):
        return [
            {"path": f"/{root}/menu", "type": "Directory", "flags": [], "arguments": [], "read_only": []}
            for root in roots
        ]

    def test_few_roots_listed_sorted_with_real_count(self):
        menus = self._menus_for_roots(["tool", "ip", "interface"])
        result = generate_toml(menus)
        covers_line = next(l for l in result.split("\n") if l.startswith("# Covers:"))
        assert covers_line == "# Covers: /interface, /ip, /tool (3 menus)"

    def test_many_roots_capped_at_twelve_with_ellipsis(self):
        roots = [f"root{i:02d}" for i in range(20)]  # 20 distinct roots
        menus = self._menus_for_roots(roots)
        result = generate_toml(menus)
        covers_line = next(l for l in result.split("\n") if l.startswith("# Covers:"))
        expected_prefix = ", ".join(f"/root{i:02d}" for i in range(_MAX_COVERED_ROOTS))
        assert covers_line == f"# Covers: {expected_prefix}, … ({len(menus)} menus)"
        # The 13th root must not leak into the header
        assert "/root12" not in covers_line

    def test_exactly_twelve_roots_not_capped(self):
        roots = [f"r{i}" for i in range(_MAX_COVERED_ROOTS)]
        menus = self._menus_for_roots(roots)
        result = generate_toml(menus)
        covers_line = next(l for l in result.split("\n") if l.startswith("# Covers:"))
        assert "…" not in covers_line
        assert covers_line.endswith(f"({_MAX_COVERED_ROOTS} menus)")

    def test_duplicate_root_segments_counted_once(self):
        menus = [
            {"path": "/ip/a", "type": "Directory", "flags": [], "arguments": [], "read_only": []},
            {"path": "/ip/b", "type": "Directory", "flags": [], "arguments": [], "read_only": []},
        ]
        result = generate_toml(menus)
        covers_line = next(l for l in result.split("\n") if l.startswith("# Covers:"))
        assert covers_line == "# Covers: /ip (2 menus)"


class TestSynthesizeDirectories:
    """Missing ancestor Directories are filled in before serialization.

    Upstream docs dropped pure-Directory sections, so parents like /ip or
    /routing/ospf vanish while their children keep their own pages. The
    synthesizer restores hierarchy integrity for prefix completion and
    unknown-menu diagnostics — gaps only, never overwrites.
    """

    @staticmethod
    def _menu(path: str, **extra) -> dict:
        base = {"path": path, "type": "Directory", "flags": [], "arguments": [], "read_only": []}
        base.update(extra)
        return base

    def test_child_only_synthesizes_single_parent(self):
        result = synthesize_directories([self._menu("/ip/address")])
        paths = [m["path"] for m in result]
        assert paths == ["/ip/address", "/ip"]
        synthesized = next(m for m in result if m["path"] == "/ip")
        assert synthesized["type"] == "Directory"
        assert synthesized["flags"] == []
        assert synthesized["arguments"] == []
        assert synthesized["read_only"] == []

    def test_deep_chain_synthesizes_every_level(self):
        # /a/b/c implies both /a/b and /a — recursion through all prefixes.
        result = synthesize_directories([self._menu("/a/b/c")])
        by_path = {m["path"]: m for m in result}
        assert sorted(by_path) == ["/a", "/a/b", "/a/b/c"]
        assert by_path["/a"]["type"] == "Directory"
        assert by_path["/a/b"]["type"] == "Directory"
        assert by_path["/a/b"]["arguments"] == []

    def test_explicit_parent_not_duplicated_nor_overwritten(self):
        explicit_parent = self._menu(
            "/routing",
            type="Command",
            arguments=[{"name": "gateway", "type": "ipAddr"}],
        )
        result = synthesize_directories([
            explicit_parent,
            self._menu("/routing/ospf/area"),
        ])
        matches = [m for m in result if m["path"] == "/routing"]
        assert len(matches) == 1, "explicit parent must not be duplicated"
        # Explicit entry survives untouched: type override and arguments kept.
        assert matches[0]["type"] == "Command"
        assert matches[0]["arguments"] == [{"name": "gateway", "type": "ipAddr"}]
        # The still-missing mid-level ancestor is synthesized as a gap fill.
        paths = [m["path"] for m in result]
        assert "/routing/ospf" in paths

    def test_single_segment_menu_has_no_ancestors(self):
        result = synthesize_directories([self._menu("/certificate")])
        assert [m["path"] for m in result] == ["/certificate"]

    def test_shared_parent_synthesized_once(self):
        result = synthesize_directories([
            self._menu("/ip/address"),
            self._menu("/ip/arp"),
        ])
        assert [m["path"] for m in result].count("/ip") == 1

    def test_input_list_not_mutated(self):
        menus = [self._menu("/ip/address")]
        original = [dict(m) for m in menus]
        synthesize_directories(menus)
        assert menus == original


class TestSynthesisPipeline:
    """Child-only docs flow end-to-end: parse → finalize → generate_toml."""

    def _write_temp(self, content: str) -> str:
        tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".txt", delete=False, encoding="utf-8")
        tmp.write(content)
        tmp.flush()
        tmp.close()
        return tmp.name

    def test_child_only_doc_yields_child_and_parent_in_toml(self):
        import tomllib

        # New upstream heading format: no leading slash; trailing space tolerated.
        content = (
            "## ip/address \n"
            "\n"
            "<ArgTable c1=\"Argument\" c2=\"Type\" c3=\"Description\">\n"
            '<ArgTableRow arg="address" typ="ipPrefix">Addr</ArgTableRow>\n'
            "</ArgTable>\n"
        )
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        result = generate_toml(finalize_menus(menus))

        parsed = tomllib.loads(result)
        entries = {m["path"]: m["type"] for m in parsed["menus"]}
        assert entries == {"/ip/address": "Directory", "/ip": "Directory"}
        # The synthesized parent stays bare: no argument rows leak into it.
        blocks = result.split("[[menus]]")[1:]
        parent_block = next(b for b in blocks if b.startswith('\npath = "/ip"\n'))
        assert "arguments" not in parent_block

    def test_generation_twice_is_byte_identical_modulo_timestamp(self):
        content = "## routing/ospf/area\n\n## system/note\n"
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        first = generate_toml(finalize_menus(list(menus)))
        second = generate_toml(finalize_menus(list(menus)))
        # Only the # Generated: timestamp may differ between runs (the same
        # tolerance write_if_changed applies); everything else must match byte
        # for byte, including the placement of synthesized directories.
        assert _strip_generated_line(first) == _strip_generated_line(second)


class TestWriteIfChanged:
    """Timestamp-only churn does not rewrite the output file."""

    def _fresh_path(self) -> Path:
        tmpdir = tempfile.mkdtemp()
        return Path(tmpdir) / "commands.toml"

    @staticmethod
    def _content(timestamp: str, body: str = "body-line") -> str:
        return (
            "# MikroTik RouterOS CLI Command Table\n"
            f"# Generated: {timestamp}\n"
            f"{body}\n"
        )

    def test_timestamp_only_difference_keeps_existing_file(self):
        target = self._fresh_path()
        first = self._content("2026-01-01T00:00:00Z")
        second = self._content("2026-02-02T12:34:56.789Z")
        assert first != second

        target.write_text(first, encoding="utf-8")
        wrote = write_if_changed(target, second)
        assert wrote is False
        assert target.read_text(encoding="utf-8") == first, "file must stay byte-identical"

    def test_identical_content_keeps_existing_file(self):
        target = self._fresh_path()
        first = self._content("2026-01-01T00:00:00Z")
        target.write_text(first, encoding="utf-8")
        assert write_if_changed(target, first) is False
        assert target.read_text(encoding="utf-8") == first

    def test_material_change_rewrites_file(self):
        target = self._fresh_path()
        first = self._content("2026-01-01T00:00:00Z", body="old body")
        second = self._content("2026-01-01T00:00:00Z", body="new body")
        target.write_text(first, encoding="utf-8")
        assert write_if_changed(target, second) is True
        assert target.read_text(encoding="utf-8") == second

    def test_missing_file_is_written(self):
        target = self._fresh_path()
        assert write_if_changed(target, self._content("t")) is True
        assert target.exists()

    def test_second_generate_over_same_menus_leaves_file_byte_identical(self):
        target = self._fresh_path()
        menus = [
            {
                "path": "/ip/address",
                "type": "Directory",
                "flags": [],
                "arguments": [
                    {
                        "name": "address",
                        "type": "enum (a | b | c)",
                        "required": True,
                        "unset": False,
                        "description": "Addr",
                        "enum_values": ["a", "b", "c"],
                    }
                ],
                "read_only": [],
            }
        ]
        first = generate_toml(menus)
        target.write_text(first, encoding="utf-8")
        # A later run over identical inputs may produce a new timestamp…
        second = generate_toml(menus)
        wrote = write_if_changed(target, second)
        # …but the tracked file must remain byte-identical either way.
        assert wrote is False
        assert target.read_text(encoding="utf-8") == first


class TestGenerateToml:
    """Tests for TOML generation."""

    def test_empty_menus(self):
        result = generate_toml([])
        assert "# MikroTik" in result
        assert "Auto-generated" in result

    def test_single_menu(self):
        menus = [
            {
                "path": "/ip/address",
                "type": "Directory",
                "flags": [],
                "arguments": [
                    {
                        "name": "address",
                        "type": "composite",
                        "required": True,
                        "unset": False,
                        "description": "IP address",
                    }
                ],
                "read_only": [],
            }
        ]
        result = generate_toml(menus)
        assert "[[menus]]" in result
        assert 'path = "/ip/address"' in result
        assert "[[menus.arguments]]" in result
        assert 'name = "address"' in result

    def test_menu_with_flags(self):
        menus = [
            {
                "path": "/ip/route",
                "type": "Directory",
                "flags": [
                    {"name": "X", "description": "disabled", "required": False}
                ],
                "arguments": [],
                "read_only": [],
            }
        ]
        result = generate_toml(menus)
        assert "[[menus.flags]]" in result
        assert 'name = "X"' in result

    def test_menu_with_read_only(self):
        menus = [
            {
                "path": "/interface/bridge",
                "type": "Directory",
                "flags": [],
                "arguments": [],
                "read_only": [
                    {
                        "name": "mac-address",
                        "type": "macAddr",
                        "required": False,
                        "unset": False,
                        "description": "MAC address",
                    }
                ],
            }
        ]
        result = generate_toml(menus)
        assert "[[menus.read_only]]" in result
        assert 'name = "mac-address"' in result

    def test_menu_with_special_chars_escaping(self):
        menus = [
            {
                "path": "/ip/address",
                "type": "Directory",
                "flags": [],
                "arguments": [
                    {
                        "name": "comment",
                        "type": "string",
                        "required": False,
                        "unset": False,
                        "description": 'Say "hi" with \\ backslash',
                    }
                ],
                "read_only": [],
            }
        ]
        result = generate_toml(menus)
        assert '\\"hi\\"' in result or 'say \\"hi\\"' in result
        assert "\\\\" in result

    def test_multiple_menus_sorted_output(self):
        menus = [
            {"path": "/ip/route", "type": "Directory", "flags": [], "arguments": [], "read_only": []},
            {"path": "/ip/address", "type": "Directory", "flags": [], "arguments": [], "read_only": []},
        ]
        result = generate_toml(menus)
        # Order preserved as input
        assert result.index("/ip/route") < result.index("/ip/address") or result.index("/ip/address") < result.index("/ip/route")


class TestExtractHeadingPath:
    """Tests for markdown heading path extraction."""

    def test_simple_menu_heading(self):
        assert _extract_heading_path("## ip/address") == "ip/address"

    def test_heading_with_leading_slash(self):
        # Heading already contains leading slash — function returns it as-is
        assert _extract_heading_path("## /ip/address") == "/ip/address"

    def test_heading_with_markdown_link_stripped(self):
        # Markdown link is stripped, leaving no "/" => returns None (treated as non-menu heading)
        assert _extract_heading_path("## [ip/address](http://example.com)") is None

    def test_heading_with_trailing_dot_stripped(self):
        assert _extract_heading_path("## ip/address.") == "ip/address"
        assert _extract_heading_path("## ip/address...") == "ip/address"

    def test_heading_without_slash_returns_none(self):
        assert _extract_heading_path("## Overview") is None
        assert _extract_heading_path("## Certificates") is None

    def test_single_hash_ignored(self):
        assert _extract_heading_path("# ip/address") is None

    def test_five_hashes_ignored(self):
        assert _extract_heading_path("##### ip/address") is None

    def test_h4_heading_is_valid(self):
        assert _extract_heading_path("#### ip/firewall/filter") == "ip/firewall/filter"

    def test_heading_with_hash_inside_returns_none(self):
        # Starts with # after extraction? e.g., "## #not-a-path/ip" -> contains "/" but starts with "#"
        assert _extract_heading_path("## #weird/path") is None

    def test_empty_heading_returns_none(self):
        assert _extract_heading_path("## ") is None

    def test_heading_with_spaces_trimmed(self):
        assert _extract_heading_path("##   ip/address   ") == "ip/address"


class TestParseLlmsFull:
    """Tests for llms-full.txt parsing with temp files — deterministic, no network."""

    def _write_temp(self, content: str) -> str:
        tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".txt", delete=False, encoding="utf-8")
        tmp.write(content)
        tmp.flush()
        tmp.close()
        return tmp.name

    def test_empty_file_returns_empty(self):
        path = self._write_temp("")
        try:
            assert parse_llms_full(path) == []
        finally:
            os.unlink(path)

    def test_non_menu_headings_ignored(self):
        content = "## Overview\nSome text\n## Certificates\nMore text\n"
        path = self._write_temp(content)
        try:
            assert parse_llms_full(path) == []
        finally:
            os.unlink(path)

    def test_single_menu_with_flags_and_arguments(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Flag" c2="Name" c3="Description">
<ArgTableRow arg="X" typ="disabled">disabled</ArgTableRow>
</ArgTable>

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="address" typ="ipPrefix" mandatory="1">IP address</ArgTableRow>
<ArgTableRow arg="interface" typ="iface_enum">Interface</ArgTableRow>
</ArgTable>

<ArgTable c1="Read-only Argument" c2="Type" c3="Description">
<ArgTableRow arg="actual-interface" typ="iface_enum">Actual interface</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert len(menus) == 1
            m = menus[0]
            assert m["path"] == "/ip/address"
            assert m["type"] == "Directory"
            assert len(m["flags"]) == 1
            assert m["flags"][0]["name"] == "X"
            assert len(m["arguments"]) == 2
            assert m["arguments"][0]["name"] == "address"
            assert m["arguments"][0]["required"] is True
            assert len(m["read_only"]) == 1
        finally:
            os.unlink(path)

    def test_missing_type_defaults_to_directory(self):
        content = """
## ip/route

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="gateway" typ="ipAddr">Gateway</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert len(menus) == 1
            assert menus[0]["path"] == "/ip/route"
            assert menus[0]["type"] == "Directory"
        finally:
            os.unlink(path)

    def test_malformed_heading_without_slash_ignored(self):
        content = """
## NotAMenu

**Type:** Directory

## ip/address

**Type:** Directory
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert len(menus) == 1
            assert menus[0]["path"] == "/ip/address"
        finally:
            os.unlink(path)

    def test_multiple_menus_only_target_included(self):
        content = """
## ip/address

**Type:** Directory

## certificate/settings

**Type:** Directory

## interface/bridge

**Type:** Directory
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            paths = [m["path"] for m in menus]
            assert "/ip/address" in paths
            assert "/interface/bridge" in paths
            assert "/certificate/settings" in paths  # Now included under complete coverage
        finally:
            os.unlink(path)

    def test_argtable_with_missing_typ_and_arg(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="address">No type</ArgTableRow>
<ArgTableRow typ="string">No arg</ArgTableRow>
<ArgTableRow>Empty row</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert len(menus) == 1
            args = menus[0]["arguments"]
            # Anonymous rows (missing arg attribute) are skipped entirely.
            assert len(args) == 1
            assert args[0]["name"] == "address"
            assert args[0]["type"] == ""
        finally:
            os.unlink(path)

    def test_argtable_unknown_c1_ignored(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Unknown" c2="Foo" c3="Bar">
<ArgTableRow arg="should-not-appear" typ="string">Hidden</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert len(menus) == 1
            m = menus[0]
            assert len(m["flags"]) == 0
            assert len(m["arguments"]) == 0
            assert len(m["read_only"]) == 0
        finally:
            os.unlink(path)

    def test_heading_with_markdown_link_filtered(self):
        content = """
## [ip/address](https://example.com)

**Type:** Directory
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            # Link stripped leaves empty path, so no valid menu
            assert len(menus) == 0
        finally:
            os.unlink(path)

    def test_heading_with_trailing_dots(self):
        content = """
## ip/address.

**Type:** Directory
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert len(menus) == 1
            assert menus[0]["path"] == "/ip/address"
        finally:
            os.unlink(path)

    def test_mandatory_and_unset_flags_parsed(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="address" typ="string" mandatory="1" unset="1">Required and unsettable</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            arg = menus[0]["arguments"][0]
            assert arg["required"] is True
            assert arg["unset"] is True
        finally:
            os.unlink(path)

    def test_description_extraction_with_empty(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="network" typ="ipAddr"></ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert menus[0]["arguments"][0]["description"] == ""
        finally:
            os.unlink(path)

    def test_last_menu_saved_at_eof(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="address" typ="ipPrefix">Addr</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert len(menus) == 1
            assert menus[0]["path"] == "/ip/address"
        finally:
            os.unlink(path)

    def test_description_with_special_chars_preserved(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="comment" typ="string">Comment with "quotes" and \\ backslash</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            desc = menus[0]["arguments"][0]["description"]
            assert "quotes" in desc
        finally:
            os.unlink(path)

    def test_deeply_nested_path_parsed(self):
        content = """
## ip/firewall/filter

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="chain" typ="enum (input | forward | output)">Chain</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert len(menus) == 1
            assert menus[0]["path"] == "/ip/firewall/filter"
            assert menus[0]["arguments"][0]["name"] == "chain"
        finally:
            os.unlink(path)

    def test_routing_scoped_filtering(self):
        content = """
## routing/ospf

**Type:** Directory

## routing/bgp

**Type:** Directory

## routing/rip

**Type:** Directory
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            paths = [m["path"] for m in menus]
            assert "/routing/ospf" in paths
            assert "/routing/bgp" in paths
            assert "/routing/rip" in paths  # Now included — complete coverage includes all routing submenus
        finally:
            os.unlink(path)

    def test_type_line_with_extra_whitespace(self):
        content = """
## ip/address

**Type:**   Directory   

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="address" typ="string">Test</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            assert menus[0]["type"] == "Directory"
        finally:
            os.unlink(path)

    def test_argtable_row_outside_table_ignored(self):
        content = """
<ArgTableRow arg="orphan" typ="string">Orphan</ArgTableRow>

## ip/address

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="address" typ="string">Valid</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            # Orphan row before any menu should be ignored
            assert len(menus) == 1
            assert len(menus[0]["arguments"]) == 1
            assert menus[0]["arguments"][0]["name"] == "address"
        finally:
            os.unlink(path)

    def test_multiple_argtables_in_sequence(self):
        content = """
## ip/address

**Type:** Directory

<ArgTable c1="Flag" c2="Name" c3="Description">
<ArgTableRow arg="X" typ="disabled">disabled</ArgTableRow>
</ArgTable>

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="address" typ="ipPrefix">Addr</ArgTableRow>
</ArgTable>

<ArgTable c1="Read-only Argument" c2="Type" c3="Description">
<ArgTableRow arg="actual-interface" typ="iface_enum">Actual</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
            m = menus[0]
            assert len(m["flags"]) == 1
            assert len(m["arguments"]) == 1
            assert len(m["read_only"]) == 1
        finally:
            os.unlink(path)


class TestExtractBareCliRoot:
    """Unit tests for the slash-less root-heading extractor."""

    def test_simple_root(self):
        assert _extract_bare_cli_root("## user") == "user"

    def test_trailing_whitespace_and_dot_normalized(self):
        # Real upstream headings carry trailing spaces ("## user ").
        assert _extract_bare_cli_root("## log  ") == "log"
        assert _extract_bare_cli_root("## undo.") == "undo"

    def test_h4_level_accepted(self):
        assert _extract_bare_cli_root("#### import") == "import"

    def test_hyphenated_word_accepted(self):
        assert _extract_bare_cli_root("## safe-mode") == "safe-mode"

    def test_slash_path_rejected(self):
        # Slash-bearing headings belong to _extract_heading_path().
        assert _extract_bare_cli_root("## user/group") is None
        assert _extract_bare_cli_root("## /ip/address") is None

    def test_uppercase_prose_rejected(self):
        assert _extract_bare_cli_root("## Overview") is None

    def test_multiword_prose_rejected(self):
        assert _extract_bare_cli_root("## print parameters") is None

    def test_non_heading_rejected(self):
        assert _extract_bare_cli_root("plain text") is None
        assert _extract_bare_cli_root("# single-hash") is None
        assert _extract_bare_cli_root("##### five-hashes") is None


class TestBareRootMenus:
    """Root pages are titled WITHOUT a leading slash upstream (`## user`).

    A bare CLI-word heading opens only a PENDING menu: it is kept only when
    a `**Type:**` line appears before the next heading of any level (other
    metadata lines such as `**Package:**` may precede it). Prose subheadings
    (`balance-xor`) never carry one and must stay out. Slash-derived menus
    never require the confirmation — missing **Type:** still defaults to
    Directory for them.
    """

    def _write_temp(self, content: str) -> str:
        tmp = tempfile.NamedTemporaryFile(mode="w", suffix=".txt", delete=False, encoding="utf-8")
        tmp.write(content)
        tmp.flush()
        tmp.close()
        return tmp.name

    def test_bare_root_with_package_then_type_captures_all_sections(self):
        # Real upstream layout: **Package:** (and possibly **Syscap:** /
        # **Conditions:**) precede **Type:** — a next-line check would miss
        # these pages. All three ArgTable sections must attach.
        content = """
## user 

**Package:** system

**Type:** Directory

<ArgTable c1="Flag" c2="Name" c3="Description">
<ArgTableRow arg="X" typ="disabled">disabled</ArgTableRow>
<ArgTableRow arg="E" typ="expired">expired</ArgTableRow>
</ArgTable>

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="name" typ="string" mandatory="1"></ArgTableRow>
</ArgTable>

<ArgTable c1="Read-only Argument" c2="Type" c3="Description">
<ArgTableRow arg="last-logged-in" typ="date"></ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        assert len(menus) == 1
        m = menus[0]
        assert m["path"] == "/user"
        assert m["type"] == "Directory"
        assert [f["name"] for f in m["flags"]] == ["X", "E"]
        assert [a["name"] for a in m["arguments"]] == ["name"]
        assert m["arguments"][0]["required"] is True
        assert [r["name"] for r in m["read_only"]] == ["last-logged-in"]

    def test_bare_word_without_type_not_captured(self):
        # Prose subheadings match the CLI regex but never carry **Type:**
        # before the next heading of any level — they must not become menus.
        content = """
## balance-xor

Some prose about the balance-xor bonding mode.

## interface/bonding

**Type:** Directory
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        assert [m["path"] for m in menus] == ["/interface/bonding"]

    def test_bare_word_before_slash_heading_not_captured(self):
        # No **Type:** between the bare word and the next heading → dropped,
        # while the slash menu itself is unaffected.
        content = """
## undo

## ip/address

**Type:** Directory
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        assert [m["path"] for m in menus] == ["/ip/address"]

    def test_unconfirmed_bare_candidates_do_not_leak(self):
        # Two consecutive unconfirmed bare words flush each other; neither
        # lands in the result.
        content = """
## environment

## redo

Some prose.

## system/note

**Type:** Directory
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        assert [m["path"] for m in menus] == ["/system/note"]

    def test_bare_root_command_captured_as_command(self):
        # Root-level COMMANDS (not Directories): Type says "Command".
        content = """
## password

**Type:** Command

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="old-password" typ="string" mandatory="1">Old password</ArgTableRow>
<ArgTableRow arg="new-password" typ="string" mandatory="1">New password</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        assert len(menus) == 1
        m = menus[0]
        assert m["path"] == "/password"
        assert m["type"] == "Command"
        assert [a["name"] for a in m["arguments"]] == ["old-password", "new-password"]
        assert m["flags"] == []
        assert m["read_only"] == []

    def test_confirmed_bare_root_flushed_at_eof(self):
        # The last section in the file goes through the same gate at EOF.
        content = """
## certificate 

**Syscap:** PKI

**Type:** Directory

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="common-name" typ="string">CN</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        assert len(menus) == 1
        assert menus[0]["path"] == "/certificate"
        assert menus[0]["arguments"][0]["name"] == "common-name"

    def test_duplicate_bare_root_first_occurrence_wins(self):
        # Upstream titles `import` twice; only the first carries **Type:**.
        # The unconfirmed duplicate must not shadow or duplicate it.
        content = """
## import 

**Type:** Command

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="file-name" typ="file">.rsc files</ArgTableRow>
</ArgTable>

## file/local

**Type:** Directory

#### import

Prose-only reprise of the import command.

```ros
[admin@admin] > import file.rsc
```
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        imports = [m for m in menus if m["path"] == "/import"]
        assert len(imports) == 1
        assert imports[0]["type"] == "Command"
        assert [a["name"] for a in imports[0]["arguments"]] == ["file-name"]

    # ── Regression pins: slash-path behavior unchanged ────────────────

    def test_slash_menu_without_type_survives_adjacent_bare_heading(self):
        # Slash-derived menus never set needs_type: a missing **Type:**
        # still defaults to Directory even when flushed by a bare heading.
        content = """
## ip/route

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="gateway" typ="ipAddr">Gateway</ArgTableRow>
</ArgTable>

## log 

**Type:** Directory
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        by_path = {m["path"]: m for m in menus}
        assert set(by_path) == {"/ip/route", "/log"}
        assert by_path["/ip/route"]["type"] == "Directory"
        assert by_path["/ip/route"]["arguments"][0]["name"] == "gateway"

    def test_multiword_prose_heading_does_not_close_current_menu(self):
        # Headings that are not path-like keep the old behavior: ignored
        # WITHOUT closing the current menu, so rows after them still attach.
        content = """
## interface/bonding

**Type:** Directory

### print parameters

<ArgTable c1="Argument" c2="Type" c3="Description">
<ArgTableRow arg="count-only" typ="switch">Show only count</ArgTableRow>
</ArgTable>
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        assert len(menus) == 1
        assert menus[0]["path"] == "/interface/bonding"
        assert menus[0]["arguments"][0]["name"] == "count-only"

    def test_uppercase_prose_heading_never_a_candidate(self):
        content = """
## Overview

Some overview text with a stray **Type:**-looking mention.

## ip/firewall/filter

**Type:** Directory
"""
        path = self._write_temp(content)
        try:
            menus = parse_llms_full(path)
        finally:
            os.unlink(path)
        assert [m["path"] for m in menus] == ["/ip/firewall/filter"]


class TestOverrides:
    """Curated additive overrides (data/overrides.toml).

    Conflict policy: ignore-with-warning (stderr). An override whose
    property already exists upstream, or whose path is unknown, is skipped
    without touching upstream-derived entries, so `make extract` stays green
    when upstream eventually documents the property. A missing overrides
    file is fine ([]); a malformed one raises OverrideError.
    """

    @staticmethod
    def _menu(path: str, arguments: list | None = None) -> dict:
        return {
            "path": path,
            "type": "Directory",
            "flags": [],
            "arguments": list(arguments) if arguments else [],
            "read_only": [],
        }

    def _write_temp(self, content: str, suffix: str = ".toml") -> Path:
        tmp = tempfile.NamedTemporaryFile(mode="w", suffix=suffix, delete=False, encoding="utf-8")
        tmp.write(content)
        tmp.flush()
        tmp.close()
        return Path(tmp.name)

    def test_override_adds_missing_property(self):
        menus = [self._menu("/ip/route", [{"name": "gateway", "type": "ipAddr"}])]
        applied = apply_overrides(
            menus,
            [{"path": "/ip/route", "property": "comment", "type": "string", "description": "Route note"}],
        )
        assert applied == 1
        names = [a["name"] for a in menus[0]["arguments"]]
        assert names == ["gateway", "comment"]
        comment = menus[0]["arguments"][1]
        assert comment["type"] == "string"
        assert comment["description"] == "Route note"
        assert comment["required"] is False

    def test_conflicting_override_ignored_with_warning_upstream_untouched(self, capsys):
        upstream = {"name": "comment", "type": "string", "required": False, "description": "Upstream text"}
        menus = [self._menu("/ip/route", [dict(upstream)])]
        applied = apply_overrides(
            menus,
            [{"path": "/ip/route", "property": "comment", "type": "string", "description": "Override text"}],
        )
        assert applied == 0
        assert menus[0]["arguments"] == [upstream]
        assert "already documents" in capsys.readouterr().err

    def test_conflict_with_flag_or_read_only_also_skipped(self, capsys):
        menus = [self._menu("/ip/route")]
        menus[0]["flags"] = [{"name": "X", "type": "", "description": ""}]
        applied = apply_overrides(
            menus,
            [{"path": "/ip/route", "property": "X", "type": "", "description": ""}],
        )
        assert applied == 0
        assert "already documents" in capsys.readouterr().err

    def test_unknown_path_skipped_with_warning(self, capsys):
        menus = [self._menu("/ip/route")]
        applied = apply_overrides(
            menus,
            [{"path": "/ip/no-such-menu", "property": "comment", "type": "string", "description": ""}],
        )
        assert applied == 0
        assert [m["path"] for m in menus] == ["/ip/route"]
        assert "unknown menu" in capsys.readouterr().err

    def test_missing_overrides_file_is_fine(self):
        assert load_overrides(Path(tempfile.mkdtemp()) / "overrides.toml") == []

    def test_load_overrides_parses_valid_file(self):
        path = self._write_temp(
            '[[overrides]]\npath = "/ip/route"\nproperty = "comment"\n'
            'type = "string"\ndescription = "Route note"\n'
        )
        try:
            assert load_overrides(path) == [
                {"path": "/ip/route", "property": "comment", "type": "string", "description": "Route note"}
            ]
        finally:
            os.unlink(path)

    def test_malformed_overrides_file_fails_loudly(self):
        bad_inputs = [
            "not = [valid",  # invalid TOML
            'overrides = "nope"\n',  # not a list
            '[[overrides]]\nproperty = "comment"\n',  # missing path
            '[[overrides]]\npath = "/ip/route"\n',  # missing property
            '[[overrides]]\npath = "no-slash"\nproperty = "comment"\n',  # bad path
        ]
        for content in bad_inputs:
            path = self._write_temp(content)
            try:
                try:
                    load_overrides(path)
                except OverrideError:
                    pass
                else:
                    raise AssertionError(f"expected OverrideError for {content!r}")
            finally:
                os.unlink(path)

    def test_header_records_overrides_count(self):
        menus = [self._menu("/ip/route")]
        assert "# overrides_applied = 1 (data/overrides.toml)" in generate_toml(menus, overrides_applied=1)
        assert "# overrides_applied = 0 (data/overrides.toml)" in generate_toml(menus)

    def test_override_flows_into_generated_toml(self):
        import tomllib

        menus = finalize_menus([self._menu("/ip/route", [{"name": "gateway", "type": "ipAddr"}])])
        applied = apply_overrides(
            menus,
            [{"path": "/ip/route", "property": "comment", "type": "string", "description": "Route note"}],
        )
        assert applied == 1
        parsed = tomllib.loads(generate_toml(menus, overrides_applied=applied))
        route = next(m for m in parsed["menus"] if m["path"] == "/ip/route")
        by_name = {a["name"]: a for a in route.get("arguments", [])}
        assert by_name["comment"]["type"] == "string"
        assert by_name["comment"]["description"] == "Route note"
        assert by_name["gateway"]["type"] == "ipAddr"
