; ── Indentation rules for RSC ─────────────────────────────────
; Zed expects @indent.begin / @indent.end (see tree-sitter-grammar.md)
(block "{" @indent.begin)
(block "}" @indent.end)
(command_substitution "[" @indent.begin)
(command_substitution "]" @indent.end)
(subexpression "(" @indent.begin)
(subexpression ")" @indent.end)
; Line continuation keeps indent on the following line
(line_continuation) @indent.continue
