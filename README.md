# hotspot-tui

A **role switcher** for one WiFi radio — Linux take on the phone
"personal hotspot".

Built for a ThinkPad X1 Carbon: iwlwifi AX211 + Quectel EM05-CE WWAN,
NetworkManager, UFW, Hyprland.

## Mental model

One radio wears exactly **one hat at a time** (firmware: with an AP
present, every virtual interface must share a single channel — so
"keep the uplink WiFi and relay it" is a half-bandwidth pseudo-feature
and pointless when you already have WiFi; the useful move is to
*switch roles*):

```
   CLIENT hat                    BASE STATION hat
 laptop ─⌁─ lap2.4 ⇝ internet    phone ─⌁─ X1C(ap0) ─NAT─ SIM ⇝ internet
        press x  ◄──────────────►  press s
```

Press `s`: release the phy from NetworkManager, reset the wifi driver
(clears leaked firmware state), bring up a 2.4GHz WPA2 AP (`hostapd`),
serve DHCP (`dnsmasq`), open UFW route/input rules, NAT through the
cellular route. Press `x`: silence the radio, restore WiFi — the
client hat goes back on.

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
