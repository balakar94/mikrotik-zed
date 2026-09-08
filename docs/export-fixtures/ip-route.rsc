# Sanitized /export-style sample for /ip/route (TEST-NET-1/2, no secrets).
# Source: hand-built from llms-full.txt property names (no device available).
/ip/route
add dst-address=0.0.0.0/0 gateway=192.0.2.254 distance=1 comment="default"
add dst-address=198.51.100.0/24 gateway=192.0.2.1 check-gateway=ping disabled=no
