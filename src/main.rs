// hotspot-tui — phone-style personal hotspot for the X1 Carbon (iwlwifi).
//
// The NM/wpa_supplicant AP path is broken on this kernel (multicast RX
// registration + iwlmvm firmware quirks), so we run the classic stack
// directly: iw __ap vif + hostapd (2.4G) + dnsmasq (DHCP) + NAT, sharing
// the cellular route. UFW defaults (deny-in/deny-routed) need explicit
// route + input rules on ap0. `managed no` (NOT `device disconnect`)
// releases the phy without leaking iwlmvm AP-context state.
// Requires passwordless sudo; disconnects WiFi while ON.
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
    Frame,
};
use std::collections::VecDeque;
use std::fs;
use std::path::PathBuf;
use std::process::Command;

const SSID: &str = "X1C";
const PASS: &str = "x1carbon123";
const CHANNEL: &str = "6"; // 2.4GHz (see README)
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
    let p = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.local/state", std::env::var("HOME").unwrap_or("/tmp".into()))))
        .join("hotspot");
    fs::create_dir_all(&p).ok();
    p
}

fn wifi_dev() -> String {
    run("nmcli -t -f DEVICE,TYPE device | awk -F: '$2==\"wifi\"{print $1; exit}'").1.trim().to_string()
}
fn ap_exists() -> bool { run("iw dev ap0 info >/dev/null 2>&1").0 }
fn hostapd_running() -> bool {
    fs::read_to_string(state_dir().join("hostapd.pid"))
        .ok()
        .and_then(|p| fs::read_dir(format!("/proc/{}", p.trim())).ok().map(|_| ()))
        .is_some()
}

struct Status { on: bool, clients: usize, wwan: bool, wifi: bool, default_via: String }

fn poll() -> Status {
    Status {
        on: ap_exists() && hostapd_running(),
        clients: run("iw dev ap0 station dump 2>/dev/null | grep -c '^Station'").1.trim().parse().unwrap_or(0),
        wwan: run("nmcli -t -f NAME connection show --active | grep -qx 'China Mobile'").0,
        wifi: run("nmcli -t -f NAME,DEVICE connection show --active | grep -cE '^lap(2\\.4)?:'").1.trim().parse::<usize>().unwrap_or(0) > 0,
        default_via: run("ip route show default | awk '{print $5}' | paste -sd, -").1.trim().to_string(),
    }
}

// ---------------- step queue ----------------

struct Ctx { log: Vec<String>, fail: bool }
type Task = Box<dyn FnOnce(&mut Ctx)>;

fn push(ctx: &mut Ctx, s: impl Into<String>) { ctx.log.push(s.into()); }

fn on_tasks() -> Vec<Task> {
    let mut t: Vec<Task> = Vec::new();
    t.push(Box::new(|c| {
        push(c, format!("→ releasing WiFi ({}) from NetworkManager…", wifi_dev()));
        let _ = run("nmcli connection down hotspot 2>/dev/null");
        let wlan = wifi_dev();
        let _ = run(&format!("nmcli device set {wlan} managed no"));
    }));
    t.push(Box::new(|c| {
        push(c, "· waiting for supplicant cleanup (1.5s)…");
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }));
    // Firmware-level reset: iwlmvm leaks the AP context on teardown
    // ("Match already configured" on every later start); only a driver
    // reload clears it. rfkill cycling is NOT enough.
    t.push(Box::new(|c| {
        push(c, "→ resetting wifi driver (clears iwlmvm AP context)…");
        let _ = run("nmcli radio wifi off");
        std::thread::sleep(std::time::Duration::from_millis(1200));
        let _ = run("sudo modprobe -r iwlmvm iwlwifi 2>/dev/null || sudo rmmod iwlmvm iwlwifi 2>/dev/null || true");
        std::thread::sleep(std::time::Duration::from_millis(1000));
        let _ = run("sudo modprobe iwlwifi");
        std::thread::sleep(std::time::Duration::from_millis(3500));
        let wlan = wifi_dev();
        let _ = run(&format!("nmcli device set {wlan} managed no"));
        std::thread::sleep(std::time::Duration::from_millis(500));
    }));
    t.push(Box::new(|c| {
        push(c, "→ creating AP interface ap0…");
        if !ap_exists() {
            let (ok, out) = run(&format!("sudo iw dev {} interface add ap0 type __ap", wifi_dev()));
            if !ok { push(c, format!("✗ ap0: {}", out.trim())); c.fail = true; return; }
        }
        let _ = run("sudo ip link set ap0 up");
    }));
    t.push(Box::new(|_c| {
        let conf = format!(
            "interface=ap0\ndriver=nl80211\nssid={SSID}\nhw_mode=g\nchannel={CHANNEL}\nwpa=2\nwpa_passphrase={PASS}\nwpa_key_mgmt=WPA-PSK\nrsn_pairwise=CCMP\nauth_algs=1\nieee80211n=1\nwmm_enabled=1\nignore_broadcast_ssid=0\n"
        );
        fs::write(state_dir().join("hostapd.conf"), conf).ok();
    }));
    // up to 4 hostapd attempts, each its own visual step
    for i in 1..=4u8 {
        t.push(Box::new(move |c| {
            if hostapd_running() { return; }
            push(c, format!("→ starting hostapd (2.4GHz ch{CHANNEL}, ssid {SSID})… attempt {i}"));
            let (_, out) = run(&format!("sudo hostapd -B -P {}/hostapd.pid {}/hostapd.conf", state_dir().display(), state_dir().display()));
            std::thread::sleep(std::time::Duration::from_millis(1200));
            if !hostapd_running() {
                let why = out.lines().find(|l| l.contains("reports") || l.contains("failed") || l.contains("Could not"))
                    .or_else(|| out.lines().next()).unwrap_or("");
                push(c, format!("· attempt {i} failed: {why}"));
                let _ = run("sudo pkill -f 'hostapd .*hotspot/hostapd.conf' 2>/dev/null");
                std::thread::sleep(std::time::Duration::from_millis(1500));
            } else {
                push(c, "✓ hostapd up");
            }
        }));
    }
    t.push(Box::new(|c| {
        if !hostapd_running() {
            push(c, "✗ hostapd failed after retries (iwlmvm state? try: reload wifi driver / reboot)");
            c.fail = true;
        }
    }));
    t.push(Box::new(|c| {
        push(c, format!("→ ap0 {SUBNET}.1/24 + DHCP + firewall + NAT…"));
        let _ = run(&format!("sudo ip addr replace {SUBNET}.1/24 dev ap0"));
        let _ = run("sudo sysctl -q -w net.ipv4.ip_forward=1");
        // UFW: routed traffic (forward) + incoming DHCP broadcasts
        let _ = run("sudo ufw route allow in on ap0 comment hotspot-tui 2>/dev/null || true");
        let _ = run("sudo ufw allow in on ap0 to any port 67,68 proto udp comment hotspot-tui 2>/dev/null || true");
        let _ = run("sudo iptables -t nat -C POSTROUTING -s 10.66.66.0/24 ! -o ap0 -j MASQUERADE 2>/dev/null || sudo iptables -t nat -A POSTROUTING -s 10.66.66.0/24 ! -o ap0 -j MASQUERADE");
        let (ok, out) = run(&format!(
            "sudo dnsmasq --interface=ap0 --bind-interfaces --port=0 \
             --dhcp-range={SUBNET}.50,{SUBNET}.200,255.255.255.0,12h \
             --dhcp-option=6,223.5.5.5 --dhcp-option=3,{SUBNET}.1 \
             --pid-file={}/dnsmasq.pid --log-facility={}/dnsmasq.log",
            state_dir().display(), state_dir().display()));
        if !ok { push(c, format!("✗ dnsmasq: {}", out.trim())); c.fail = true; return; }
        let s = poll();
        push(c, format!("✓ hotspot ON — {SSID}/{PASS} — clients {SUBNET}.50-200 via {}",
            if s.wwan { "SIM (CMCC)" } else { "default route" }));
    }));
    t
}

fn off_tasks(restore_wifi: bool) -> Vec<Task> {
    let mut t: Vec<Task> = Vec::new();
    t.push(Box::new(|c| {
        push(c, "→ stopping AP services…");
        let _ = run(&format!("sudo kill $(cat {}/hostapd.pid) 2>/dev/null", state_dir().display()));
        let _ = run("sudo pkill -f 'hostapd .*hotspot/hostapd.conf' 2>/dev/null");
        let _ = run(&format!("sudo kill $(cat {}/dnsmasq.pid) 2>/dev/null", state_dir().display()));
    }));
    t.push(Box::new(|c| {
        push(c, "→ removing firewall rules + ap0…");
        let _ = run("sudo ufw route delete allow in on ap0 comment hotspot-tui 2>/dev/null || true");
        let _ = run("sudo ufw delete allow in on ap0 to any port 67,68 proto udp comment hotspot-tui 2>/dev/null || true");
        let _ = run("sudo iptables -t nat -D POSTROUTING -s 10.66.66.0/24 ! -o ap0 -j MASQUERADE 2>/dev/null || true");
        let _ = run("sudo iptables -D FORWARD -i ap0 -j ACCEPT 2>/dev/null || true");
        let _ = run("sudo iptables -D FORWARD -o ap0 -m conntrack --ctstate RELATED,ESTABLISHED -j ACCEPT 2>/dev/null || true");
        let _ = run("sudo iw dev ap0 del 2>/dev/null || true");
        let wlan = wifi_dev();
        let _ = run(&format!("nmcli device set {wlan} managed yes"));
    }));
    if restore_wifi {
        t.push(Box::new(|c| {
            // rfkill cycle resets iwlwifi firmware, releasing any leaked AP
            // contexts — without this, a second start fails with
            // `Match already configured`.
            push(c, "→ resetting wifi radio…");
            let _ = run("nmcli radio wifi off");
            std::thread::sleep(std::time::Duration::from_millis(1500));
            let _ = run("nmcli radio wifi on");
            std::thread::sleep(std::time::Duration::from_millis(2500));
            push(c, "→ restoring WiFi…");
            for cname in ["lap2.4", "lap"] {
                let (ok, _) = run(&format!("nmcli connection up '{cname}'"));
                if ok { push(c, format!("✓ {cname} reconnected")); }
            }
        }));
    }
    t.push(Box::new(|c| push(c, "✓ hotspot OFF")));
    t
}

// ---------------- UI ----------------

struct App {
    st: Status,
    ctx: Ctx,
    queue: VecDeque<Task>,
    quit: bool,
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(5), Constraint::Length(1)])
        .split(f.area());
    let s = &app.st;
    let busy = !app.queue.is_empty();
    let badge = if s.on {
        Span::styled(" HOTSPOT ON ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" HOTSPOT OFF ", Style::default().fg(Color::Black).bg(Color::Red).add_modifier(Modifier::BOLD))
    };
    let dim = Style::default().fg(Color::DarkGray);
    let lines = vec![
        Line::from(vec![badge,
            Span::styled(if busy { "  working…" } else { "" }, Style::default().fg(Color::Yellow)),
            Span::raw("  "),
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
    let tail: Vec<Line> = app.ctx.log.iter().rev().take(40).rev().cloned().map(Line::from).collect();
    f.render_widget(Paragraph::new(tail).block(Block::bordered().title(" Log ")), chunks[1]);
    f.render_widget(
        Paragraph::new("s start (drops WiFi, shares SIM) · x stop (restores WiFi) · q quit")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );
}

fn drain(tasks: Vec<Task>, log: &mut Vec<String>) {
    let mut ctx = Ctx { log: std::mem::take(log), fail: false };
    let mut tasks = tasks.into_iter();
    loop {
        match tasks.next() {
            Some(t) if !ctx.fail => t(&mut ctx),
            None if !ctx.fail => break,
            _ => {
                ctx.log.push("✗ aborted — rolling back".into());
                ctx.fail = false;
                for t in off_tasks(true) { t(&mut ctx); }
                break;
            }
        }
    }
    *log = ctx.log;
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut log = Vec::new();
    match args.first().map(String::as_str) {
        Some("--on") => { drain(on_tasks(), &mut log); log.iter().for_each(|l| println!("{l}")); return; }
        Some("--off") => { drain(off_tasks(true), &mut log); log.iter().for_each(|l| println!("{l}")); return; }
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
    let mut app = App {
        st: poll(),
        ctx: Ctx { log: vec!["ready.".into()], fail: false },
        queue: VecDeque::new(),
        quit: false,
    };

    while !app.quit {
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        // run ONE queued step per frame → incremental feedback
        if let Some(task) = app.queue.pop_front() {
            task(&mut app.ctx);
            if app.ctx.fail {
                app.ctx.fail = false;
                app.ctx.log.push("✗ aborted — rolling back".into());
                app.queue.clear(); // don't run the rest of the failed sequence
                for t in off_tasks(true) { app.queue.push_back(t); }
            }
            if app.queue.is_empty() { app.st = poll(); }
            continue;
        }
        let timeout = std::time::Duration::from_millis(1000);
        if event::poll(timeout).unwrap() {
            if let Event::Key(k) = event::read().unwrap() {
                if k.kind != KeyEventKind::Press { continue; }
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
                    KeyCode::Char('s') if !app.st.on => {
                        app.ctx.log.push("── start requested ──".into());
                        for t in on_tasks() { app.queue.push_back(t); }
                    }
                    KeyCode::Char('x') if app.st.on => {
                        app.ctx.log.push("── stop requested ──".into());
                        for t in off_tasks(true) { app.queue.push_back(t); }
                    }
                    _ => {}
                }
            }
        }
        app.st = poll();
    }
    disable_raw_mode().ok();
    std::io::stdout().execute(LeaveAlternateScreen).ok();
}
