# Sanitized /export-style sample for /system/scheduler (no secrets).
# Source: hand-built from llms-full.txt property names (no device available).
# The on-event payload only logs a line; it carries no credentials.
/system/scheduler
add name=nightly-log on-event=":log info \"nightly tick\"" start-date=2026-01-01 start-time=02:00:00 interval=1d comment="nightly" disabled=no
