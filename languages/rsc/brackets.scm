; ── Bracket matching for RSC ──────────────────────────────────
; Note: `"` is handled via config.toml brackets with `not_in = ["string"]`
; rather than a query here — avoids matching inside string nodes.
; Keep in sync with languages/rsc/config.toml brackets table.

("(" @open ")" @close)
("[" @open "]" @close)
("{" @open "}" @close)
