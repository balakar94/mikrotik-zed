// Quote, stray-brace and syntax cap rules.
// Copied (not moved) from `lsp/src/diagnostics.rs` (`mod syntax_rules` L3141-3170, L3357-3477); the original block is
// left untouched. `use super::*` is adapted to `use crate::diagnostics::*;` for the new location.
use crate::diagnostics::*;
use crate::menus::MenuData;

fn synth() -> MenuData {
    MenuData::from_toml_str(
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
path = "/tool/fetch"
type = "Command"
[[menus.arguments]]
name = "url"
type = "string"
[[menus.arguments]]
name = "ssl-verify"
type = "bool"
"#,
    )
}

fn codes(diags: &[Diagnostic]) -> Vec<&str> {
    diags.iter().filter_map(|d| d.code.as_deref()).collect()
}
#[test]
fn test_stray_close_brace_reported_at_char() {
    // '}' with an empty stack → unmatched-brace at that exact character.
    let doc = "}\n:put x\n";
    let diags = compute_diagnostics(&synth(), doc, "file:///a.rsc");
    assert_eq!(diags.len(), 1, "got {diags:?}");
    let d = &diags[0];
    assert_eq!(d.code.as_deref(), Some("unmatched-brace"));
    assert_eq!(d.severity, Some(severity::ERROR));
    assert_eq!(d.message, "Unmatched '}': no '{' is open at this point");
    assert_eq!(
        d.range.start,
        Position {
            line: 0,
            character: 0
        }
    );
    assert_eq!(
        d.range.end,
        Position {
            line: 0,
            character: 1
        }
    );
}

#[test]
fn test_close_after_balanced_block_only_flags_the_extra() {
    // A well-formed block closes cleanly; only the EXTRA '}' is stray.
    let doc = ":do {\n:put x\n}\n}\n";
    let diags = compute_diagnostics(&synth(), doc, "file:///a.rsc");
    assert_eq!(diags.len(), 1, "got {diags:?}");
    assert_eq!(diags[0].code.as_deref(), Some("unmatched-brace"));
    assert_eq!(
        diags[0].range.start,
        Position {
            line: 3,
            character: 0
        }
    );
}

#[test]
fn test_empty_and_whitespace_docs_no_syntax_diagnostics() {
    let data = synth();
    assert!(compute_diagnostics(&data, "", "file:///a.rsc").is_empty());
    assert!(compute_diagnostics(&data, "   \n\n\t\n  ", "file:///a.rsc").is_empty());
}

#[test]
fn test_syntax_rule_runs_alongside_menu_rules_in_one_publish() {
    // Interaction contract: menu-rule diagnostics and syntax-rule
    // diagnostics flow through the same compute_diagnostics result.
    let doc = "/foo/bar add x=1\ndo {\n";
    let diags = compute_diagnostics(&synth(), doc, "file:///a.rsc");
    let c = codes(&diags);
    assert!(c.contains(&"unknown-menu"), "menu rule fired, got {c:?}");
    assert!(
        c.contains(&"unclosed-brace"),
        "syntax rule fired, got {c:?}"
    );
}

#[test]
fn test_syntax_findings_from_all_three_kinds_sort_globally() {
    // Ordering contract for the deferred-materialization walk: findings
    // from all three sources (stray close, unclosed open, unterminated
    // quote) are sorted globally by document position — no assumption
    // about which kind precedes which is allowed.
    //
    // Lines 0–1 are stray '}' with an empty stack; lines 2–3 open '{'
    // that never close; line 4 opens a quote that swallows only its own
    // tail (it is last, so no later event is masked).
    let doc = "}\n}\n{\n{\n:put \"x\n";
    let diags = compute_diagnostics(&synth(), doc, "file:///a.rsc");
    let starts: Vec<_> = diags
        .iter()
        .map(|d| (d.code.as_deref().unwrap_or(""), d.range.start.clone()))
        .collect();
    assert_eq!(
        starts,
        vec![
            (
                "unmatched-brace",
                Position {
                    line: 0,
                    character: 0
                }
            ),
            (
                "unmatched-brace",
                Position {
                    line: 1,
                    character: 0
                }
            ),
            (
                "unclosed-brace",
                Position {
                    line: 2,
                    character: 0
                }
            ),
            (
                "unclosed-brace",
                Position {
                    line: 3,
                    character: 0
                }
            ),
            (
                "unclosed-quote",
                Position {
                    line: 4,
                    character: 5
                }
            ),
        ],
        "all three kinds interleaved in one document-ordered publish, got {starts:?}"
    );
}
