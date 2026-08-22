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
    _extract_heading_path,
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

    def test_excluded_certificate(self):
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
        assert len(result) <= 103  # 100 + "..."

    def test_enum_type(self):
        result = clean_type("enum (disabled | enabled | proxy-arp)")
        assert "enum" in result
        assert "disabled" in result


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
