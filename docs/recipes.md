# Recipes

Short, safe RouterOS tasks for a `.rsc` file edited in Zed. Every recipe
assumes the file is deployed with the [device deploy](device-deploy.md)
workflow — dry-run first, then push.

## Safe-change checklist

1. **Read before writing.** Print the menu you are about to change:
   `/ip address print`, `/ip firewall filter print`, `/ip dhcp-client print`.
2. **Back up.** `/export file=pre-<timestamp>` (or push with
   `--backup`, which writes the backup first and aborts if it fails).
3. **Check locally.** Run *MikroTik: Check script (dry-run, no device)*;
   fix diagnostics in the editor.
4. **Dry-run the deploy** and read the preview.
5. **Push** with `--method rest` (or `--method ssh` for long payloads).
6. **Verify on the device** — a 2xx response alone does not prove the import
   applied; the deploy tool scans for failure markers and the REST sentinel.
7. **Keep the backup** until the change is confirmed; roll back by importing
   it with `/import file=<backup>`.

## Back up the current configuration

On-device export (SSH or the deploy task; `--backup` does this
automatically before a push):

```rsc
# Named so the newest backup sorts last.
/export file=pre-20260101-120000
```

The file lands in the device's file list. Pull it with your usual file
transfer if you need an off-device copy.

## Assign a static IP address

```rsc
# Replace with your LAN values; `address` is required for `add`.
/ip address add address=192.168.88.2/24 interface=bridge1 comment="static LAN"
```

If the interface already has an address from DHCP, disable the client first
(see below) or the device keeps both.

## Enable a DHCP client

```rsc
# `interface` is required. Enable DNS/route/NTP from the lease explicitly.
/ip dhcp-client add interface=ether1 disabled=no \
    add-default-route=yes use-peer-dns=yes use-peer-ntp=yes
```

Then check the lease:

```rsc
/ip dhcp-client print detail
```

## Add a firewall filter rule

Rules are order-sensitive: `/ip firewall filter add` appends to the chain.
Read the chain before and after:

```rsc
# Allow established/related first so replies are not dropped.
/ip firewall filter add chain=input action=accept \
    connection-state=established,related comment="established/related"

# Allow ICMP so the device can be pinged.
/ip firewall filter add chain=input action=accept protocol=icmp \
    comment="allow ICMP"

# Drop anything else reaching the input chain.
/ip firewall filter add chain=input action=drop comment="drop rest"
```

Change management:

```rsc
# Move a rule by position (0-based), or disable it instead of deleting.
/ip firewall filter move 3 destination=1
/ip firewall filter disable 4
/ip firewall filter print
```

`remove` is gated by the deploy tool's destructive pre-scan and requires
`--force-destructive` (after a backup) — prefer `disable` while testing.

## Set the device identity and comment

```rsc
/system identity set name=router-edge comment="managed from Zed"
```

## Related

- [Language features](language-features.md) — diagnostics that fire on these
  commands (missing required properties, unknown menu paths).
- [Device deploy](device-deploy.md) — transports, verification, exit codes.
- [Live enrichment](live-enrichment.md) — complete interface/list names from
  the device instead of typing them.
