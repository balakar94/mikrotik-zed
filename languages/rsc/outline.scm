; ── Outline / symbol view for RSC ─────────────────────────────
; FALLBACK provider: Zed consults this Tree-sitter outline only when the
; `rsc-ls` language server is unavailable. When the server runs, its
; textDocument/documentSymbol response (lsp/src/symbols.rs) TAKES
; PRECEDENCE and this file is ignored. Keep both providers aligned: one
; row per menu command plus one row per script keyword/variable — never
; one row per property line (menu_continuation fragments are
; intentionally NOT listed; they would flood the outline with one entry
; per `key=value` pair while the server collapses runs and reports
; details instead).
;
; Kind alignment with symbols.rs (LSP DocumentSymbolKind values):
; - menu_command rows ......... Object (19); @context is the root menu so
;   `/ip address add …` shows as "ip > address".
; - :local/:global/:set rows .. Variable (13); @name is the declared
;   IDENTIFIER (not the command word), matching the server landmark that
;   rename targets.
; - all other :verb rows ....... Function (12); @name is the command name
;   (e.g. "put" for `:put`).
;
; @name = label for the symbol.
; @context = prefix shown before @name in the outline (e.g., "ip > address").
;
; For menu commands, show the first sub-menu as the name under the root menu
; context.  E.g. `/ip address add ...` → "ip > address".
; For commands without sub-menus, show the root menu as the name.

(menu_command
  (menu_prefix)
  (root_menu (identifier) @context)
  (sub_menu (identifier) @name)
) @item

(menu_command
  (menu_prefix)
  (root_menu (identifier) @name)
) @item

; Variable declarations (:local/:global/:set name …) → Variable (13).
; The outline label is the declared identifier, mirroring symbols.rs.
((global_command
   (global_command_name (identifier) @_cmd)
   (identifier) @name) @item
 (#match? @_cmd "^(local|global|set)$"))

; All other script commands (:put, :if, :foreach, …) → Function (12).
; Guarded so declaration lines are listed exactly once (by the pattern
; above), never duplicated here.
((global_command
   (global_command_name (identifier) @name)) @item
 (#not-match? @name "^(local|global|set)$"))
