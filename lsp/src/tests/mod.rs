// White-box unit tests (same crate, may use `pub(crate)`).
// One file per area/aspect, max ~300 lines soft / 400 hard.
// Black-box E2E stays in `lsp/tests/` and must not import from here.
//
// Blanket allow is intentional for now: the suite normalization copied
// small helpers per file and many are unused in their host file (128
// dead-code/unused-import hits without it). Scoping allows per file is
// future cleanup — do not remove this line without fixing all of them.
#![allow(dead_code, unused_imports)]

#[cfg(test)]
mod caps;

#[cfg(test)]
mod cli;

#[cfg(test)]
mod completion_colon;

#[cfg(test)]
mod completion_context;

#[cfg(test)]
mod completion_diagnostics;

#[cfg(test)]
mod completion_docs;

#[cfg(test)]
mod completion_helpers;

#[cfg(test)]
mod completion_live;

#[cfg(test)]
mod completion_menu;

#[cfg(test)]
mod completion_multiline;

#[cfg(test)]
mod completion_property;

#[cfg(test)]
mod completion_ranking;

#[cfg(test)]
mod completion_ranking_meta;

#[cfg(test)]
mod completion_snippets;

#[cfg(test)]
mod completion_value;

#[cfg(test)]
mod diagnostics_basic;

#[cfg(test)]
mod diagnostics_caps;

#[cfg(test)]
mod diagnostics_commands;

#[cfg(test)]
mod diagnostics_continuation;

#[cfg(test)]
mod diagnostics_enums;

#[cfg(test)]
mod diagnostics_logical_lines;

#[cfg(test)]
mod diagnostics_precision;

#[cfg(test)]
mod diagnostics_ranges;

#[cfg(test)]
mod diagnostics_readonly;

#[cfg(test)]
mod diagnostics_required;

#[cfg(test)]
mod diagnostics_severity;

#[cfg(test)]
mod diagnostics_severity_extra;

#[cfg(test)]
mod diagnostics_syntax;

#[cfg(test)]
mod diagnostics_syntax_extra;

#[cfg(test)]
mod diagnostics_truncation;

#[cfg(test)]
mod diagnostics_unset;

#[cfg(test)]
mod diagnostics_unset_forms;

#[cfg(test)]
mod diagnostics_validators_bool;

#[cfg(test)]
mod diagnostics_validators_ip;

#[cfg(test)]
mod diagnostics_validators_scalar;

#[cfg(test)]
mod encoding_edits;

#[cfg(test)]
mod encoding_offsets;

#[cfg(test)]
mod encoding_positions;

#[cfg(test)]
mod encoding_symbols;

#[cfg(test)]
mod encoding_utf16;

#[cfg(test)]
mod folding;

#[cfg(test)]
mod framing_codec;

#[cfg(test)]
mod framing_codec_extra;

#[cfg(test)]
mod hover_builtin;

#[cfg(test)]
mod hover_edge;

#[cfg(test)]
mod hover_fixtures;

#[cfg(test)]
mod hover_menu;

#[cfg(test)]
mod hover_property;

#[cfg(test)]
mod hover_verbs;

#[cfg(test)]
mod live_audit;

#[cfg(test)]
mod live_cache;

#[cfg(test)]
mod live_config;

#[cfg(test)]
mod live_fetch;

#[cfg(test)]
mod live_hosts;

#[cfg(test)]
mod live_resources;

#[cfg(test)]
mod live_ssrf;

#[cfg(test)]
mod live_tls;

#[cfg(test)]
mod live_transport;

#[cfg(test)]
mod live_validation;

#[cfg(test)]
mod logging;

#[cfg(test)]
mod menus_args;

#[cfg(test)]
mod menus_lookup;

#[cfg(test)]
mod navigation_definition;

#[cfg(test)]
mod navigation_index;

#[cfg(test)]
mod navigation_references;

#[cfg(test)]
mod navigation_resolution;

#[cfg(test)]
mod parser_line;

#[cfg(test)]
mod parser_parse;

#[cfg(test)]
mod parser_quotes;

#[cfg(test)]
mod parser_structure;

#[cfg(test)]
mod parser_token;

#[cfg(test)]
mod rename;

#[cfg(test)]
mod server_basic;

#[cfg(test)]
mod server_cache;

#[cfg(test)]
mod server_code_actions_basic;

#[cfg(test)]
mod server_code_actions_kinds;

#[cfg(test)]
mod server_code_actions_recovery;

#[cfg(test)]
mod server_config;

#[cfg(test)]
mod server_diagnostics;

#[cfg(test)]
mod server_docs;

#[cfg(test)]
mod server_errors;

#[cfg(test)]
mod server_limits;

#[cfg(test)]
mod server_live;

#[cfg(test)]
mod server_sync;

#[cfg(test)]
mod server_sync_extra;

#[cfg(test)]
mod server_uri;

#[cfg(test)]
mod signature_filtering;

#[cfg(test)]
mod signature_help;

#[cfg(test)]
mod signature_help_cursor;

#[cfg(test)]
mod signature_help_docs;

#[cfg(test)]
mod signature_protocol;

#[cfg(test)]
mod signature_sanitizer;

#[cfg(test)]
mod suggest;

#[cfg(test)]
mod symbols;

#[cfg(test)]
mod text_util;

#[cfg(test)]
mod text_util_sanitize;
