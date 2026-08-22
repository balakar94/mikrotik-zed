; ── Outline / symbol view for RSC ─────────────────────────────
; @name = label for the symbol
; @context = prefix shown before @name in the outline (e.g., "ip > address")
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

; Global commands (e.g., :put, :local, :if)
(global_command
  (identifier) @name) @item
