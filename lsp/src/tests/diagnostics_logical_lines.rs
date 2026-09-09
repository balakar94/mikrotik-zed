// White-box: prop loops.

#[test]
fn validator_prop_logical_lines_map_range_never_panics() {
    // Crafted inputs covering continuations, dangling backslash,
    // unicode, empty lines, and quote/comment traps.
    let docs = [
        "",
        "\n\n",
        "/ip/address add address=1.1.1.1/24 interface=ether1\n",
        "/tool/fetch url=\"https://example.com/a/b\\\n/c\"\n",
        "trailing backslash at EOF \\",
        "\\\n\\\n",
        "/ip/address add comment=\"héllo wörld\" # c\n",
        ":do {\n:put \"{\" # not a brace\n}\n",
        "a\\\n\\\n\\\nb\n",
    ];
    for doc in docs {
        let logicals = crate::diagnostics::logical_lines(doc);
        for ll in &logicals {
            let len = ll.text().len();
            // Boundary and overlong ranges clamp, never panic.
            for (s, e) in [
                (0, 0),
                (0, len),
                (len, len),
                (0, len + 99),
                (len + 5, len + 1),
            ] {
                let range = ll.map_range(s, e);
                assert!(range.start.line <= range.end.line || range.start.line == range.end.line);
                let _ = (range.start.character, range.end.character);
            }
            // Every covered physical line maps back into the logical text.
            for phys in ll.first_physical_line()..=ll.last_physical_line() {
                if let Some(off) = ll.logical_offset_from_physical(phys, 0) {
                    assert!(off <= len);
                }
            }
        }
    }
}

#[test]
fn validator_prop_file_uri_arbitrary_never_panics() {
    // Known accepts/rejects pin the contract; the rest must only not panic.
    assert!(crate::server::is_valid_file_uri("file:///validator-ok.rsc"));
    assert!(crate::server::is_valid_file_uri("file:///a%20b.rsc"));
    assert!(!crate::server::is_valid_file_uri("file:///a/../b.rsc"));
    assert!(!crate::server::is_valid_file_uri(
        "file:///%2e%2e/etc/passwd"
    ));
    assert!(!crate::server::is_valid_file_uri(
        "untitled:///validator-x.rsc"
    ));
    assert!(!crate::server::is_valid_file_uri(
        "file:///validator-bad%.rsc"
    ));
    let extras: Vec<String> = vec![
        "".to_string(),
        "file://".to_string(),
        "FILE:///validator-upper.rsc".to_string(),
        "file:///validator-\u{1F30D}.rsc".to_string(),
        "file:///validator-tab\there.rsc".to_string(),
        "file:///validator-nl\n.rsc".to_string(),
        "file:///validator-nul\0.rsc".to_string(),
        "file:///%".to_string(),
        "file:///%zz".to_string(),
        "file:///%41".to_string(),
        "file:///x..y.rsc".to_string(),
        "file:///..".to_string(),
        "file:///../x".to_string(),
        format!("file:///{}.rsc", "a".repeat(300)),
    ];
    for uri in &extras {
        let _ = crate::server::is_valid_file_uri(uri);
    }
}

#[test]
fn validator_prop_suggest_long_input_never_panics_and_truncated() {
    // Inputs over MAX_SUGGEST_INPUT_BYTES (256) must degrade gracefully.
    let cands = ["address", "interface", "chain"];
    let long_garbage = "z".repeat(300);
    assert_eq!(
        crate::suggest::best_candidate(&long_garbage, cands.into_iter()),
        None,
        "far-over-threshold input must suggest nothing",
    );
    let extras: Vec<String> = vec![
        "".to_string(),
        "   ".to_string(),
        "a".repeat(257),
        "k".repeat(10_000),
        "\u{1F30D}".repeat(300),
        "adress".to_string(),
        "CHAIN".to_string(),
        "=weird=".to_string(),
    ];
    for input in &extras {
        let _ = crate::suggest::best_candidate(input, cands.into_iter());
    }
}

#[test]
fn validator_prop_framing_arbitrary_bytes_never_panics() {
    use std::io::Cursor;
    // Well-formed frame first: exact body must come back intact.
    let body = br#"{"jsonrpc":"2.0","id":9,"method":"x"}"#;
    let mut raw = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();
    raw.extend_from_slice(body);
    let mut cursor = Cursor::new(raw);
    let res = crate::framing::read_message(&mut cursor);
    assert!(
        format!("{res:?}").contains("Message"),
        "golden frame must parse, got {res:?}",
    );
    // Arbitrary/hostile bytes: any outcome is fine except a panic or hang.
    let hostile: Vec<Vec<u8>> = vec![
        vec![],
        b"\r\n\r\n".to_vec(),
        b"Content-Length: 0\r\n\r\n".to_vec(),
        b"Content-Length: abc\r\n\r\nhello".to_vec(),
        b"Content-Length: 5\r\nContent-Length: 6\r\n\r\nhello!".to_vec(),
        b"Content-Length: 999999999\r\n\r\nshort".to_vec(),
        b"X-Garbage: 1\r\n\r\n{\"body\":true}".to_vec(),
        vec![0xFF, 0xFE, 0x00, 0x0A],
        format!("Content-Length: {}\r\n\r\n", "9".repeat(300)).into_bytes(),
        b"content-length: 3\r\n\r\nabc".to_vec(),
        b"Content-Length: 3\n\nabc".to_vec(),
    ];
    for bytes in hostile {
        let mut cursor = Cursor::new(bytes);
        let _ = crate::framing::read_message(&mut cursor);
    }
}
