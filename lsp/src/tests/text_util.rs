// White-box: text_util (textutil).
use crate::text_util::*;

#[test]
fn test_verb_role_unified_default_arm() {
    // Reconciliation pin: both hover and signature share this arm.
    assert_eq!(verb_role("frobnicate"), "is a standard RouterOS command");
    assert_eq!(verb_role("ADD"), "creates a new entry");
}

#[test]
fn test_normalize_key_case_folds() {
    assert_eq!(normalize_key("/IP/Address"), "/ip/address");
}

#[test]
fn test_collapse_controls_runs_become_one_space() {
    assert_eq!(collapse_controls("a\nb"), "a b");
    assert_eq!(collapse_controls("a\r\n\tb"), "a b");
    assert_eq!(collapse_controls("plain"), "plain");
}

#[test]
fn test_sanitize_detail_single_line_and_capped() {
    assert_eq!(sanitize_detail_text("a\nb\rc\td"), "a b c d");
    let long = "x".repeat(MAX_DETAIL_CHARS + 50);
    let out = sanitize_detail_text(&long);
    assert_eq!(out.chars().count(), MAX_DETAIL_CHARS + 1);
    assert!(out.ends_with('…'));
}

#[test]
fn test_sanitize_label_segment_controls_and_type_cap() {
    assert_eq!(sanitize_label_segment("name", "string"), "name=string");
    assert_eq!(sanitize_label_segment("a\nb", "x\ry"), "a b=x y");
    let long_type = "t".repeat(MAX_LABEL_TYPE_CHARS + 40);
    let seg = sanitize_label_segment("n", &long_type);
    assert_eq!(
        seg.split('=').nth(1).unwrap().chars().count(),
        MAX_LABEL_TYPE_CHARS
    );
}

#[test]
fn test_sanitize_markdown_links_images_tags() {
    assert_eq!(
        sanitize_markdown_for_hover("see [docs](https://example.com/x) now"),
        "see docs now"
    );
    assert_eq!(
        sanitize_markdown_for_hover("logo ![alt](https://example.com/i.png) end"),
        "logo  end"
    );
    assert_eq!(sanitize_markdown_for_hover("a <b>bold</b> c"), "a bold c");
}

#[test]
fn test_sanitize_markdown_controls_newlines_truncate() {
    assert_eq!(sanitize_markdown_for_hover("a\x01b\x7Fc"), "abc");
    assert_eq!(sanitize_markdown_for_hover("a\n\n\n\nb"), "a\n\nb");
    let long = "x".repeat(MAX_HOVER_DESC_CHARS + 100);
    let out = sanitize_markdown_for_hover(&long);
    assert_eq!(out.chars().count(), MAX_HOVER_DESC_CHARS + 1);
    assert!(out.ends_with('…'));
    assert_eq!(sanitize_markdown_for_hover("plain text"), "plain text");
}

#[test]
fn test_strip_helpers_keep_lone_brackets_and_broken_markup() {
    // Unclosed markup is kept verbatim (no runaway consumption).
    assert_eq!(
        sanitize_markdown_for_hover("a [broken link"),
        "a [broken link"
    );
    assert_eq!(
        sanitize_markdown_for_hover("a <lone bracket"),
        "a <lone bracket"
    );
    assert_eq!(
        sanitize_markdown_for_hover("a < b"),
        "a < b",
        "lone angle without a closing `>` is kept literally"
    );
    assert_eq!(
        sanitize_markdown_for_hover("a ![broken image"),
        "a ![broken image"
    );
}

#[test]
fn test_sanitize_markdown_strips_multiline_script() {
    // Multiline tag spans are stripped (tags go, inner text stays —
    // same as the single-line `a <b>bold</b> c` → `a bold c` contract).
    let out = sanitize_markdown_for_hover("a <script>\nalert(1)\n</script> b");
    assert!(!out.contains('<'));
    assert!(!out.contains('>'));
    assert!(out.contains("alert(1)"));
}

#[test]
fn test_sanitize_markdown_long_link_text_rewritten() {
    // A >512-char link text is rewritten to its text (URL dropped):
    // truncate-then-strip bounds the window instead of a length gate.
    let text = "t".repeat(600);
    let out = sanitize_markdown_for_hover(&format!("[{text}](https://evil.example/x)"));
    assert!(
        !out.contains("https://evil.example"),
        "URL must not survive: {out:?}"
    );
    assert!(out.contains(&text[..100]), "link text kept");
}

#[test]
fn test_type_gloss_known_types() {
    assert!(type_gloss("iface_enum").is_some());
    assert!(type_gloss("ipPrefix").is_some());
    assert!(type_gloss("bool").is_some());
    assert!(type_gloss("string").is_none());
}
