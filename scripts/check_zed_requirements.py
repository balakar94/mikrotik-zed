#!/usr/bin/env python3
"""
Check this extension against Zed's published requirements for extensions.

Scope enforced locally today: the `extension.toml` manifest — its schema
conformance plus the registry policies Zed applies to extensions (ID rules,
semver, grammar pins, language cross-references).

Zed deserializes extension.toml into its ExtensionManifest struct with serde's
default behavior: unknown keys are silently ignored (there is no
deny_unknown_fields), so typos and fields dropped upstream never surface
locally — they surface as registry review rejections or dead configuration at
runtime. This script makes such violations fatal before a release ships.

Checks:
  R1  Required top-level keys present and non-empty.
  R2  No unknown top-level keys (whitelist mirrors ExtensionManifest).
  R3  `id` is kebab-case and contains neither "zed" nor "extension"
      (Zed registry policy).
  R4  `schema_version` == 1.
  R5  `version` is valid semver.
  R6  `authors` is a non-empty list of non-empty strings.
  R7  Every path in `languages`: directory exists relative to the repo root,
      contains a parseable config.toml with non-empty `name` and `grammar`
      strings, and the referenced grammar exists in the [grammars] table.
  R8  Every [grammars.*]: `repository` starts with https:// and `rev` is a
      40-character lowercase hex git SHA.
  R9  Every [language_servers.*]: only {name, languages, language_ids} keys;
      `name` non-empty string; `languages` is a non-empty list whose entries
      equal the `name` of some language config.toml (from R7).
  R10 Only with --online: every grammar rev resolves upstream via the GitHub
      commits API. Never touches the network without the flag.

Usage:
  python3 scripts/check_zed_requirements.py [--file PATH] [--online]

Exit codes:
  0  manifest valid
  1  violations found
  2  IO or usage error (unreadable file, invalid TOML, bad CLI arguments)

Stdlib only; requires Python >= 3.12 (tomllib).
"""

import argparse
import re
import sys
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

# Repo root = parent of scripts/, independent of the current working directory.
REPO_ROOT = Path(__file__).resolve().parent.parent

DEFAULT_MANIFEST_PATH = REPO_ROOT / "extension.toml"

# ── Schema constants ─────────────────────────────────────────────────────────

# Top-level keys accepted by Zed's ExtensionManifest struct. Source of truth:
# crates/extension/src/extension_manifest.rs in zed-industries/zed (main
# branch, verified 2026-08). Upstream serde silently ignores anything else, so
# a key added here without a matching upstream struct field is a bug: it would
# let typos slip through again.
KNOWN_TOP_LEVEL_KEYS: frozenset[str] = frozenset({
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
})

REQUIRED_TOP_LEVEL_KEYS: tuple[str, ...] = (
    "id",
    "name",
    "version",
    "schema_version",
    "authors",
    "description",
    "repository",
)

# Keys allowed inside a [language_servers.<id>] section. Deliberately stricter
# than upstream serde (which also tolerates deprecated/legacy entry keys): this
# repo only ever writes these three, and a tight set catches renames early.
LANGUAGE_SERVER_ENTRY_KEYS: frozenset[str] = frozenset({"name", "languages", "language_ids"})

EXPECTED_SCHEMA_VERSION = 1
ID_FORBIDDEN_SUBSTRINGS: tuple[str, ...] = ("zed", "extension")

KEBAB_CASE_RE = re.compile(r"^[a-z0-9]+(-[a-z0-9]+)*$")
SEMVER_RE = re.compile(r"^\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$")
HEX40_RE = re.compile(r"^[0-9a-f]{40}$")
GITHUB_REPO_URL_RE = re.compile(
    r"^https://github\.com/([A-Za-z0-9._-]+)/([A-Za-z0-9._-]+?)(?:\.git)?/?$"
)

# ── Online verification (R10) ────────────────────────────────────────────────

GITHUB_API_TIMEOUT = 15  # seconds per request
USER_AGENT = "mikrotik-zed check_zed_requirements/1.0"

# Exit codes.
EXIT_OK = 0
EXIT_VIOLATIONS = 1
EXIT_IO_OR_USAGE = 2


class ManifestError(Exception):
    """Manifest cannot be read or parsed (mapped to exit code 2)."""


def load_manifest(path: Path) -> dict:
    """Parse the manifest TOML, raising ManifestError on IO/parse failure."""
    try:
        with open(path, "rb") as handle:
            return tomllib.load(handle)
    except OSError as exc:
        raise ManifestError(f"cannot read manifest {path}: {exc}") from exc
    except tomllib.TOMLDecodeError as exc:
        raise ManifestError(f"invalid TOML in {path}: {exc}") from exc


def _is_nonempty_str(value: object) -> bool:
    return isinstance(value, str) and bool(value.strip())


def _present_and_nonempty(value: object) -> bool:
    """R1 emptiness rule: whitespace-only strings and empty lists are empty."""
    if isinstance(value, str):
        return bool(value.strip())
    if isinstance(value, list):
        return bool(value)
    return True


# ── Checks (pure: no printing, no I/O beyond reading language configs) ──────

def check_required_keys(manifest: dict) -> list[str]:
    """R1: required top-level keys exist and are non-empty."""
    violations = []
    for key in REQUIRED_TOP_LEVEL_KEYS:
        if key not in manifest:
            violations.append(f"missing required top-level key '{key}'")
        elif not _present_and_nonempty(manifest[key]):
            violations.append(f"required top-level key '{key}' is present but empty")
    return violations


def check_unknown_keys(manifest: dict) -> list[str]:
    """R2: every top-level key is part of Zed's ExtensionManifest schema."""
    return [
        f"unknown top-level key '{key}' (not part of Zed's ExtensionManifest schema)"
        for key in manifest
        if key not in KNOWN_TOP_LEVEL_KEYS
    ]


def check_id(manifest: dict) -> list[str]:
    """R3: kebab-case id without registry-forbidden substrings.

    Missing/empty ids are reported by R1; skip here to avoid duplicate noise.
    """
    violations = []
    ext_id = manifest.get("id")
    if not _is_nonempty_str(ext_id):
        return violations
    if KEBAB_CASE_RE.fullmatch(ext_id) is None:
        violations.append(
            f"id '{ext_id}' is not kebab-case "
            "(expected lowercase letters/digits separated by single hyphens)"
        )
    lowered = ext_id.lower()
    for forbidden in ID_FORBIDDEN_SUBSTRINGS:
        if forbidden in lowered:
            violations.append(
                f"id '{ext_id}' must not contain '{forbidden}' (Zed registry policy)"
            )
    return violations


def check_schema_version(manifest: dict) -> list[str]:
    """R4: schema_version is the integer 1 (bool is excluded despite being int)."""
    value = manifest.get("schema_version")
    if value is None:
        return []  # absence reported by R1
    if isinstance(value, bool) or not isinstance(value, int):
        return [f"schema_version must be the integer {EXPECTED_SCHEMA_VERSION}, got {value!r}"]
    if value != EXPECTED_SCHEMA_VERSION:
        return [f"schema_version must be {EXPECTED_SCHEMA_VERSION}, got {value}"]
    return []


def check_version(manifest: dict) -> list[str]:
    """R5: version is valid semver (MAJOR.MINOR.PATCH[-PRERELEASE][+BUILD])."""
    value = manifest.get("version")
    if value is None:
        return []  # absence reported by R1
    if not isinstance(value, str) or SEMVER_RE.fullmatch(value) is None:
        return [f"version {value!r} is not valid semver"]
    return []


def check_authors(manifest: dict) -> list[str]:
    """R6: authors is a non-empty list of non-empty strings."""
    value = manifest.get("authors")
    if value is None:
        return []  # absence reported by R1
    if not isinstance(value, list) or not value:
        return [f"authors must be a non-empty list of strings, got {value!r}"]
    invalid = [entry for entry in value if not _is_nonempty_str(entry)]
    if invalid:
        return [f"authors must contain only non-empty strings, got invalid entries: {invalid!r}"]
    return []


def collect_language_names(manifest: dict, root: Path) -> tuple[list[str], list[str]]:
    """R7: validate every languages/* entry; collect their `name` values.

    Returns (names, violations); `names` feeds the language_servers check (R9).
    """
    names: list[str] = []
    violations: list[str] = []
    entries = manifest.get("languages")
    if entries is None:
        return names, []  # absence reported by R1
    if not isinstance(entries, list) or not entries:
        return names, [f"languages must be a non-empty list of directory paths, got {entries!r}"]

    raw_grammars = manifest.get("grammars")
    grammars = raw_grammars if isinstance(raw_grammars, dict) else {}

    for entry in entries:
        if not _is_nonempty_str(entry):
            violations.append(f"languages entries must be non-empty strings, got {entry!r}")
            continue
        lang_dir = root / entry
        if not lang_dir.is_dir():
            violations.append(f"[languages] '{entry}': directory not found relative to {root}")
            continue
        config_path = lang_dir / "config.toml"
        if not config_path.is_file():
            violations.append(f"[languages] '{entry}': missing config.toml")
            continue
        try:
            config = tomllib.loads(config_path.read_text(encoding="utf-8"))
        except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as exc:
            violations.append(f"[languages] '{entry}': config.toml is not parseable ({exc})")
            continue
        name = config.get("name")
        if _is_nonempty_str(name):
            names.append(name)
        else:
            violations.append(f"[languages] '{entry}' config.toml: 'name' must be a non-empty string")
        grammar = config.get("grammar")
        if not _is_nonempty_str(grammar):
            violations.append(f"[languages] '{entry}' config.toml: 'grammar' must be a non-empty string")
        elif grammar not in grammars:
            violations.append(
                f"[languages] '{entry}' config.toml: grammar '{grammar}' not found in the [grammars] table"
            )
    return names, violations


def check_grammars(manifest: dict) -> list[str]:
    """R8: https repository URLs and 40-char lowercase hex revs."""
    violations: list[str] = []
    grammars = manifest.get("grammars")
    if grammars is None:
        return violations  # a missing table surfaces via R7 cross-reference
    if not isinstance(grammars, dict):
        return [f"grammars must be a table of [grammars.<name>] sections, got {grammars!r}"]
    for grammar_name, entry in grammars.items():
        prefix = f"grammars.{grammar_name}"
        if not isinstance(entry, dict):
            violations.append(f"{prefix}: must be a table")
            continue
        repository = entry.get("repository")
        if not isinstance(repository, str) or not repository.startswith("https://"):
            violations.append(f"{prefix}.repository must start with https://, got {repository!r}")
        rev = entry.get("rev")
        if not isinstance(rev, str) or HEX40_RE.fullmatch(rev) is None:
            violations.append(
                f"{prefix}.rev must be a 40-character lowercase hex git SHA, got {rev!r}"
            )
    return violations


def check_language_servers(manifest: dict, language_names: list[str]) -> list[str]:
    """R9: strict entry shape, with `languages` values bound to real languages."""
    violations: list[str] = []
    servers = manifest.get("language_servers")
    if servers is None:
        return violations
    if not isinstance(servers, dict):
        return [f"language_servers must be a table of [language_servers.<id>] sections, got {servers!r}"]

    known = sorted(set(language_names))
    known_hint = f" (known: {', '.join(known)})" if known else ""
    for server_id, entry in servers.items():
        prefix = f"language_servers.{server_id}"
        if not isinstance(entry, dict):
            violations.append(f"{prefix}: must be a table")
            continue
        for key in entry:
            if key not in LANGUAGE_SERVER_ENTRY_KEYS:
                allowed = ", ".join(sorted(LANGUAGE_SERVER_ENTRY_KEYS))
                violations.append(f"{prefix}: unknown key '{key}' (allowed: {allowed})")
        name = entry.get("name")
        if not _is_nonempty_str(name):
            violations.append(f"{prefix}: 'name' must be a non-empty string")
        languages = entry.get("languages")
        if not isinstance(languages, list) or not languages:
            violations.append(f"{prefix}: 'languages' must be a non-empty list of strings")
            continue
        for language in languages:
            if not _is_nonempty_str(language):
                violations.append(
                    f"{prefix}: 'languages' entries must be non-empty strings, got {language!r}"
                )
            elif language not in known:
                violations.append(
                    f"{prefix}: languages entry '{language}' does not match any "
                    f"language config.toml 'name'{known_hint}"
                )
        # `language_ids` is allowed but unconstrained here: upstream maps it to
        # arbitrary LSP language identifiers, nothing local to validate against.
    return violations


def _github_owner_repo(url: str) -> tuple[str, str] | None:
    """Extract (owner, repo) from an https GitHub URL, else None."""
    match = GITHUB_REPO_URL_RE.match(url)
    if match is None:
        return None
    return match.group(1), match.group(2)


def check_grammar_revs_online(manifest: dict) -> list[str]:
    """R10 (--online): every grammar rev exists upstream on GitHub.

    Runs only when explicitly requested; HTTP 200 means the pinned rev is real.
    Shape problems are intentionally not re-reported here (offline checks own
    them); this check only adds network-verifiable violations.
    """
    violations: list[str] = []
    grammars = manifest.get("grammars")
    if not isinstance(grammars, dict):
        return violations
    for grammar_name, entry in grammars.items():
        if not isinstance(entry, dict):
            continue
        repository = entry.get("repository")
        rev = entry.get("rev")
        if not isinstance(repository, str) or not isinstance(rev, str):
            continue  # already reported by check_grammars
        owner_repo = _github_owner_repo(repository)
        if owner_repo is None:
            violations.append(
                f"grammars.{grammar_name}: cannot verify rev online — "
                f"not a GitHub repository URL: {repository!r}"
            )
            continue
        owner, repo = owner_repo
        url = f"https://api.github.com/repos/{owner}/{repo}/commits/{rev}"
        status: int
        try:
            request = urllib.request.Request(
                url,
                headers={"User-Agent": USER_AGENT, "Accept": "application/vnd.github+json"},
            )
            with urllib.request.urlopen(request, timeout=GITHUB_API_TIMEOUT) as response:
                status = response.status
        except urllib.error.HTTPError as exc:
            status = exc.code
        except urllib.error.URLError as exc:
            reason = getattr(exc, "reason", exc)
            violations.append(
                f"grammars.{grammar_name}: rev verification request failed ({reason}) for {url}"
            )
            continue
        if status != 200:
            violations.append(
                f"grammars.{grammar_name}: rev {rev} not found upstream (HTTP {status}) at {url}"
            )
    return violations


# ── Orchestration ────────────────────────────────────────────────────────────

def run_checks(manifest: dict, root: Path, *, online: bool = False) -> list[tuple[str, list[str]]]:
    """Run every check in deterministic order as (check name, violations)."""
    language_names, language_violations = collect_language_names(manifest, root)
    sections: list[tuple[str, list[str]]] = [
        ("required top-level keys", check_required_keys(manifest)),
        ("known top-level keys", check_unknown_keys(manifest)),
        ("id policy", check_id(manifest)),
        ("schema_version", check_schema_version(manifest)),
        ("version", check_version(manifest)),
        ("authors", check_authors(manifest)),
        ("languages", language_violations),
        ("grammars", check_grammars(manifest)),
        ("language_servers", check_language_servers(manifest, language_names)),
    ]
    if online:
        sections.append(("grammar revs upstream", check_grammar_revs_online(manifest)))
    return sections


def validate(manifest: dict, root: Path, *, online: bool = False) -> list[str]:
    """All violations as a flat list; empty list means the manifest is valid."""
    return [
        violation
        for _, violations in run_checks(manifest, root, online=online)
        for violation in violations
    ]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        prog="check_zed_requirements.py",
        description="Check extension.toml against Zed's requirements for extensions "
        "(manifest schema + registry policy).",
    )
    parser.add_argument(
        "--file",
        metavar="PATH",
        default=None,
        help=f"manifest to validate (default: {DEFAULT_MANIFEST_PATH}); "
        "language directories resolve against the repo root either way",
    )
    parser.add_argument(
        "--online",
        action="store_true",
        help="also verify grammar revs exist upstream (network calls to api.github.com)",
    )
    args = parser.parse_args(argv)

    manifest_path = Path(args.file) if args.file is not None else DEFAULT_MANIFEST_PATH
    try:
        manifest = load_manifest(manifest_path)
    except ManifestError as exc:
        print(f"[error] {exc}", file=sys.stderr)
        return EXIT_IO_OR_USAGE

    error_count = 0
    for check_name, violations in run_checks(manifest, REPO_ROOT, online=args.online):
        if violations:
            for violation in violations:
                print(f"[error] {violation}")
            error_count += len(violations)
        else:
            print(f"[ok] {check_name}")
    print(f"{manifest_path.name}: {error_count} error(s)")
    return EXIT_OK if error_count == 0 else EXIT_VIOLATIONS


if __name__ == "__main__":
    sys.exit(main())
