// ── Publish + code-action methods ───────────────────────────
//
// `impl Server` diagnosis publication and quick-fix computation,
// extracted verbatim from `server.rs`. Field access needs no changes:
// all `Server` fields are `pub(crate)`.

use crate::caps::MAX_CODE_ACTIONS;
use crate::diagnostics;
use crate::encoding::{
    convert_diagnostic_ranges, lsp_character_to_byte_offset, lsp_position_to_offset,
};
#[cfg(not(test))]
use crate::logging::log_error;
use crate::parser::tokenize_with_spans;
use crate::server::Server;
use crate::server_proto::{Suggestion, wire_position};
use crate::suggest;
#[cfg(not(test))]
use std::io::Write;

impl Server {
    /// Compute diagnostics for `doc` and convert emitted range characters
    /// from internal byte-offset semantics to the negotiated position
    /// encoding. Shared by both push paths (didOpen / didChange) and the
    /// pull handler so they can never diverge.
    pub(crate) fn encoded_diagnostics(
        &self,
        doc_text: &str,
        uri: &str,
    ) -> Vec<diagnostics::Diagnostic> {
        let diags = diagnostics::compute_diagnostics(&self.data, doc_text, uri);
        convert_diagnostic_ranges(diags, doc_text, self.position_encoding)
    }

    /// Build `quickfix` CodeActions for client-echoed diagnostics.
    ///
    /// Eligibility: `source == "rsc-ls"` AND code `unknown-property`,
    /// `unknown-menu`, or `invalid-enum-value`. Anything else — foreign
    /// sources, other codes, missing or mistyped fields — is ignored by
    /// design: a quick-fix must never be invented that cannot be grounded
    /// in the document.
    ///
    /// For each eligible diagnostic the mistyped token is recovered from
    /// the tracked document at the diagnostic's own range (never by
    /// re-parsing our message text) and [`suggest::best_candidate`] picks
    /// a deterministic replacement under the length-aware threshold. The
    /// candidate SET depends on the code:
    ///
    /// - `unknown-property`: the property names of THE menu the
    ///   diagnostic's line belongs to, resolved with the same
    ///   line-resolution machinery the diagnostic pipeline uses
    ///   ([`diagnostics::resolve_menu_for_line`]).
    /// - `unknown-menu`: every known menu path.
    /// - `invalid-enum-value`: the enum members of the argument named by
    ///   the `key=value` token pair whose value span overlaps the
    ///   diagnostic range. The pair is located WITHOUT touching the
    ///   message: the covering logical line is tokenized with spans
    ///   (exactly like signature help), wire positions are mapped into
    ///   logical coordinates via [`diagnostics::LogicalLine::logical_offset_from_physical`],
    ///   and only the pair's KEY is consumed — the replacement text is
    ///   derived from the recovered range slice itself, so even a stale
    ///   range can never splice mismatched text. A quoted typo is
    ///   repaired to a quoted member in the SAME quote style; every
    ///   unresolved link (no menu, no pair, no argument, no members) or
    ///   candidate beyond threshold yields no action rather than a guess.
    ///
    /// Total actions are capped at [`MAX_CODE_ACTIONS`].
    pub(crate) fn compute_code_actions(
        &self,
        uri: &str,
        doc: &str,
        client_diags: &[serde_json::Value],
    ) -> Vec<serde_json::Value> {
        let mut actions = Vec::new();
        if client_diags.is_empty() {
            return actions;
        }
        // One continuation-aware logical-line join per REQUEST, shared by
        // every diagnostic below — not one join per diagnostic.
        let logicals = diagnostics::logical_lines(doc);

        for diag in client_diags {
            if actions.len() >= MAX_CODE_ACTIONS {
                break;
            }
            if diag.get("source").and_then(|s| s.as_str()) != Some(diagnostics::DIAGNOSTIC_SOURCE) {
                continue;
            }
            // LSP Diagnostic.code is number|string; ours are strings, so a
            // numeric or absent code fails this binding and is skipped.
            let Some(code) = diag.get("code").and_then(|c| c.as_str()) else {
                continue;
            };
            // The offending-token range, echoed verbatim into the edit.
            let Some(range) = diag.get("range") else {
                continue;
            };
            let Some((start_line, start_char)) = wire_position(range.get("start")) else {
                continue;
            };
            let Some((end_line, end_char)) = wire_position(range.get("end")) else {
                continue;
            };

            // Recover the mistyped token text from the document itself.
            // Stale ranges (pointing outside the current text) and absurdly
            // long spans yield no suggestion rather than a wild guess.
            let start_off =
                lsp_position_to_offset(doc, start_line, start_char, self.position_encoding);
            let end_off = lsp_position_to_offset(doc, end_line, end_char, self.position_encoding);
            let (start_off, end_off) = match (start_off, end_off) {
                (Ok(s), Ok(e)) if e > s && e - s <= suggest::MAX_SUGGEST_INPUT_BYTES => (s, e),
                _ => continue,
            };
            let input = &doc[start_off..end_off];

            let fix = match code {
                // Candidate set: the property names of THE menu the
                // diagnostic's line belongs to. If that menu cannot be
                // resolved (implicit parent, stale range), skip — never
                // guess across all menus.
                "unknown-property" => {
                    let Some(menu) =
                        diagnostics::resolve_menu_for_line(&self.data, &logicals, start_line)
                    else {
                        continue;
                    };
                    suggest::best_candidate(
                        input,
                        menu.arguments
                            .iter()
                            .chain(menu.flags.iter())
                            .chain(menu.read_only.iter())
                            .map(|a| &a.name),
                    )
                    .map(Suggestion::plain)
                }
                // Candidate set: every known menu path. best_candidate is
                // deterministic, so HashMap iteration order is irrelevant.
                "unknown-menu" => suggest::best_candidate(input, self.data.menu_by_path.keys())
                    .map(Suggestion::plain),
                // Candidate set: the enum members of the `key=value` pair
                // the diagnostic points at, recovered WITHOUT parsing the
                // message. The Rule 5 range covers exactly the value part
                // (quotes included), so the pair is found by matching that
                // range against token VALUE spans in logical coordinates —
                // the same tokenize-the-logical-line walk signature help
                // performs.
                "invalid-enum-value" => {
                    let Some(menu) =
                        diagnostics::resolve_menu_for_line(&self.data, &logicals, start_line)
                    else {
                        continue;
                    };
                    let Some(ll) = diagnostics::covering_logical_line(&logicals, start_line) else {
                        continue;
                    };
                    // Wire characters → byte offsets within their own
                    // physical lines → logical offsets (the conversion
                    // chain the signature-help handler uses); token spans
                    // live in logical coordinates, so the comparison must
                    // happen there.
                    let lines: Vec<&str> = doc.lines().collect();
                    let to_logical = |line_idx: usize, character: usize| -> Option<usize> {
                        let text = lines.get(line_idx)?;
                        let byte =
                            lsp_character_to_byte_offset(text, character, self.position_encoding);
                        ll.logical_offset_from_physical(line_idx, byte)
                    };
                    let (Some(log_start), Some(log_end)) = (
                        to_logical(start_line, start_char),
                        to_logical(end_line, end_char),
                    ) else {
                        continue;
                    };
                    // First key=value token whose VALUE part overlaps the
                    // diagnostic range; only its KEY is consumed — the
                    // repaired text comes from `input`, the exact slice
                    // this edit replaces.
                    let tokens = tokenize_with_spans(ll.text());
                    let Some(key) = tokens.iter().find_map(|t| {
                        let (key_part, _) = crate::parser::split_key_value(&t.text)?;
                        let eq = key_part.len();
                        let value_start = t.start + eq + 1;
                        (log_start < t.end && log_end > value_start).then_some(key_part)
                    }) else {
                        continue;
                    };
                    // Mirror the Rule 5 emitter: only `arguments` carry
                    // enum-typed values, and an argument without resolvable
                    // members never guesses.
                    let Some(arg) = menu.arguments.iter().find(|a| a.name == *key) else {
                        continue;
                    };
                    let members = arg.enum_members();
                    if members.is_empty() {
                        continue;
                    }
                    // Strip quotes exactly like the Rule 5 emitter did
                    // before validating, so distance is measured against
                    // the bare member.
                    let trimmed_input = input.trim();
                    let stripped = trimmed_input.trim_matches('"').trim_matches('\'');
                    let Some(member) = suggest::best_candidate(stripped, members.iter()) else {
                        continue;
                    };
                    // Preserve the value's quote style: a quoted typo is
                    // repaired to a quoted member, a bare one stays bare.
                    // Only a symmetric pair of quotes counts; anything
                    // else (debris, unterminated string) edits as bare.
                    let quote_style = trimmed_input.chars().next().filter(|&q| {
                        (q == '"' || q == '\'')
                            && trimmed_input.len() >= 2
                            && trimmed_input.ends_with(q)
                    });
                    Some(Suggestion {
                        new_text: match quote_style {
                            Some(q) => format!("{q}{member}{q}"),
                            None => member.clone(),
                        },
                        title_subject: member,
                    })
                }
                _ => continue,
            };
            let Some(fix) = fix else {
                continue;
            };

            // serde_json's json! macro cannot take the dynamic URI as a map
            // key, so the per-URI change list is inserted explicitly.
            let mut changes = serde_json::Map::new();
            changes.insert(
                uri.to_string(),
                serde_json::json!([{
                    "range": range,
                    "newText": fix.new_text,
                }]),
            );
            actions.push(serde_json::json!({
                "title": format!("Did you mean '{}'?", fix.title_subject),
                "kind": "quickfix",
                "diagnostics": [diag],
                "edit": { "changes": changes },
            }));
        }
        actions
    }

    /// Emit one `textDocument/publishDiagnostics` notification.
    ///
    /// Write-failure policy (deliberate, documented): a failed write is
    /// logged and SURVIVED — it must NOT kill the server loop. A
    /// notification carries no `id` the client blocks on; transient stdout
    /// backpressure or a momentarily closed client pipe should not take the
    /// session down, and if stdout is truly dead the next request/response
    /// write in [`Server::run`] fails and terminates cleanly anyway.
    /// Request-path failures, by contrast, already surface as JSON-RPC
    /// error responses (or fatal run-loop exits), preserving that split.
    ///
    /// Under `cargo test` the serialized notification is recorded on the
    /// server instead of written (stdout writes would be swallowed by the
    /// test harness capture), giving tests an observable spy.
    pub(crate) fn publish_diagnostics(
        &mut self,
        uri: &str,
        diagnostics: Vec<diagnostics::Diagnostic>,
    ) {
        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "textDocument/publishDiagnostics",
            "params": {
                "uri": uri,
                "diagnostics": diagnostics
            }
        });
        #[cfg(test)]
        {
            self.published.push((uri.to_string(), notif));
        }
        #[cfg(not(test))]
        {
            match serde_json::to_string(&notif) {
                Ok(json) => {
                    let header = format!("Content-Length: {}\r\n\r\n", json.len());
                    let mut stdout = std::io::stdout().lock();
                    if let Err(e) = stdout
                        .write_all(header.as_bytes())
                        .and_then(|_| stdout.write_all(json.as_bytes()))
                        .and_then(|_| stdout.flush())
                    {
                        // Non-fatal by policy — see the doc comment above.
                        log_error!(
                            "failed to write publishDiagnostics notification for {uri:?}: {e}"
                        );
                    }
                }
                Err(e) => {
                    // Serialization failure is a bug, not a client
                    // condition; non-fatal by the same policy.
                    log_error!("failed to serialize publishDiagnostics notification: {e}");
                }
            }
        }
    }
}
