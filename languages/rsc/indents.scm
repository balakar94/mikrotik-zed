; ── Indentation rules for RSC ─────────────────────────────────
; Modern Zed contract (language_core::grammar): @indent is REQUIRED and marks
; a node whose span should be indented after its first line — single-line
; matches are ignored automatically, so one capture per bracketing node covers
; both open and close. Optional markers: @start / @end / @outdent (plus
; @start.<suffix>, tied to config.toml regex rules).
; The legacy @indent.begin/@indent.end/@indent.continue names no longer exist.
(block) @indent
(command_substitution) @indent
(subexpression) @indent
