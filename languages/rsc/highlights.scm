; ── MikroTik RouterOS Script — highlights ────────────────────────
; CURATED — hand-maintained action-verb list
; No generator exists; edit the (#match? ...) alternations below in place (data/commands.toml holds per-menu argument enums, not this list).
;
; Captures express semantic intent only: actual colors come from the user's
; theme, and this file makes no literal-color promises. Zed resolves multiple
; captures on one node right-to-left (rightmost first, then leftwards
; fallback), so theme-dependent values carry `@preferred @fallback` chains:
;   - menus/verbs ....... structural anchors (root menu, sub-menu segments,
;                         action verbs, `where`, control-flow words);
;   - strings/comments .. quoted strings, sub-menu text, comments;
;   - properties ........ `@type @property` on `name=` keys: @property is the
;                         semantically exact capture, @type the fallback for
;                         themes that do not style @property;
;   - literals .......... numbers, IPs, MACs, durations, mixed scalars;
;   - booleans .......... `@boolean @diff.plus` (on/true) and
;                         `@boolean @diff.minus` (off/false): the diff text
;                         colors are preferred where the theme defines them;
;   - comment values .... `@string @diff.minus`: red where the theme defines
;                         @diff.minus, normal string color otherwise;
;   - nil ............... `@constant @constant.builtin`: constant.builtin is
;                         preferred, plain @constant covers themes (e.g. the
;                         One themes) that do not define constant.builtin;
;   - variables ......... `@variable @variable.parameter` on references;
;                         declarations and loop variables stay parameter-only.

; ── Comments ─────────────────────────────────────────────────────
(comment) @comment

; ── Menu prefix "/" ──────────────────────────────────────────────
(menu_prefix) @punctuation.special

; ── Root menu — first command after / ────────────────────────────
; e.g. "ip" in /ip route add …
(root_menu
  (identifier) @function)

; ── Sub-menus — subsequent segments ────────────────────────────
; e.g. "route", "add" in /ip route add …
(sub_menu
  (identifier) @string)

; ── Identifiers inside command_substitution / menu_continuation ──
; Commands like "find", "set" inside [...] → string-colored text
(command_substitution
  (identifier) @string)

; Direct identifiers in a menu_continuation are property values/leaders
; (e.g. the `.sn.mynetname.net` tail of a dotted value, or list items),
; never sub-menu segments — those only appear after a `/`. Command-style
; leaders (`add`, `set`, …) are promoted by the menu_continuation verb
; override below; no first-child special case is needed because that
; override supplies it.
(menu_continuation
  (identifier) @constant)

; ── Catch-all: bare identifiers in menu_command → plain value ──
; These are typically values after line continuation like `password=\nvalue`
; Placed AFTER specific string captures but BEFORE keyword overrides so
; action verbs can still be promoted to @keyword.  Specific captures
; (root_menu/sub_menu) are structurally distinct (wrapped nodes) and are
; not affected by this catch-all, but ordering keeps precedence explicit.
(menu_command
  (identifier) @constant)

; ── Action commands ────────────────────────────────────────────
; Override sub_menu / bare-identifier string/plain-value captures for
; commands that modify/query state.  Must come LAST so they win over
; @string/@constant.
((sub_menu
   (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping|monitor|watch|fetch|resolve|check|cancel|flush|run|info|warning|error|debug|unset|scan|reboot|shutdown|backup|save|restore|update|install|renew|release|torch|sniffer|connect|disconnect)$"))

; Only a verb directly following a `\` continuation is a command here
; (e.g. `/ip firewall filter \` + `add chain=input`). Other bare
; identifiers in menu_command are values — including comma-separated list
; members such as `policy=ftp,reboot,…` — and stay @constant.
; Residual: a continued list whose FIRST member is a verb-list word
; (`policy=\` + `password,read`) is still promoted; the grammar cannot
; distinguish it from a continued verb without property context.
((menu_command
   (line_continuation) . (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping|monitor|watch|fetch|resolve|check|cancel|flush|run|info|warning|error|debug|unset|scan|reboot|shutdown|backup|save|restore|update|install|renew|release|torch|sniffer|connect|disconnect)$"))

; Action commands inside [...] → keyword
((command_substitution
   (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping|monitor|watch|fetch|resolve|check|cancel|flush|run|info|warning|error|debug|unset|scan|reboot|shutdown|backup|save|restore|update|install|renew|release|torch|sniffer|connect|disconnect)$"))

; Action commands in continuation → keyword
((menu_continuation
   (identifier) @keyword)
  (#match? @keyword "^(add|remove|set|get|print|enable|disable|find|comment|move|export|import|edit|reset|force-update|beep|blink|password|quit|redo|undo|ping|monitor|watch|fetch|resolve|check|cancel|flush|run|info|warning|error|debug|unset|scan|reboot|shutdown|backup|save|restore|update|install|renew|release|torch|sniffer|connect|disconnect)$"))

; ── Named parameters — property=value ──────────────────────────
; Property key: @property is the semantically exact capture (rightmost =
; preferred); @type is the fallback for themes that lack @property.
(named_param
  name: (identifier) @type @property)

; Property value (identifiers like ether1, bridge) → plain value
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
; constant.builtin preferred; plain @constant covers themes (One Dark/Light)
; that define no style for constant.builtin.
(nil_literal) @constant @constant.builtin

; ── Variables ───────────────────────────────────────────────────
; Whole-node fallback FIRST: later patterns win, so the `$`/identifier
; parts below (and the function-call pattern after them) override it for
; their own ranges.
(variable_reference) @variable

(variable_reference
  "$" @punctuation.special
  (identifier) @variable @variable.parameter)

; ── Function calls ──────────────────────────────────────────────
; Must come AFTER the variable captures so `$func` in `$func arg`
; renders as @function instead of the whole-node @variable.
(function_call
  (variable_reference
    (identifier) @function))

; ── Strings ────────────────────────────────────────────────────
(string) @string
; NOTE: `#` inside a quoted value is string content by design — e.g. a
; script `source="… # comment …"` is one (string) node, so the `#` is never
; highlighted as a comment. That is intended; no capture changes here.

; ── URLs ───────────────────────────────────────────────────────
(url) @string.special

; Quoted URLs: the grammar tokenizes a quoted value (including one split
; across a `\` continuation) as a single (string) node, so the (url) node
; never appears. Detect the scheme prefix so quoted and unquoted URLs share
; the same capture. Plain strings keep @string (earlier pattern); this later
; @string.special only wins on the scheme match.
((string) @_url_str @string.special
  (#match? @_url_str "^['\"]?[A-Za-z][A-Za-z0-9+.-]*://"))

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
; Every capture pair below is a fallback chain: the rightmost capture wins
; when the theme defines it, otherwise Zed falls back leftwards.

; comment=... red where @diff.minus is themed, normal string color elsewhere.
((named_param
   name: (identifier) @_comment_prop
   value: (literal
           (string) @string @diff.minus))
 (#eq? @_comment_prop "comment"))

((named_param
   name: (identifier) @_comment_prop
   value: (identifier) @string @diff.minus)
 (#eq? @_comment_prop "comment"))

; yes → on-color, no → off-color. `yes`/`no` parse as
; literal(boolean_literal) in value position (verified with
; `tree-sitter parse`), so the identifier-based form never matched.
; @boolean is the portable fallback; true/false stay plain @boolean values
; (the generic (boolean_literal) @boolean capture above) unless the diff
; colors are available.
((named_param
   value: (literal (boolean_literal) @boolean @diff.plus))
 (#eq? @diff.plus "yes"))

((named_param
   value: (literal (boolean_literal) @boolean @diff.minus))
 (#eq? @diff.minus "no"))

; comment=yes / comment=no: treated like every other comment value; kept
; after the generic yes/no patterns so comment values always win.
((named_param
   name: (identifier) @_comment_prop
   value: (literal (boolean_literal) @string @diff.minus))
 (#eq? @_comment_prop "comment"))

; :return true / :return false — same literal(boolean_literal) shape;
; placed last so they win over the generic @boolean capture above.
((global_command
   (global_command_name (identifier) @_cmd)
   (literal (boolean_literal) @boolean @diff.plus))
 (#eq? @_cmd "return")
 (#eq? @diff.plus "true"))

((global_command
   (global_command_name (identifier) @_cmd)
   (literal (boolean_literal) @boolean @diff.minus))
 (#eq? @_cmd "return")
 (#eq? @diff.minus "false"))
