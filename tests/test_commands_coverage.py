"""Invariant and regression suite over the TRACKED generated command table.

Every assertion here encodes a finding of the manual audit of the upstream
CLI reference (manual.mikrotik.com) that motivated gating bare-root CLI pages
behind a ``**Type:**`` line in scripts/extract_commands.py. The suite guards
the OUTPUT artifact (data/commands.toml), not the extraction code itself:

- it reads ONLY the committed table (never llms-full.txt, which is gitignored
  and absent in CI);
- assertions are deliberately subset/membership-shaped so future upstream doc
  syncs can grow properties without breaking the gate. Exact counts and exact
  property lists are NEVER pinned where upstream may grow.

A failure here means one of two things: either regeneration drifted from a
guaranteed invariant (hygiene class), or an audited slice of real RouterOS
CLI surface silently vanished from what ships to users (coverage classes).
"""

import re
import tomllib
from pathlib import Path

# ── Module-level load: single parse, no network, no fixtures ──────────

COMMANDS_TOML_PATH = Path(__file__).resolve().parents[1] / "data" / "commands.toml"

with COMMANDS_TOML_PATH.open("rb") as _fh:
    _DATA = tomllib.load(_fh)

MENUS: list[dict] = _DATA["menus"]
BY_PATH: dict[str, dict] = {m["path"]: m for m in MENUS}

# Canonical shape observed across the whole table: leading slash, lowercase,
# no whitespace, no traversal. Kept in lockstep with the allowlist the LSP
# applies when loading the embedded table (lsp/src/menus.rs).
_PATH_PATTERN = re.compile(r"^/[a-z0-9][a-z0-9/_.-]*$")

_VALID_MENU_TYPES = {"Directory", "Command", "Settings Directory"}

_ENTRY_SECTIONS = ("flags", "arguments", "read_only")

# Generic add-item names injected by scripts/extract_commands.py. Subtracted
# for upstream-only assertions so injected rows can never mask an upstream
# zero-row regression (see TestMainCommandCoverage).
_GENERIC_NAMES: set[str] = {"comment", "disabled", "place-before", "copy-from"}


def _names(menu: dict, section: str) -> set[str]:
    """Property names recorded under one section of a menu."""
    return {entry["name"] for entry in menu.get(section, [])}


def _total_properties(menu: dict) -> int:
    """All documented properties across flags + arguments + read-only."""
    return sum(len(menu.get(section, [])) for section in _ENTRY_SECTIONS)


def _upstream_total(menu: dict) -> int:
    """Properties excluding generic add-item names.

    Generics are injected only into [[menus.arguments]], so any argument
    whose name is a generic name is discounted; flags/read-only keep their
    full counts. A menu whose only rows are generics scores 0 — exactly the
    upstream regression this guards against.
    """
    total = sum(len(menu.get(section, [])) for section in ("flags", "read_only"))
    total += sum(1 for entry in menu.get("arguments", []) if entry.get("name") not in _GENERIC_NAMES)
    return total


class TestGeneratedTableHygiene:
    """Invariants that must hold for ANY regeneration of data/commands.toml.

    These are structural guarantees downstream consumers rely on: the LSP
    indexes menus by exact path, branches on the ``type`` string, and renders
    property names verbatim. A violation breaks lookups quietly instead of
    failing loudly, hence the explicit gate.
    """

    def test_menu_paths_are_unique(self):
        # menu_by_path-style lookups are built as dicts keyed by path; a
        # duplicate entry would silently shadow its twin and lose properties.
        paths = [m["path"] for m in MENUS]
        duplicates = sorted({p for p in paths if paths.count(p) > 1})
        assert not duplicates, f"duplicate menu paths in generated table: {duplicates}"

    def test_menu_paths_match_canonical_shape(self):
        # The audit found trailing-slash variants of the same menu would land
        # as near-duplicate entries and split completion/diagnostic traffic
        # between "/ip/address/" and "/ip/address". Lowercase-only matches the
        # LSP's character allowlist; anything else gets dropped at load time.
        offenders = [
            m["path"]
            for m in MENUS
            if not _PATH_PATTERN.match(m["path"]) or m["path"].endswith("/")
        ]
        assert not offenders, f"paths violating canonical shape: {offenders[:10]}"

    def test_every_menu_type_is_from_the_known_vocabulary(self):
        # The language server string-matches exactly these three values when
        # deciding verb suggestions vs. property completion. An unexpected
        # value would degrade every menu carrying it without any error.
        bad = {
            m["path"]: m["type"] for m in MENUS if m.get("type") not in _VALID_MENU_TYPES
        }
        assert not bad, f"menus with unknown type values: {bad}"

    def test_every_property_entry_has_a_non_empty_name(self):
        # Rows without a usable name would surface as empty completion items
        # or blank hover cards — worse than omitting them entirely.
        offenders = []
        for m in MENUS:
            for section in _ENTRY_SECTIONS:
                for entry in m.get(section, []):
                    if not entry.get("name"):
                        offenders.append((m["path"], section))
        assert not offenders, f"property entries without a name: {offenders[:10]}"

    def test_enum_values_appear_only_under_arguments(self):
        # Generator contract: enum_values are emitted exclusively inside
        # [[menus.arguments]] blocks — flags and read-only entries never get
        # one. Parsed TOML loses which sub-table a key belonged to, so scan
        # the raw text while tracking the active [[menus.*]] header.
        hosting_sections: set[str | None] = set()
        current_section: str | None = None
        for line in COMMANDS_TOML_PATH.read_text(encoding="utf-8").splitlines():
            stripped = line.strip()
            if stripped.startswith("[[") and stripped.endswith("]]"):
                current_section = stripped
            elif stripped.startswith("enum_values"):
                hosting_sections.add(current_section)
        assert hosting_sections == {
            "[[menus.arguments]]"
        }, f"enum_values leaked into unexpected sections: {hosting_sections}"

    def test_covers_header_count_matches_parsed_menus(self):
        # Header lines 1-2 identify the artifact; line 3 carries the
        # "# Covers:" summary whose trailing "(N menus)" count is computed by
        # the generator from the same payload. If the header ever drifts from
        # the body (hand edits, partial writes), consumers can't trust either.
        lines = COMMANDS_TOML_PATH.read_text(encoding="utf-8").splitlines()
        assert len(lines) >= 3, "generated table must start with a 3-line header"
        assert lines[0].startswith("# MikroTik"), "line 1 must be the title comment"
        assert lines[1].startswith("#"), "line 2 must be a comment (generated-from note)"
        covers = lines[2]
        assert covers.startswith("# Covers:"), "line 3 must be the '# Covers:' summary"
        match = re.search(r"\((\d+) menus\)$", covers)
        assert match, f"'# Covers:' line must end with '(N menus)': {covers!r}"
        assert int(match.group(1)) == len(
            MENUS
        ), "header menu count drifted from parsed [[menus]] payload"


class TestMainCommandCoverage:
    """The main-command checklist from the manual audit.

    These are the menus a working RouterOS configuration touches daily. Each
    was verified BY HAND against the upstream CLI reference during the audit;
    if any loses its documented properties again, hover/completion degrade
    for the most-used part of the grammar.
    """

    REQUIRED_MENUS: list[str] = [
        # IP layer
        "/ip/address",
        "/ip/firewall/filter",
        "/ip/firewall/nat",
        "/ip/firewall/connection",
        "/ip/firewall/mangle",
        "/ip/route",
        "/ip/dns",
        "/ip/dhcp-server",
        "/ip/dhcp-client",
        "/ip/service",
        "/ip/pool",
        # Interfaces
        "/interface/bridge",
        "/interface/bridge/port",
        "/interface/list",
        "/interface/vlan",
        "/interface/wireguard",
        "/interface/ethernet",
        "/interface/bonding",
        "/interface/pppoe-client",
        # IPv6
        "/ipv6/address",
        "/ipv6/route",
        "/ipv6/firewall/filter",
        "/ipv6/nd",
        # Routing
        "/routing/ospf",
        "/routing/bgp",
        "/routing/table",
        # Queues
        "/queue/simple",
        "/queue/tree",
        "/queue/type",
        # System
        "/system/clock",
        "/system/ntp/client",
        "/system/scheduler",
        "/system/script",
        "/system/logging",
        "/system/resource",
        "/system/package/update",
        "/user/group",
        # Tools
        "/tool/ping",
        "/tool/traceroute",
        "/tool/netwatch",
        "/tool/fetch",
        "/tool/torch",
        "/tool/profile",
        "/tool/e-mail",
        "/tool/e-mail/send",
        # PPP, CAPsMAN, containers, storage, user manager, zerotier
        "/ppp/secret",
        "/ppp/profile",
        "/ppp/active",
        "/caps-man/configuration",
        "/container",
        "/disk",
        "/file",
        "/user-manager",
        "/zerotier",
    ]

    # Pure-hierarchy roots: their own pages legitimately carry no ArgTables
    # because the CHILD menus hold the properties (/interface/*, /routing/*).
    # Emptiness is PERMITTED here, never required — e.g. /interface happens to
    # document interface-level flags today. Any other checklist menu MUST
    # have properties; that distinction is exactly what the audit encoded.
    HIERARCHY_ONLY_CONTAINERS: set[str] = {
        "/interface",
        "/routing/ospf",
        "/routing/bgp",
        "/routing/filter",
    }

    def test_required_menus_exist_with_documented_properties(self):
        missing: list[str] = []
        empty: list[str] = []
        for path in self.REQUIRED_MENUS:
            menu = BY_PATH.get(path)
            if menu is None:
                missing.append(path)
                continue
            if path not in self.HIERARCHY_ONLY_CONTAINERS and _total_properties(menu) <= 0:
                empty.append(path)
        assert not missing, f"audited menus missing from generated table: {missing}"
        assert not empty, (
            "audited menus present but without any documented property "
            f"(flags+arguments+read_only == 0): {empty}"
        )

    def test_required_menus_have_upstream_properties_beyond_generics(self):
        # Generic injection (comment/disabled/place-before/copy-from) must
        # never mask an upstream zero-row regression: every non-container
        # checklist menu must carry at least one property that is NOT a
        # generic add-item name.
        empty_upstream = [
            path
            for path in self.REQUIRED_MENUS
            if path not in self.HIERARCHY_ONLY_CONTAINERS
            and BY_PATH.get(path) is not None
            and _upstream_total(BY_PATH[path]) <= 0
        ]
        assert not empty_upstream, (
            "menus kept alive only by generic injection (upstream rows vanished): "
            f"{empty_upstream}"
        )

    def test_top_menu_upstream_floors(self):
        # Minimal upstream floors for the highest-traffic menus: far below
        # today's counts (so ordinary doc growth/shrinkage passes) but well
        # above zero (so a dropped ArgTable fails loudly). Counts exclude
        # generic names (see _upstream_total).
        floors = {
            "/ip/address": 5,
            "/ip/route": 10,
            "/ip/firewall/filter": 30,
            "/ipv6/address": 8,
        }
        for path, floor in floors.items():
            menu = BY_PATH.get(path)
            assert menu is not None, f"top menu {path} vanished from the table"
            upstream = _upstream_total(menu)
            assert upstream >= floor, (
                f"{path} upstream properties collapsed to {upstream} "
                f"(floor {floor}) — upstream ArgTable likely lost"
            )

    def test_hierarchy_only_containers_exist_as_directories(self):
        # The four known pure-container roots must still EXIST (prefix
        # completion and children indexing depend on them) even though their
        # property tables may legitimately be empty.
        for path in sorted(self.HIERARCHY_ONLY_CONTAINERS):
            menu = BY_PATH.get(path)
            assert menu is not None, f"hierarchy container {path} missing"
            assert (
                menu["type"] == "Directory"
            ), f"{path} must remain typed Directory, got {menu['type']!r}"

    def test_formerly_exempted_roots_do_carry_properties(self):
        # During the audit these roots were briefly misjudged as empty
        # hierarchy nodes like /routing/ospf. Post-fix they all document real
        # properties; regressing any of them back to zero would hide entire
        # feature areas (SNMP, certificates, disks, containers…).
        formerly_exempt = [
            "/certificate",
            "/snmp",
            "/disk",
            "/file",
            "/container",
            "/user-manager",
            "/zerotier",
        ]
        empty = [
            path
            for path in formerly_exempt
            if path not in BY_PATH or _total_properties(BY_PATH[path]) <= 0
        ]
        assert not empty, f"roots wrongly empty again (must carry properties): {empty}"

    # ── Property spot checks (subset assertions — upstream may grow) ──

    def test_user_arguments_include_mandatory_credentials(self):
        # /user is the highest-risk bare-root capture: name/group/password
        # are mandatory upstream, and losing the required markers would break
        # snippet-quality completions for user management.
        user = BY_PATH["/user"]
        names = _names(user, "arguments")
        assert {"name", "group", "password"} <= names, f"/user arguments too small: {names}"
        for arg in user["arguments"]:
            if arg["name"] in ("name", "group", "password"):
                assert arg.get("required") is True, f"/user.{arg['name']} must stay required"

    def test_log_read_only_columns_present(self):
        # /log documents only read-only output columns; they power hover for
        # the most common print workflow on the router.
        log = BY_PATH["/log"]
        assert {"buffer", "time", "topics", "message"} <= _names(log, "read_only")

    def test_snmp_documents_enabled_argument(self):
        # /snmp is a Settings Directory whose single most important property
        # is `enabled`; its presence proves settings-style pages survive
        # extraction intact.
        snmp = BY_PATH["/snmp"]
        assert "enabled" in _names(snmp, "arguments")

    def test_radius_arguments_cover_service_address_secret(self):
        radius = BY_PATH["/radius"]
        names = _names(radius, "arguments")
        assert {"service", "address", "secret"} <= names, f"/radius arguments too small: {names}"

    def test_firewall_filter_chain_action_with_action_enum_members(self):
        # chain/action are THE defining properties of firewall rules; action's
        # enum members feed completion, so at least the universal `accept`
        # member must survive (full list intentionally NOT pinned — upstream
        # grows new actions regularly).
        fw_filter = BY_PATH["/ip/firewall/filter"]
        names = _names(fw_filter, "arguments")
        assert "chain" in names, "/ip/firewall/filter lost the chain argument"
        assert "action" in names, "/ip/firewall/filter lost the action argument"
        action = next(a for a in fw_filter["arguments"] if a["name"] == "action")
        assert "accept" in action.get("enum_values", []), "action enum lost the accept member"

    def test_tool_email_send_naming_trap(self):
        # Naming trap from the audit: the docs call it "e-mail" (with hyphen),
        # not "email". /tool/e-mail/send is the send COMMAND nested under the
        # /tool/e-mail SETTINGS page — both must exist under their spelled
        # names or completions for mail sending vanish.
        send = BY_PATH["/tool/e-mail/send"]
        assert send["type"] == "Command", "/tool/e-mail/send must stay a Command"


class TestRootLevelCommands:
    """Root-level CLI commands were ENTIRELY MISSING before the bare-root fix.

    Upstream titles these pages without a leading slash (``## password``), so
    the old extractor skipped every one of them. They are single-word console
    commands users type constantly; if any goes missing again the LSP
    silently loses real CLI surface rather than erroring.
    """

    ROOT_COMMANDS: list[str] = [
        "/import",
        "/password",
        "/quit",
        "/redo",
        "/undo",
        "/beep",
        "/blink",
    ]

    def test_root_commands_present_typed_command(self):
        for path in self.ROOT_COMMANDS:
            menu = BY_PATH.get(path)
            assert menu is not None, f"root command {path} vanished from the table"
            assert (
                menu["type"] == "Command"
            ), f"{path} must stay typed Command, got {menu['type']!r}"

    def test_environment_present(self):
        # /environment rides along with the other bare-root pages but its
        # upstream page types it as a Directory (verbatim **Type:** line), so
        # only existence is pinned — not command-hood.
        menu = BY_PATH.get("/environment")
        assert menu is not None, "/environment vanished from the table"

    def test_import_documents_file_name_argument(self):
        # file-name is the whole point of /import; its absence would mean the
        # command was captured without its ArgTable.
        import_cmd = BY_PATH["/import"]
        assert "file-name" in _names(import_cmd, "arguments")

    def test_password_arguments_cover_credential_trio(self):
        password = BY_PATH["/password"]
        names = _names(password, "arguments")
        assert {
            "old-password",
            "new-password",
            "confirm-new-password",
        } <= names, f"/password arguments too small: {names}"

    def test_safe_mode_is_settings_directory(self):
        safe_mode = BY_PATH["/safe-mode"]
        assert safe_mode is not None, "/safe-mode vanished from the table"
        assert (
            safe_mode["type"] == "Settings Directory"
        ), f"/safe-mode type drifted: {safe_mode['type']!r}"


class TestSectionLeakRegressions:
    """Child pages ordered BEFORE their parent root page upstream.

    In llms-full.txt some child sections precede their parent's root section.
    Pre-fix, such a root absorbed the PREVIOUS entry's Type line and ArgTable
    rows, mis-attributing properties across menus. Each test pins a boundary
    the audit verified by hand: the parent keeps what belongs to it, the
    child stays clean.
    """

    def test_certificate_card_verify_is_self_contained(self):
        # /certificate/card-verify follows the /certificate root page in the
        # docs. It must keep its own pin argument and none of the root's
        # flags (K, L) or the root's common-name argument.
        card_verify = BY_PATH["/certificate/card-verify"]
        assert card_verify["type"] == "Command"
        assert "pin" in _names(card_verify, "arguments"), "card-verify lost its pin argument"
        cert_root = BY_PATH["/certificate"]
        leaked_flags = _names(card_verify, "flags") & _names(cert_root, "flags") & {"K", "L"}
        assert not leaked_flags, f"root certificate flags leaked into card-verify: {leaked_flags}"
        assert (
            "common-name" not in _names(card_verify, "arguments")
        ), "root certificate common-name leaked into card-verify"

    def test_radius_monitor_does_not_inherit_root_arguments(self):
        # service/secret belong to the /radius root page; /radius/monitor is
        # a stats-only command whose rows are all read-only counters.
        monitor = BY_PATH["/radius/monitor"]
        assert monitor["type"] == "Command"
        monitor_args = _names(monitor, "arguments")
        assert "service" not in monitor_args, "root /radius service leaked into monitor"
        assert "secret" not in monitor_args, "root /radius secret leaked into monitor"

    def test_app_owns_container_command_lines_update_does_not(self):
        # container-command-lines documents the /app root page itself; the
        # /app/update command page must not absorb it.
        app = BY_PATH["/app"]
        update = BY_PATH["/app/update"]
        assert "container-command-lines" in _names(app, "arguments")
        assert (
            "container-command-lines" not in _names(update, "arguments")
        ), "app root argument leaked into /app/update"

    def test_queue_type_typed_directory_with_core_arguments(self):
        # Its upstream page literally declares Type: Directory — pre-fix the
        # leaked Type line from the previous section labelled it Command,
        # which would suppress standard directory verbs in completion.
        queue_type = BY_PATH["/queue/type"]
        assert queue_type["type"] == "Directory", f"type drifted: {queue_type['type']!r}"
        assert {"name", "kind"} <= _names(queue_type, "arguments")


class TestExportFixtures:
    """Every property used in docs/export-fixtures/ must exist in the table.

    The fixtures are sanitized anonymized `/export`-style samples (TEST-NET
    addresses, no secrets; hand-built from llms-full.txt property names —
    see docs/export-fixtures/README.md). If a fixture property is missing
    here, either the table lost real CLI surface or the fixture invented a
    name: both must fail loudly.
    """

    FIXTURES_DIR = Path(__file__).resolve().parents[1] / "docs" / "export-fixtures"

    # Print selectors, not properties — never asserted against the table.
    _NON_PROPERTY_KEYS = {"numbers"}

    @classmethod
    def _parse_fixture(cls, text: str) -> list[tuple[str, str, str]]:
        """Parse one .rsc fixture into (menu_path, verb, property) triples."""
        import shlex

        found: list[tuple[str, str, str]] = []
        current_menu: str | None = None
        for raw_line in text.splitlines():
            line = raw_line.strip()
            if not line or line.startswith("#"):
                continue
            if line.startswith("/"):
                current_menu = line.split()[0]
                continue
            assert current_menu is not None, f"property row before any /menu header: {line!r}"
            try:
                tokens = shlex.split(line, posix=True)
            except ValueError:
                tokens = line.split()
            if not tokens:
                continue
            verb = tokens[0]
            for token in tokens[1:]:
                if "=" not in token:
                    continue
                key = token.split("=", 1)[0].lstrip("!+")
                if not key or key in cls._NON_PROPERTY_KEYS:
                    continue
                found.append((current_menu, verb, key))
        return found

    def test_fixtures_directory_holds_expected_samples(self):
        assert self.FIXTURES_DIR.is_dir(), f"missing {self.FIXTURES_DIR}"
        names = sorted(p.name for p in self.FIXTURES_DIR.glob("*.rsc"))
        for expected in ("ip-address.rsc", "ip-route.rsc", "system-scheduler.rsc"):
            assert expected in names, f"expected fixture {expected} missing (found {names})"

    def test_every_fixture_property_exists_in_commands_toml(self):
        assert list(self.FIXTURES_DIR.glob("*.rsc")), "no .rsc fixtures to check"
        unknown_menus: list[str] = []
        unknown_props: list[str] = []
        for fixture in sorted(self.FIXTURES_DIR.glob("*.rsc")):
            for menu_path, verb, prop in self._parse_fixture(
                fixture.read_text(encoding="utf-8")
            ):
                menu = BY_PATH.get(menu_path)
                if menu is None:
                    unknown_menus.append(f"{fixture.name}: {menu_path}")
                    continue
                known = (
                    _names(menu, "flags")
                    | _names(menu, "arguments")
                    | _names(menu, "read_only")
                )
                if prop not in known:
                    unknown_props.append(f"{fixture.name}: {menu_path} {verb} {prop!r}")
        assert not unknown_menus, f"fixture menus missing from table: {unknown_menus}"
        assert not unknown_props, f"fixture properties missing from table: {unknown_props}"

    def test_fixtures_contain_no_secrets(self):
        # Guard against accidental credential commits: fixtures must stay on
        # TEST-NET documentation addresses and carry no secret-looking keys.
        import re

        secret_key = re.compile(r"(password|passwd|secret|private-key|certificate)", re.IGNORECASE)
        real_ip = re.compile(r"(10\.|192\.168\.|172\.(1[6-9]|2\d|3[01])\.)")
        offenders: list[str] = []
        for fixture in sorted(self.FIXTURES_DIR.glob("*.rsc")):
            for menu_path, _verb, prop in self._parse_fixture(
                fixture.read_text(encoding="utf-8")
            ):
                if secret_key.search(prop):
                    offenders.append(f"{fixture.name}: secret-looking key {prop!r}")
            text = fixture.read_text(encoding="utf-8")
            for line in text.splitlines():
                stripped = line.strip()
                if not stripped or stripped.startswith("#"):
                    continue
                if real_ip.search(stripped):
                    offenders.append(f"{fixture.name}: RFC1918 address in {stripped!r}")
        assert not offenders, f"fixture hygiene violations: {offenders}"
