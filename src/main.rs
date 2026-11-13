// hotspot-tui — phone-style personal hotspot for the X1 Carbon (iwlwifi).
//
// The NM/wpa_supplicant AP path is broken on this kernel (multicast RX
// registration + 5GHz NO_IR regulatory), so we run the classic stack
// directly: iw ap-vif + hostapd (2.4G) + dnsmasq (DHCP) + NAT to the SIM.
// Requires passwordless sudo for net admin; disconnects WiFi while ON
// (iwlwifi single-channel constraint), shares the WWAN default route.
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const SSID: &str = "X1C";
const PASS: &str = "x1carbon123";
const CHANNEL: &str = "6"; // 2.4GHz, regulatory-safe on this machine
const SUBNET: &str = "10.66.66";

fn run(cmd: &str) -> (bool, String) {
    match Command::new("sh").arg("-c").arg(cmd).output() {
        Ok(o) => (
            o.status.success(),
            String::from_utf8_lossy(&o.stdout).to_string()
                + &String::from_utf8_lossy(&o.stderr),
        ),
        Err(e) => (false, e.to_string()),
    }
}

fn state_dir() -> PathBuf {
    let p = dirs_or_tmp();
    fs::create_dir_all(&p).ok();
    p
}
fn dirs_or_tmp() -> PathBuf {
    std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.local/state", std::env::var("HOME").unwrap_or("/tmp".into()))))
        .join("hotspot")
}

fn wifi_dev() -> String {
    run("nmcli -t -f DEVICE,TYPE device | awk -F: '$2==\"wifi\"{print $1; exit}'")
        .1
        .trim()
        .to_string()
}

fn ap_exists() -> bool {
    run("iw dev ap0 info >/dev/null 2>&1").0
}
fn hostapd_running() -> bool {
    let pidf = state_dir().join("hostapd.pid");
    fs::read_to_string(&pidf)
        .ok()
        .and_then(|p| fs::read_dir(format!("/proc/{}", p.trim())).ok().map(|_| p.trim().to_string()))
        .is_some()
}

struct Status {
    on: bool,
    clients: usize,
    wwan: bool,
    wifi: bool,
    default_via: String,
}

fn poll() -> Status {
    let st = Status {
        on: ap_exists() && hostapd_running(),
        clients: run("iw dev ap0 station dump 2>/dev/null | grep -c '^Station'").1.trim().parse().unwrap_or(0),
        wwan: run("nmcli -t -f NAME connection show --active | grep -qx 'China Mobile'").0,
        wifi: run("nmcli -t -f NAME,DEVICE connection show --active | grep -cE '^lap(2\\.4)?:'").1.trim().parse::<usize>().unwrap_or(0) > 0,
        default_via: run("ip route show default | awk '{print $5}' | paste -sd, -").1.trim().to_string(),
    };
    st
}

fn hostapd_conf() -> String {
    format!(
        "interface=ap0\ndriver=nl80211\nssid={SSID}\nhw_mode=g\nchannel={CHANNEL}\nwpa=2\nwpa_passphrase={PASS}\nwpa_key_mgmt=WPA-PSK\nrsn_pairwise=CCMP\nauth_algs=1\nieee80211n=1\nwmm_enabled=1\nignore_broadcast_ssid=0\n"
    )
}

fn on(log: &mut Vec<String>) {
    let wlan = wifi_dev();
    log.push(format!("→ disconnecting WiFi ({wlan}) — iwlwifi single-channel constraint"));
    let _ = run("nmcli connection down hotspot 2>/dev/null");
    let _ = run(&format!("nmcli device disconnect {wlan}"));
    let _ = run(&format!("nmcli device set {wlan} managed no")); // NM off our back
    std::thread::sleep(std::time::Duration::from_millis(800));

    log.push("→ creating AP interface ap0…".into());
    if !ap_exists() {
        let (ok, out) = run(&format!("sudo iw dev {wlan} interface add ap0 type __ap"));
        if !ok { log.push(format!("✗ ap0: {}", out.trim())); return; }
    }
    let _ = run("sudo ip link set ap0 up");

    fs::write(state_dir().join("hostapd.conf"), hostapd_conf()).ok();
    log.push(format!("→ starting hostapd (2.4GHz ch{CHANNEL}, ssid {SSID})…"));
    let (ok, out) = run(&format!("sudo hostapd -B -P {}/hostapd.pid {}/hostapd.conf", state_dir().display(), state_dir().display()));
    if !ok || !hostapd_running() {
        log.push(format!("✗ hostapd failed: {}", out.trim()));
        off(log, true);
        return;
    }

    log.push(format!("→ ap0 {SUBNET}.1/24 + DHCP + NAT via SIM…"));
    let _ = run(&format!("sudo ip addr replace {SUBNET}.1/24 dev ap0"));
    let _ = run("sudo sysctl -q -w net.ipv4.ip_forward=1");
    let _ = run("sudo iptables -t nat -A POSTROUTING -s 10.66.66.0/24 ! -o ap0 -j MASQUERADE");
    let _ = run("sudo iptables -A FORWARD -i ap0 -j ACCEPT");
    let _ = run("sudo iptables -A FORWARD -o ap0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT");
    let (ok, out) = run(&format!(
        "sudo dnsmasq --interface=ap0 --bind-interfaces --port=0 \
         --dhcp-range={SUBNET}.50,{SUBNET}.200,255.255.255.0,12h \
         --dhcp-option=6,223.5.5.5 --dhcp-option=3,{SUBNET}.1 \
         --pid-file={}/dnsmasq.pid --log-facility={}/dnsmasq.log",
        state_dir().display(), state_dir().display()
    ));
    if !ok { log.push(format!("✗ dnsmasq: {}", out.trim())); }
    let s = poll();
    log.push(format!(
        "✓ hotspot ON — clients: {SUBNET}.x via {} — connect to {SSID} / {PASS}",
        if s.wwan { "SIM (CMCC)" } else { "default route" }
    ));
}

fn off(log: &mut Vec<String>, wifi_restore: bool) {
    let _ = run(&format!("sudo kill $(cat {}/hostapd.pid) 2>/dev/null", state_dir().display()));
    let _ = run("sudo pkill -f 'hostapd .*hotspot/hostapd.conf' 2>/dev/null");
    let _ = run(&format!("sudo kill $(cat {}/dnsmasq.pid) 2>/dev/null", state_dir().display()));
    let _ = run("sudo iptables -t nat -D POSTROUTING -s 10.66.66.0/24 ! -o ap0 -j MASQUERADE 2>/dev/null");
    let _ = run("sudo iptables -D FORWARD -i ap0 -j ACCEPT 2>/dev/null");
    let _ = run("sudo iptables -D FORWARD -o ap0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null");
    let _ = run("sudo iw dev ap0 del 2>/dev/null");
    let _ = run("sudo ip addr flush dev ap0 2>/dev/null");
    let wlan = wifi_dev();
    let _ = run(&format!("nmcli device set {wlan} managed yes"));
    if wifi_restore {
        std::thread::sleep(std::time::Duration::from_millis(500));
        for c in ["lap2.4", "lap"] {
            let (ok, _) = run(&format!("nmcli connection up '{c}'"));
            log.push(if ok { format!("✓ {c} reconnected") } else { format!("· {c} not reconnected") });
        }
    }
    log.push("✓ hotspot OFF".into());
}

struct App { st: Status, log: Vec<String>, quit: bool, busy: bool }

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(5), Constraint::Length(1)])
        .split(f.area());
    let s = &app.st;
    let badge = if s.on {
        Span::styled(" HOTSPOT ON ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" HOTSPOT OFF ", Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD))
    };
    let dim = Style::default().fg(Color::DarkGray);
    let lines = vec![
        Line::from(vec![badge, Span::raw("  "),
            Span::styled(format!("default route: {}", if s.default_via.is_empty() { "none".into() } else { s.default_via.clone() }),
                Style::default().fg(if s.default_via.contains("wwp") { Color::Green } else { Color::Yellow }))]),
        Line::from(vec![Span::styled(format!("SSID    {SSID}  ({PASS})  2.4GHz ch{CHANNEL}"), dim)]),
        Line::from(vec![Span::styled("Clients ", dim),
            Span::styled(s.clients.to_string(), Style::default().fg(Color::Cyan)),
            Span::raw("    "),
            Span::styled("WiFi ", dim), Span::raw(if s.wifi { "connected" } else { "down" }),
            Span::raw("    "),
            Span::styled("SIM ", dim), Span::raw(if s.wwan { "CMCC" } else { "down" })]),
        Line::from(vec![Span::styled("AP pool 10.66.66.50-200  gateway .1  dns 223.5.5.5", dim)]),
    ];
    f.render_widget(Paragraph::new(lines).block(Block::bordered().title(" Status ")), chunks[0]);
    let tail: Vec<Line> = app.log.iter().rev().take(40).rev().cloned().map(Line::from).collect();
    f.render_widget(Paragraph::new(tail).block(Block::bordered().title(" Log ")), chunks[1]);
    f.render_widget(
        Paragraph::new("s start (drops WiFi, shares SIM) · x stop (restores WiFi) · q quit")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut log = Vec::new();
    match args.first().map(String::as_str) {
        Some("--on") => { on(&mut log); log.iter().for_each(|l| println!("{l}")); return; }
        Some("--off") => { off(&mut log, true); log.iter().for_each(|l| println!("{l}")); return; }
        Some("--status") => {
            let s = poll();
            println!("on={} clients={} wifi={} sim={} via={}", s.on, s.clients, s.wifi, s.wwan, s.default_via);
            return;
        }
        _ => {}
    }

    use crossterm::{
        event::{self, Event, KeyCode, KeyEventKind},
        terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
        ExecutableCommand,
    };
    enable_raw_mode().expect("tty required (or use --on/--off/--status)");
    std::io::stdout().execute(EnterAlternateScreen).unwrap();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout())).unwrap();
    let mut app = App { st: poll(), log: vec!["ready.".into()], quit: false, busy: false };
    while !app.quit {
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        if event::poll(std::time::Duration::from_millis(1000)).unwrap() {
            if let Event::Key(k) = event::read().unwrap() {
                if k.kind != KeyEventKind::Press { continue; }
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
                    KeyCode::Char('s') if !app.st.on && !app.busy => {
                        app.busy = true; on(&mut app.log); app.st = poll(); app.busy = false;
                    }
                    KeyCode::Char('x') if app.st.on && !app.busy => {
                        app.busy = true; off(&mut app.log, true); app.st = poll(); app.busy = false;
                    }
                    _ => {}
                }
                continue;
            }
        }
        app.st = poll();
    }
    disable_raw_mode().ok();
    std::io::stdout().execute(LeaveAlternateScreen).ok();
}
