# hotspot-tui

Phone-style **personal hotspot** TUI for Linux laptops (built for a ThinkPad
X1 Carbon: iwlwifi AX211 + Quectel EM05-CE WWAN, NetworkManager, Hyprland).

Press `s`: disconnect WiFi, bring up a 2.4GHz WPA2 AP (`hostapd` on a
dedicated `__ap` interface), serve DHCP (`dnsmasq`), NAT through the
cellular default route. Press `x`: tear it all down and restore WiFi.

> ⚠️ Prototype. Requires passwordless `sudo` for `ip/iptables/iw/hostapd/dnsmasq`
> and is tuned to *this* machine (interface discovery is best-effort; SSID
> `X1C`, subnet `10.66.66.0/24` are compile-time constants for now).

## Why not `nmcli device wifi hotspot`?

Three walls hit on this exact kernel/driver stack (all debugged on-device):

1. **iwlwifi single-channel constraint** — managed + AP must share one
   channel; activating an AP kicks every associated WiFi network.
2. **wpa_supplicant 2.12 AP startup regression** with this kernel:
   `nl80211: kernel reports: multicast RX registrations are not supported`
   → NetworkManager's whole AP path fails. `hostapd` directly works.
3. **5GHz regulatory `NO_IR`** — every 5GHz channel refuses Initial
   Radiation (AP transmit) on this machine; `iw reg set CN` does not stick
   (iwlwifi firmware reg domain). v1 is therefore 2.4GHz ch6.

## Usage

```
hotspot             # TUI (s start · x stop · q quit)
hotspot --on        # scriptable equivalents
hotspot --off
hotspot --status
```

## Stack

Rust · ratatui · crossterm · nmcli/iw/hostapd/dnsmasq/iptables via shell.
No daemon of its own; state (pids/conf) under `$XDG_STATE_HOME/hotspot/`.

## Known issues

- 2.4GHz only (see regulatory note above); channel fixed to 6.
- Hotspot clients do **not** traverse host VPN/proxy (e.g. Clash TUN
  intercepts host-local traffic only) — they NAT straight out of the WAN.
- SSID/password/subnet are constants; no config file yet.
- `off` re-connects two hard-coded WiFi profiles (`lap2.4`, `lap`).
- Untested with multiple WWAN bearers / carrier tethering blocks.

## License

MIT
