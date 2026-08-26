; ── MikroTik RouterOS Script — highlights ────────────────────────
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
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping)$"))

((menu_command
  (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping)$"))

; Action commands inside [...] → purple
((command_substitution
  (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping)$"))

; Action commands in continuation → purple
((menu_continuation
  (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping)$"))

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

; ── :return true / :return false ────────────────────────────────
((global_command
   (global_command_name (identifier) @_cmd)
   (identifier) @diff.plus)
 (#eq? @_cmd "return")
 (#eq? @diff.plus "true"))

((global_command
   (global_command_name (identifier) @_cmd)
   (identifier) @diff.minus)
 (#eq? @_cmd "return")
 (#eq? @diff.minus "false"))

; ── Control flow keywords ───────────────────────────────────────
"do" @keyword
"else" @keyword
"while" @keyword

(do_block "=" @operator)
(else_block "=" @operator)

; ── Booleans ────────────────────────────────────────────────────
(boolean_literal) @boolean

; ── Nil ─────────────────────────────────────────────────────────
(nil_literal) @constant.builtin

; ── Function calls ──────────────────────────────────────────────
(function_call
  (variable_reference
    (identifier) @function))

; ── Variables ───────────────────────────────────────────────────
(variable_reference
  "$" @punctuation.special
  (identifier) @variable.parameter)

(variable_reference) @variable

; ── Strings ────────────────────────────────────────────────────
(string) @string

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

; yes → green (on), no → red (off).
; NOTE: in value position the lexer resolves yes/no as identifiers (lexical
; conflict with `identifier`), so these match named_param identifier values,
; not boolean_literal nodes. true/false stay plain values.
((named_param
   value: (identifier) @diff.plus)
 (#eq? @diff.plus "yes"))

((named_param
   value: (identifier) @diff.minus)
 (#eq? @diff.minus "no"))
