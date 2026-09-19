; ── MikroTik RouterOS Script — highlights ────────────────────────
; CURATED — hand-maintained action-verb list
; No generator exists; edit the (#match? ...) alternations below in place (data/commands.toml holds per-menu argument enums, not this list).
; Color scheme:
;   Blue   = root menu (first command after /)
;   Green  = sub-menus, quoted strings, `yes`
;   Yellow = property names (name=value)
;   Orange = bare values, numbers, IPs, true/false
;   Purple = action verbs, :globals, "/"
;   Red    = comment values, `no`
;   Grey   = comments

; ── Comments ─────────────────────────────────────────────────────
(comment) @comment

; ── Menu prefix "/" ──────────────────────────────────────────────
(menu_prefix) @punctuation.special

; ── Root menu — first command after / (blue) ────────────────────
; e.g. "ip" in /ip route add …
(root_menu
  (identifier) @function)

; ── Sub-menus — subsequent segments (green) ────────────────────
; e.g. "route", "add" in /ip route add …
(sub_menu
  (identifier) @string)

; ── Identifiers inside command_substitution / menu_continuation ──
; Commands like "find", "set" inside [...] → green
(command_substitution
  (identifier) @string)

(menu_continuation
  (identifier) @string)

; ── Catch-all: bare identifiers in menu_command → orange ───────
; These are typically values after line continuation like `password=\nvalue`
; Placed AFTER specific string captures but BEFORE keyword overrides so
; action verbs can still be promoted to @keyword.  Specific captures
; (root_menu/sub_menu) are structurally distinct (wrapped nodes) and are
; not affected by this catch-all, but ordering keeps precedence explicit.
(menu_command
  (identifier) @constant)

; ── Action commands (purple) ──────────────────────────────────
; Override sub_menu / bare-identifier green/orange for commands that
; modify/query state.  Must come LAST so they win over @string/@constant.
((sub_menu
   (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping|monitor|watch|fetch|resolve|check|cancel|flush|run|info|warning|error|debug|unset|scan|reboot|shutdown|backup|save|restore|update|install|renew|release|torch|sniffer|connect|disconnect)$"))

((menu_command
   (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping|monitor|watch|fetch|resolve|check|cancel|flush|run|info|warning|error|debug|unset|scan|reboot|shutdown|backup|save|restore|update|install|renew|release|torch|sniffer|connect|disconnect)$"))

; Action commands inside [...] → purple
((command_substitution
   (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping|monitor|watch|fetch|resolve|check|cancel|flush|run|info|warning|error|debug|unset|scan|reboot|shutdown|backup|save|restore|update|install|renew|release|torch|sniffer|connect|disconnect)$"))

; Action commands in continuation → purple
((menu_continuation
   (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping|monitor|watch|fetch|resolve|check|cancel|flush|run|info|warning|error|debug|unset|scan|reboot|shutdown|backup|save|restore|update|install|renew|release|torch|sniffer|connect|disconnect)$"))

; ── Named parameters — property=value ──────────────────────────
; Property name → yellow (like the MikroTik terminal)
(named_param
  name: (identifier) @type)

; Property value (identifiers like ether1, bridge) → orange
(named_param
  value: (identifier) @constant)

; ── = sign in named params ──────────────────────────────────────
(named_param "=" @operator)

; ── Global commands (:put, :local, :for, etc.) ──────────────────
(global_command_name) @keyword

; ── Variable declarations (:local/:global/:set name …) ──────────
((global_command
   (global_command_name (identifier) @_cmd)
   (identifier) @variable.parameter)
  (#match? @_cmd "^(local|global|set)$"))

; ── Loop variables (:foreach i, :for i, :onerror e) ─────────────
((global_command
   (global_command_name (identifier) @_cmd)
   (identifier) @variable.parameter)
  (#match? @_cmd "^(foreach|for|onerror)$"))

; ── :return true / :return false ────────────────────────────────
; `true`/`false` parse as literal(boolean_literal), not identifiers.
; The override patterns live in the final overrides section so they win
; over the generic (boolean_literal) @boolean capture below.
; ─────────────────────────────────────────────────────────────────

; ── Control flow keywords ───────────────────────────────────────
"do" @keyword
"else" @keyword
"while" @keyword

; `where` in query clauses (`print where …`, `[find where …]`) parses as
; a sub_menu identifier; promote it to a keyword.
((sub_menu
   (identifier) @keyword)
  (#eq? @keyword "where"))

(do_block "=" @operator)
(else_block "=" @operator)

; `in=` (foreach / onerror) and `while=` share the named_param `=` shape
; but are parsed as dedicated clauses, so they need their own captures.
(for_in_clause
  "in" @keyword
  "=" @operator)

(while_condition
  "=" @operator)

; ── Booleans ────────────────────────────────────────────────────
(boolean_literal) @boolean

; ── Nil ─────────────────────────────────────────────────────────
(nil_literal) @constant.builtin

; ── Variables ───────────────────────────────────────────────────
; Whole-node fallback FIRST: later patterns win, so the `$`/identifier
; parts below (and the function-call pattern after them) override it for
; their own ranges.
(variable_reference) @variable

(variable_reference
  "$" @punctuation.special
  (identifier) @variable.parameter)

; ── Function calls ──────────────────────────────────────────────
; Must come AFTER the variable captures so `$func` in `$func arg`
; renders as @function instead of the whole-node @variable.
(function_call
  (variable_reference
    (identifier) @function))

; ── Strings ────────────────────────────────────────────────────
(string) @string

; ── URLs ───────────────────────────────────────────────────────
(url) @string.special

; ── Mixed scalars (time, classifier, client-id, account) ───────
(mixed_value) @number

; ── Numbers ─────────────────────────────────────────────────────
(number) @number

; ── IP addresses / prefixes ─────────────────────────────────────
(ip_address) @number
(ip_prefix) @number
(mac_address) @number
(duration) @number

; ── Arrays ──────────────────────────────────────────────────────
(array
  "{" @punctuation.bracket
  "}" @punctuation.bracket)

; ── Operators ───────────────────────────────────────────────────
(operator) @operator

; ── Array access arrow ──────────────────────────────────────────
; `->` is an anonymous token in array_access, not the operator rule.
(array_access "->" @operator)

; ── Brackets ────────────────────────────────────────────────────
[
  "(" ")" "[" "]" "{" "}"
] @punctuation.bracket

; ── Statement separator ───────────────────────────────────────
";" @punctuation.delimiter

; ── Line continuation ──────────────────────────────────────────
(line_continuation) @punctuation.special

; ── Parent navigation ───────────────────────────────────────────
(parent_navigation) @string.special

; ── Command substitution ────────────────────────────────────────
(command_substitution
  "[" @punctuation.bracket
  "]" @punctuation.bracket)

; ── Subexpressions ──────────────────────────────────────────────
(subexpression
  "(" @punctuation.bracket
  ")" @punctuation.bracket)

; ── Block delimiters ────────────────────────────────────────────
(block
  "{" @punctuation.bracket
  "}" @punctuation.bracket)

; ── Overrides (MUST stay last — later patterns win on same node) ──

; comment=... always red, quoted or bare (field work request #5)
((named_param
   name: (identifier) @_comment_prop
   value: (literal
           (string) @diff.minus))
 (#eq? @_comment_prop "comment"))

((named_param
   name: (identifier) @_comment_prop
   value: (identifier) @diff.minus)
 (#eq? @_comment_prop "comment"))

; yes → green (on), no → red (off). `yes`/`no` parse as
; literal(boolean_literal) in value position (verified with
; `tree-sitter parse`), so the identifier-based form never matched.
; true/false stay plain @boolean values.
((named_param
   value: (literal (boolean_literal) @diff.plus))
 (#eq? @diff.plus "yes"))

((named_param
   value: (literal (boolean_literal) @diff.minus))
 (#eq? @diff.minus "no"))

; comment=yes / comment=no: red regardless of on/off wording; kept after
; the generic yes/no patterns so comment values always win.
((named_param
   name: (identifier) @_comment_prop
   value: (literal (boolean_literal) @diff.minus))
 (#eq? @_comment_prop "comment"))

; :return true / :return false — same literal(boolean_literal) shape;
; placed last so they win over the generic @boolean capture above.
((global_command
   (global_command_name (identifier) @_cmd)
   (literal (boolean_literal) @diff.plus))
 (#eq? @_cmd "return")
 (#eq? @diff.plus "true"))

((global_command
   (global_command_name (identifier) @_cmd)
   (literal (boolean_literal) @diff.minus))
 (#eq? @_cmd "return")
 (#eq? @diff.minus "false"))
