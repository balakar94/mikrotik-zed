// Caps companions and URI validation.
// Copied (not moved) from `lsp/src/server.rs` (`mod tests` L2331-2357, L2359-2452); the original block is
// left untouched. `use super::*` is adapted to `use crate::server::{Server, is_valid_file_uri};` for the new location.
use crate::caps::{MAX_DIAG_BYTES, MAX_DIAG_LINES, MAX_DOC_SIZE, MAX_DOCS};
use crate::diagnostics;
use crate::menus::MenuData;
use crate::server::{Server, is_valid_file_uri};
use std::sync::Arc;

fn synthetic_data() -> Arc<MenuData> {
    Arc::new(MenuData::from_toml_str(
        r#"
[[menus]]
path = "/ip/address"
type = "Directory"
[[menus.arguments]]
name = "address"
type = "ipPrefix"
required = true
[[menus.arguments]]
name = "interface"
type = "iface_enum"
required = true
[[menus]]
path = "/ip/firewall/filter"
type = "Directory"
[[menus.arguments]]
name = "chain"
type = "enum (input | forward | output)"
required = true
[[menus.arguments]]
name = "action"
type = "enum (accept | drop | reject)"
"#,
    ))
}
// ── Caps constants ────────────────────────────────────────────────
//
// The shared caps themselves are pinned by exact value in `caps.rs`
// (and cross-checked by tests/test_enclosure.py). The tests below are
// behavioral companions: they prove each cap is actually enforced.

#[test]
fn test_caps_max_diag_bytes_is_500kb() {
    assert_eq!(MAX_DIAG_BYTES, 500_000);
    // Behavioral companion: a doc larger than 500KB is truncated before
    // diagnosis instead of blowing up.
    let data = synthetic_data();
    let line = "/ip/address add address=1.1.1.1 interface=ether1\n";
    // ~50 bytes per line -> 20k lines = ~1M bytes
    let doc = line.repeat(20_000);
    assert!(doc.len() > 500_000);
    let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///test.rsc");
    // Diagnostics are capped; should not blow up (plus truncation hint)
    assert!(diags.len() <= 3001);
}

#[test]
fn test_caps_max_diag_lines_is_3000() {
    assert_eq!(MAX_DIAG_LINES, 3000);
    let data = synthetic_data();
    let doc = "/unknown/menu add foo=bar\n".repeat(5000);
    let diags = diagnostics::compute_diagnostics(&data, &doc, "file:///test.rsc");
    assert!(
        diags.len() <= 3001,
        "diag lines capped at 3000 plus truncation hint, got {}",
        diags.len()
    );
}

// ── URI validation ────────────────────────────────────────────────

#[test]
fn test_uri_valid_file_uris() {
    assert!(is_valid_file_uri("file:///home/user/test.rsc"));
    assert!(is_valid_file_uri("file:///test.rsc"));
    assert!(is_valid_file_uri("file:///a/b/c/d.rsc"));
}

#[test]
fn test_uri_rejects_untitled() {
    assert!(!is_valid_file_uri("untitled://test.rsc"));
    assert!(!is_valid_file_uri("untitled:Untitled-1"));
}

#[test]
fn test_uri_rejects_http_and_https() {
    assert!(!is_valid_file_uri("http://example.com/test.rsc"));
    assert!(!is_valid_file_uri("https://example.com/test.rsc"));
}

#[test]
fn test_uri_rejects_other_schemes() {
    assert!(!is_valid_file_uri("ftp://example.com/file.rsc"));
    assert!(!is_valid_file_uri("vscode://file/test.rsc"));
    assert!(!is_valid_file_uri("file:/test.rsc")); // only one slash
    assert!(!is_valid_file_uri("/test.rsc"));
    assert!(!is_valid_file_uri(""));
}

#[test]
fn test_uri_rejects_path_traversal() {
    assert!(!is_valid_file_uri("file:///home/../etc/passwd"));
    assert!(!is_valid_file_uri("file:///test/../secret.rsc"));
    assert!(!is_valid_file_uri("file:///a/b/../../c.rsc"));
}

#[test]
fn test_uri_rejects_null_byte() {
    assert!(!is_valid_file_uri("file:///test\0.rsc"));
    assert!(!is_valid_file_uri("file://\0/test.rsc"));
    let uri_with_null = format!("file:///test{}.rsc", '\0');
    assert!(!is_valid_file_uri(&uri_with_null));
}

#[test]
fn test_uri_allows_valid_with_dots_in_name() {
    // Single dot is fine; only an exact ".." segment is traversal.
    // "my..file" and "..hidden" are valid filenames per segment check;
    // only a segment exactly equal to ".." is rejected.
    assert!(is_valid_file_uri("file:///home/user/file.test.rsc"));
    assert!(is_valid_file_uri("file:///home/user/.hidden.rsc"));
    assert!(is_valid_file_uri("file:///home/user/my..file.rsc"));
    assert!(is_valid_file_uri("file:///home/user/..hidden.rsc"));
    assert!(!is_valid_file_uri("file:///home/user/../other.rsc"));
    assert!(!is_valid_file_uri("file:///home/user/.."));
}

// ── didOpen / didChange / didClose handling ───────────────────────
