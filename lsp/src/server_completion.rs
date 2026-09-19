// ── Completion request handler ───────────────────────────────────────────
//
// `textDocument/completion` request/response mapping, extracted verbatim
// from `server.rs` so the dispatch match stays a thin protocol router:
// context preparation (wire position → byte offset, continuation-aware
// logical prefix, live-enrichment trigger), item computation through
// `completion.rs`, and the logical/physical `textEdit` range injection.
// Pure completion logic stays in `completion.rs`.

use crate::caps::MAX_COMPLETION_ITEMS;
use crate::completion;
use crate::diagnostics;
use crate::encoding::{PositionEncoding, byte_offset_to_utf16_units, lsp_character_to_byte_offset};
use crate::live::{LiveCache, trigger_enrichment_for_completion};
use crate::logging::{log_debug, log_warn, uri_for_log};
use crate::parser::{build_before_cursor_from_lines, parse_line};
use crate::server::Server;
use crate::server_proto::invalid_params_response;

impl Server {
    /// Handle one `textDocument/completion` REQUEST and return its response.
    ///
    /// `params` is the WHOLE JSON-RPC message (the dispatch-loop convention);
    /// `id` is moved into the response.
    pub(crate) fn handle_completion(
        &mut self,
        id: serde_json::Value,
        params: &serde_json::Value,
    ) -> serde_json::Value {
        // Requests must always be answered: malformed params →
        // -32602, untracked URI (never opened, closed, or rejected
        // at MAX_DOCS) → spec-permitted null result. Never silence.
        let Some(uri) = params["params"]["textDocument"]["uri"].as_str() else {
            return invalid_params_response(&id, "missing textDocument.uri");
        };
        let pos = &params["params"]["position"];
        let Some(line) = pos["line"].as_u64() else {
            return invalid_params_response(&id, "missing position.line");
        };
        let Some(character) = pos["character"].as_u64() else {
            return invalid_params_response(&id, "missing position.character");
        };
        let Some(doc) = self.docs.get(uri) else {
            log_debug!(
                "completion for untracked URI, returning null result: {}",
                uri_for_log(uri)
            );
            return serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": null,
            });
        };

        // Convert the wire `character` ONCE into a byte offset within
        // the cursor line, taken from the single `str::lines` split
        // below that `build_before_cursor_from_lines` also consumes,
        // so encoding math cannot diverge from the string sliced.
        let line_idx = line as usize;
        let lines: Vec<&str> = doc.lines().collect();
        let current_line = lines.get(line_idx).copied().unwrap_or("");
        let char_byte =
            lsp_character_to_byte_offset(current_line, character as usize, self.position_encoding);
        let before_cursor = build_before_cursor_from_lines(&lines, line_idx, char_byte);
        // Live enrichment: stale-while-revalidate (non-blocking).
        // Completion only reads fresh cache; misses trigger a background
        // thread that hydrates for the next keystroke. Coalescing and
        // negative TTL prevent retry spam. Never logs pass.
        if self.live_config.is_active() {
            let context = parse_line(&self.data, &before_cursor);
            // Property key of the completion target: the `key` of a
            // trailing `key=value` token, if the cursor is inside a
            // property assignment.
            let tokens = crate::parser::tokenize(&before_cursor);
            let property = tokens.last().and_then(|tok| {
                crate::parser::split_key_value(tok).map(|(key, _)| key.trim_start_matches(':'))
            });
            // Menu-declared argument type for the property (empty
            // string when the property is unknown to the menu).
            let arg_type = property
                .and_then(|key| {
                    self.data
                        .menu_by_path
                        .get(&crate::text_util::normalize_path(&context.path))
                        .and_then(|menu| {
                            menu.arguments
                                .iter()
                                .find(|a| a.name.eq_ignore_ascii_case(key))
                        })
                        .map(|arg| arg.arg_type.as_str())
                })
                .unwrap_or("");
            trigger_enrichment_for_completion(
                &self.live_cache,
                &self.live_config,
                property,
                &context.path,
                arg_type,
            );
        }
        // Continuation-aware logical prefix up to the cursor: the
        // physical line alone cannot see a path segment split across a
        // `\` join. Computed once here and reused by the textEdit
        // injection below, so the completion layer and the physical
        // range mapping agree on the joined text. The parse-cache
        // lookup is the single full-document hash for this request.
        let logicals = self.parse_cache.lookup_or_insert(uri, doc);
        let covering = diagnostics::covering_logical_line(logicals, line_idx);
        let cursor_logical_opt =
            covering.and_then(|ll| ll.logical_offset_from_physical(line_idx, char_byte));
        let logical_prefix_text: Option<String> = match (covering, cursor_logical_opt) {
            (Some(ll), Some(cursor_logical)) => {
                let text = ll.text();
                let clamped =
                    crate::encoding::floor_char_boundary(text, cursor_logical.min(text.len()));
                Some(text[..clamped].to_string())
            }
            _ => None,
        };
        let mut items = {
            // Non-blocking live read: `try_lock` never waits for a
            // background fetch holding the cache. `WouldBlock` falls
            // back to the static snapshot (`None`); a poisoned mutex
            // recovers the inner guard as before. The guard lives only
            // for the synchronous cache read inside
            // `compute_completions_with_logical` (fresh-entry `Arc`
            // clones); the long textEdit injection below runs lock-free.
            match self.live_cache.try_lock() {
                Ok(live_guard) => completion::compute_completions_with_logical(
                    &self.data,
                    &before_cursor,
                    logical_prefix_text.as_deref(),
                    Some(&*live_guard as &LiveCache),
                ),
                Err(std::sync::TryLockError::WouldBlock) => {
                    log_debug!("live cache busy, completing from static snapshot");
                    completion::compute_completions_with_logical(
                        &self.data,
                        &before_cursor,
                        logical_prefix_text.as_deref(),
                        None,
                    )
                }
                Err(std::sync::TryLockError::Poisoned(e)) => {
                    log_warn!("live cache lock poisoned, recovering");
                    let live_guard = e.into_inner();
                    completion::compute_completions_with_logical(
                        &self.data,
                        &before_cursor,
                        logical_prefix_text.as_deref(),
                        Some(&*live_guard as &LiveCache),
                    )
                }
            }
        };

        // ── textEdit injection (C-02 logical vs physical) ────────
        // Populate `textEdit` so accepting a completion replaces the
        // already-typed prefix instead of inserting beside it
        // (`in` + `input` → `input`, not `ininput`). `insertText` is
        // retained as fallback for clients that ignore `textEdit` (Zed
        // supports it). If computing the range fails we leave `textEdit`
        // as `None` and the client falls back to insertion at cursor.
        //
        // Four cases are handled (per spec, at least these):
        // - value completions after `=`: range covers the typed suffix
        //   after `=` (excluding a leading opening quote so `"in` → `input`
        //   preserves the quote as `"input`);
        // - sub-menu / verb completions before a verb: when the cursor
        //   sits inside a partial token that prefixes a child name, range
        //   covers that token so `addr` → `address`;
        // - partial menu-path segment completions (`/ip/addr`): range
        //   covers only the typed final segment so `addr` → `address`
        //   while the already-typed `/ip/` prefix is preserved.
        // - property / flag completions after a verb (kinds 5/14): the
        //   same partial-name span as the sub-menu case, so `inter` →
        //   `interface=…` replaces instead of appending. Both branches share
        //   `completion::partial_name_span` with the completion layer (no
        //   `=`, excluded `/ : " ' ( [ $` leaders, mid-token cursor).
        // For all other cases (e.g., already-finished token + space) the
        // edit is zero-length at the cursor (pure insertion).
        //
        // Logical vs physical: RouterOS `\`-continued commands join
        // several physical lines into one logical line. `before_cursor`
        // is a logical join (via `build_before_cursor`), but
        // `line_text`/`char_byte` are physical. We map the cursor into
        // logical coordinates with `diagnostics::logical_lines` +
        // `LogicalLine::logical_offset_from_physical` (as signatureHelp
        // does), compute the prefix range in logical space, then map it
        // back with `LogicalLine::map_range` to physical line/character.
        // This makes a cursor on continuation line 2 (e.g.
        // `gateway=1.1.1.1 \` + `comment="x"`) cover the logical token
        // correctly while staying byte- and UTF-16-correct.
        {
            let line_text = current_line;
            // No pre-clear pass is needed: every completion-layer
            // `textEdit` shadow is overwritten by the mapping below.
            // The shadow carriers are a closed set — sub-menu / verb
            // (kinds 9/3), partial menu-path segment (kind 9) and value
            // (kind 12) — and each has a matching branch here whose
            // range is always produced (logical mapping or the
            // same-token physical fallback). Property / flag items
            // carry no shadow at all. A line-0 guess therefore cannot
            // reach the wire; if a new shadow-carrying kind is added,
            // extend the filters below in lockstep or the stale line-0
            // range will be serialized.
            // Value vs non-value decision uses the same tolerant trimmed
            // logic as `completion::match_context` — driven by the
            // logical `before_cursor` (continuation-aware).
            let trimmed_bc = before_cursor.trim_end();
            let has_trailing_ws = trimmed_bc.len() != before_cursor.len();
            let trimmed_last = crate::parser::tokenize(trimmed_bc)
                .last()
                .cloned()
                .unwrap_or_default();
            let mut value_range_phys: Option<(usize, usize, usize, usize)> = None;
            // (phys_start_line, phys_start_char_byte, phys_end_line, phys_end_char_byte) in
            // byte offsets
            // Logical join, covering line and cursor mapping were
            // computed once before the completion call above; reusing
            // them keeps a single full-document hash per request and
            // guarantees the mapping matches the completion context.
            if let Some((key_part, value_part)) = crate::parser::split_key_value(&trimmed_last) {
                let _ = key_part;
                let raw_suffix = value_part;
                let trimmed_suffix = raw_suffix.trim_matches(|c| c == '"' || c == '\'');
                if !has_trailing_ws || trimmed_suffix.is_empty() {
                    // Value context confirmed.
                    // Try logical mapping.
                    let mut logical_success = false;
                    if let (Some(ll), Some(cursor_logical)) = (covering, cursor_logical_opt) {
                        let logical_text = ll.text();
                        let cursor_logical_clamped = cursor_logical.min(logical_text.len());
                        let cursor_logical_clamped = crate::encoding::floor_char_boundary(
                            logical_text,
                            cursor_logical_clamped,
                        );
                        let logical_prefix = &logical_text[..cursor_logical_clamped];
                        let tokens = crate::parser::tokenize_with_spans(logical_prefix);
                        if let Some(tok) = tokens.last() {
                            if let Some((key_part, _)) = crate::parser::split_key_value(&tok.text) {
                                let pos = key_part.len();
                                let suffix_part = &tok.text[pos + 1..];
                                // Same effective span the completion
                                // layer uses: preserve a leading
                                // opening quote and leave a trailing
                                // closing quote in place.
                                let (span_start, span_end) =
                                    completion::value_replacement_span(suffix_part);
                                let base = tok.start + pos + 1;
                                let (log_s, log_e) = if has_trailing_ws && trimmed_suffix.is_empty()
                                {
                                    (cursor_logical_clamped, cursor_logical_clamped)
                                } else {
                                    (base + span_start, base + span_end)
                                };
                                let log_s = log_s.min(logical_text.len()).min(log_e);
                                let log_e = log_e.min(logical_text.len());
                                let log_s =
                                    crate::encoding::floor_char_boundary(logical_text, log_s);
                                let log_e =
                                    crate::encoding::floor_char_boundary(logical_text, log_e);
                                let range = ll.map_range(log_s, log_e);
                                // Convert physical byte offsets per line for utf16 later;
                                // store as (start_line, start_byte, end_line, end_byte)
                                value_range_phys = Some((
                                    range.start.line as usize,
                                    range.start.character as usize,
                                    range.end.line as usize,
                                    range.end.character as usize,
                                ));
                                logical_success = true;
                            }
                        } else if has_trailing_ws && trimmed_suffix.is_empty() {
                            let range =
                                ll.map_range(cursor_logical_clamped, cursor_logical_clamped);
                            value_range_phys = Some((
                                range.start.line as usize,
                                range.start.character as usize,
                                range.end.line as usize,
                                range.end.character as usize,
                            ));
                            logical_success = true;
                        }
                    }
                    if !logical_success {
                        // Physical fallback (single-line case or no logical coverage).
                        let prefix_line = &line_text[..char_byte.min(line_text.len())];
                        let tokens = crate::parser::tokenize_with_spans(prefix_line);
                        if let Some(tok) = tokens.last() {
                            if let Some((key_part, _)) = crate::parser::split_key_value(&tok.text) {
                                let pos = key_part.len();
                                let suffix_part = &tok.text[pos + 1..];
                                let (span_start, span_end) =
                                    completion::value_replacement_span(suffix_part);
                                let base = tok.start + pos + 1;
                                let (s, e) = if has_trailing_ws && trimmed_suffix.is_empty() {
                                    (char_byte, char_byte)
                                } else {
                                    (base + span_start, base + span_end)
                                };
                                let s_clamped = s.min(line_text.len()).min(e);
                                let e_clamped = e.min(line_text.len());
                                let s_floored =
                                    crate::encoding::floor_char_boundary(line_text, s_clamped);
                                let e_floored =
                                    crate::encoding::floor_char_boundary(line_text, e_clamped);
                                value_range_phys = Some((line_idx, s_floored, line_idx, e_floored));
                            }
                        } else if has_trailing_ws && trimmed_suffix.is_empty() {
                            value_range_phys = Some((line_idx, char_byte, line_idx, char_byte));
                        }
                    }
                }
            }
            if let Some((s_line, s_byte, e_line, e_byte)) = value_range_phys {
                // Convert byte offsets to wire characters per encoding, using
                // the physical line text for the corresponding line (so
                // multi-byte chars are counted correctly per line).
                // Reuses the single request-level `lines` split above.
                let s_line_text = lines.get(s_line).copied().unwrap_or("");
                let e_line_text = lines.get(e_line).copied().unwrap_or("");
                let start_char = match self.position_encoding {
                    PositionEncoding::Utf8 => s_byte as u32,
                    PositionEncoding::Utf16 => byte_offset_to_utf16_units(s_line_text, s_byte),
                };
                let end_char = match self.position_encoding {
                    PositionEncoding::Utf8 => e_byte as u32,
                    PositionEncoding::Utf16 => byte_offset_to_utf16_units(e_line_text, e_byte),
                };
                for item in &mut items {
                    if item.kind == Some(completion::kind::ENUM_MEMBER) {
                        let new_text = item
                            .insert_text
                            .clone()
                            .unwrap_or_else(|| item.label.clone());
                        item.text_edit = Some(completion::TextEdit {
                            range: completion::CompletionRange {
                                start: completion::CompletionPosition {
                                    line: s_line as u32,
                                    character: start_char,
                                },
                                end: completion::CompletionPosition {
                                    line: e_line as u32,
                                    character: end_char,
                                },
                            },
                            new_text,
                        });
                    }
                }
            } else {
                // Sub-menu / verb (kinds 9/3) and property / flag
                // (kinds 5/14) prefix case (logical-aware with physical
                // fallback). Both share `completion::partial_name_span` with
                // the completion layer, so the token one side typed is the
                // token the other replaces; the kind filter below only
                // selects which items receive the shared range. Ranges always
                // land on the cursor line via logical mapping or the physical
                // cursor line — never a line-0 guess.
                let mut submenu_range_phys: Option<(usize, usize, usize, usize)> = None;
                let mut typed_lower: Option<String> = None;
                // Try logical first when covering exists.
                if let (Some(ll), Some(cursor_logical)) = (covering, cursor_logical_opt) {
                    let logical_text = ll.text();
                    let cursor_logical_clamped = cursor_logical.min(logical_text.len());
                    let cursor_logical_clamped =
                        crate::encoding::floor_char_boundary(logical_text, cursor_logical_clamped);
                    let logical_prefix = &logical_text[..cursor_logical_clamped];
                    if let Some((typed, log_s, log_e)) =
                        completion::partial_name_span(logical_prefix)
                    {
                        let lower = typed.to_ascii_lowercase();
                        if has_prefix_match(&items, &lower) {
                            let range = ll.map_range(log_s, log_e);
                            submenu_range_phys = Some((
                                range.start.line as usize,
                                range.start.character as usize,
                                range.end.line as usize,
                                range.end.character as usize,
                            ));
                            typed_lower = Some(lower);
                        }
                    }
                }
                if submenu_range_phys.is_none() {
                    // Physical fallback.
                    let prefix_line = &line_text[..char_byte.min(line_text.len())];
                    if let Some((typed, s_byte, e_byte)) =
                        completion::partial_name_span(prefix_line)
                    {
                        let lower = typed.to_ascii_lowercase();
                        if has_prefix_match(&items, &lower) {
                            submenu_range_phys = Some((line_idx, s_byte, line_idx, e_byte));
                            typed_lower = Some(lower);
                        }
                    }
                }
                // Partial menu-path segment (`/ip/addr`): the
                // completion layer offers the parent's matching child
                // menu and replaces ONLY the typed final segment. Map
                // that segment span from the logical join (preferred,
                // continuation-aware) or the physical cursor line so
                // the segment-only edit never reaches the wire with a
                // line-0 guess.
                if submenu_range_phys.is_none() {
                    if let (Some(ll), Some(cursor_logical)) = (covering, cursor_logical_opt) {
                        let logical_text = ll.text();
                        let cursor_logical_clamped = cursor_logical.min(logical_text.len());
                        let cursor_logical_clamped = crate::encoding::floor_char_boundary(
                            logical_text,
                            cursor_logical_clamped,
                        );
                        let logical_prefix = &logical_text[..cursor_logical_clamped];
                        if let Some((typed, s, e)) =
                            completion::partial_path_segment(logical_prefix)
                        {
                            let range = ll.map_range(s, e);
                            submenu_range_phys = Some((
                                range.start.line as usize,
                                range.start.character as usize,
                                range.end.line as usize,
                                range.end.character as usize,
                            ));
                            typed_lower = Some(typed.to_ascii_lowercase());
                        }
                    }
                    if submenu_range_phys.is_none() {
                        let prefix_line = &line_text[..char_byte.min(line_text.len())];
                        if let Some((typed, s, e)) = completion::partial_path_segment(prefix_line) {
                            submenu_range_phys = Some((line_idx, s, line_idx, e));
                            typed_lower = Some(typed.to_ascii_lowercase());
                        }
                    }
                }
                if let (Some((s_line, s_byte, e_line, e_byte)), Some(lower)) =
                    (submenu_range_phys, typed_lower)
                {
                    // Reuses the single request-level `lines` split above.
                    let s_line_text = lines.get(s_line).copied().unwrap_or("");
                    let e_line_text = lines.get(e_line).copied().unwrap_or("");
                    let start_char = match self.position_encoding {
                        PositionEncoding::Utf8 => s_byte as u32,
                        PositionEncoding::Utf16 => byte_offset_to_utf16_units(s_line_text, s_byte),
                    };
                    let end_char = match self.position_encoding {
                        PositionEncoding::Utf8 => e_byte as u32,
                        PositionEncoding::Utf16 => byte_offset_to_utf16_units(e_line_text, e_byte),
                    };
                    for item in &mut items {
                        if is_edit_target_kind(item.kind)
                            && item.label.to_ascii_lowercase().starts_with(&lower)
                        {
                            let new_text = item
                                .insert_text
                                .clone()
                                .unwrap_or_else(|| item.label.clone());
                            item.text_edit = Some(completion::TextEdit {
                                range: completion::CompletionRange {
                                    start: completion::CompletionPosition {
                                        line: s_line as u32,
                                        character: start_char,
                                    },
                                    end: completion::CompletionPosition {
                                        line: e_line as u32,
                                        character: end_char,
                                    },
                                },
                                new_text,
                            });
                        }
                    }
                }
            }
        }

        // A response that hit the item cap is by definition not the
        // complete candidate set: tell the client so it re-queries on
        // further typing instead of treating the truncated list as
        // final. Equality with the cap is the only signal available
        // here (the completion layer does not surface a truncation
        // flag); an untruncated list of exactly `MAX_COMPLETION_ITEMS`
        // merely triggers one redundant re-query.
        let is_incomplete = items.len() >= MAX_COMPLETION_ITEMS;
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "isIncomplete": is_incomplete,
                "items": items,
            },
        })
    }
}

/// Completion kinds whose `textEdit` the server rewrites positionally.
///
/// Single place for the shadow-carrier set: a new shadow-carrying kind must
/// be added here or its line-0 shadow could reach the wire.
fn is_edit_target_kind(kind: Option<i32>) -> bool {
    matches!(
        kind,
        Some(completion::kind::CLASS)
            | Some(completion::kind::FUNCTION)
            | Some(completion::kind::PROPERTY)
            | Some(completion::kind::CONSTANT)
    )
}

/// True when at least one rewrite-target item starts with `lower`.
///
/// Computing the replacing range is only useful when some item can actually
/// match the typed prefix; this mirrors the client-side prefix match the
/// completion layer ranked against.
fn has_prefix_match(items: &[completion::CompletionItem], lower: &str) -> bool {
    items
        .iter()
        .any(|it| is_edit_target_kind(it.kind) && it.label.to_ascii_lowercase().starts_with(lower))
}
