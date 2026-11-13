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


// ---------------- config ----------------
#[derive(Clone)]
struct Cfg { ssid: String, pass: String, channel: String, subnet: String }
impl Default for Cfg {
    fn default() -> Self {
        Cfg { ssid: SSID.into(), pass: PASS.into(), channel: CHANNEL.into(), subnet: SUBNET.into() }
    }
}
fn cfg_path() -> PathBuf {
    let p = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("{}/.config", std::env::var("HOME").unwrap_or("/tmp".into()))))
        .join("hotspot");
    fs::create_dir_all(&p).ok();
    p.join("config")
}
fn cfg_load() -> Cfg {
    let mut c = Cfg::default();
    if let Ok(txt) = fs::read_to_string(cfg_path()) {
        for l in txt.lines() {
            if let Some((k, v)) = l.split_once('=') {
                match k.trim() {
                    "ssid" => c.ssid = v.trim().to_string(),
                    "password" => c.pass = v.trim().to_string(),
                    "channel" => c.channel = v.trim().to_string(),
                    "subnet" => c.subnet = v.trim().to_string(),
                    _ => {}
                }
            }
        }
    }
    c
}
fn cfg_save(c: &Cfg) -> std::io::Result<()> {
    fs::write(cfg_path(), format!("ssid={}\npassword={}\nchannel={}\nsubnet={}\n", c.ssid, c.pass, c.channel, c.subnet))
}

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

fn on_tasks(cfg: &Cfg) -> Vec<Task> {
    let mut t: Vec<Task> = Vec::new();
    t.push(Box::new(|c| {
        push(c, format!("→ role: client ⇒ BASE STATION · releasing phy ({}) from NM…", wifi_dev()));
        let _ = run("nmcli connection down hotspot 2>/dev/null");
        let wlan = wifi_dev();
        let _ = run(&format!("nmcli device set {wlan} managed no"));
    }));
    t.push(Box::new(|c| {
        push(c, "· supplicant cleanup…");
        std::thread::sleep(std::time::Duration::from_millis(1500));
    }));
    // Firmware-level reset: iwlmvm leaks the AP context on teardown
    // ("Match already configured" on every later start); only a driver
    // reload clears it. rfkill cycling is NOT enough.
    t.push(Box::new(|c| {
        push(c, "→ ⌁ driver reset (clears iwlmvm AP context)…");
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
        push(c, "→ creating ap0 (base-station hat)…");
        if !ap_exists() {
            let (ok, out) = run(&format!("sudo iw dev {} interface add ap0 type __ap", wifi_dev()));
            if !ok { push(c, format!("✗ ap0: {}", out.trim())); c.fail = true; return; }
        }
        let _ = run("sudo ip link set ap0 up");
    }));
    // up to 4 hostapd attempts, each its own visual step
    for i in 1..=4u8 {
        let cfg = cfg.clone();
        t.push(Box::new(move |c| {
            if hostapd_running() { return; }
            push(c, format!("→ ⌁ broadcasting {} on ch{}… attempt {i}", cfg.ssid, cfg.channel));
            let conf = format!(
                "interface=ap0\ndriver=nl80211\nssid={}\nhw_mode=g\nchannel={}\nwpa=2\nwpa_passphrase={}\nwpa_key_mgmt=WPA-PSK\nrsn_pairwise=CCMP\nauth_algs=1\nieee80211n=1\nwmm_enabled=1\nignore_broadcast_ssid=0\n",
                cfg.ssid, cfg.channel, cfg.pass);
            fs::write(state_dir().join("hostapd.conf"), conf).ok();
            let (_, out) = run(&format!("sudo hostapd -B -P {}/hostapd.pid {}/hostapd.conf", state_dir().display(), state_dir().display()));
            std::thread::sleep(std::time::Duration::from_millis(1200));
            if !hostapd_running() {
                let why = out.lines().find(|l| l.contains("reports") || l.contains("failed") || l.contains("Could not"))
                    .or_else(|| out.lines().next()).unwrap_or("");
                // config-level errors are deterministic — retrying is pointless
                if out.contains("Line") && (out.contains("invalid") || out.contains("unknown")) {
                    push(c, format!("✗ hostapd rejected config: {why}"));
                    c.fail = true;
                    return;
                }
                push(c, format!("· attempt {i} failed: {why}"));
                let _ = run("sudo pkill -f 'hostapd .*hotspot/hostapd.conf' 2>/dev/null");
                std::thread::sleep(std::time::Duration::from_millis(1500));
            } else {
                push(c, "✓ radio is broadcasting");
            }
        }));
    }
    t.push(Box::new(|c| {
        if !hostapd_running() {
            push(c, "✗ hostapd failed after retries (iwlmvm state? try: reload wifi driver / reboot)");
            c.fail = true;
        }
    }));
    let cfg = cfg.clone();
    t.push(Box::new(move |c| {
        let subnet = cfg.subnet.clone();
        push(c, format!("→ plumbing: dhcp + firewall + NAT ⇢ SIM…"));
        let _ = run(&format!("sudo ip addr replace {subnet}.1/24 dev ap0"));
        let _ = run("sudo sysctl -q -w net.ipv4.ip_forward=1");
        // UFW: routed traffic (forward) + incoming DHCP broadcasts
        let _ = run("sudo ufw route allow in on ap0 comment hotspot-tui 2>/dev/null || true");
        let _ = run("sudo ufw allow in on ap0 to any port 67,68 proto udp comment hotspot-tui 2>/dev/null || true");
        let _ = run(&format!("sudo iptables -t nat -C POSTROUTING -s {subnet}.0/24 ! -o ap0 -j MASQUERADE 2>/dev/null || sudo iptables -t nat -A POSTROUTING -s {subnet}.0/24 ! -o ap0 -j MASQUERADE"));
        let (ok, out) = run(&format!(
            "sudo dnsmasq --interface=ap0 --bind-interfaces --port=0 \
             --dhcp-range={subnet}.50,{subnet}.200,255.255.255.0,12h \
             --dhcp-option=6,223.5.5.5 --dhcp-option=3,{subnet}.1 \
             --pid-file={}/dnsmasq.pid --log-facility={}/dnsmasq.log",
            state_dir().display(), state_dir().display()));
        if !ok { push(c, format!("✗ dnsmasq: {}", out.trim())); c.fail = true; return; }
        let s = poll();
        push(c, format!("✓ ONLINE  phone ⇐ ap0 ⇐ NAT ⇐ {}   (pool {subnet}.50-200)",
            if s.wwan { "CMCC" } else { "default route" }));
    }));
    t
}

fn off_tasks(restore_wifi: bool) -> Vec<Task> {
    let mut t: Vec<Task> = Vec::new();
    t.push(Box::new(|c| {
        push(c, "→ role: BASE STATION ⇒ client · silencing radio…");
        let _ = run(&format!("sudo kill $(cat {}/hostapd.pid) 2>/dev/null", state_dir().display()));
        let _ = run("sudo pkill -f 'hostapd .*hotspot/hostapd.conf' 2>/dev/null");
        let _ = run(&format!("sudo kill $(cat {}/dnsmasq.pid) 2>/dev/null", state_dir().display()));
    }));
    t.push(Box::new(|c| {
        push(c, "→ removing firewall rules + ap0…");
        let _ = run("sudo ufw route delete allow in on ap0 comment hotspot-tui 2>/dev/null || true");
        let _ = run("sudo ufw delete allow in on ap0 to any port 67,68 proto udp comment hotspot-tui 2>/dev/null || true");
        let cfg = cfg_load();
        let _ = run(&format!("sudo iptables -t nat -D POSTROUTING -s {}.0/24 ! -o ap0 -j MASQUERADE 2>/dev/null || true", cfg.subnet));
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
            push(c, "→ rejoining WiFi (client hat back on)…");
            for cname in ["lap2.4", "lap"] {
                let (ok, _) = run(&format!("nmcli connection up '{cname}'"));
                if ok { push(c, format!("✓ {cname} reconnected")); }
            }
        }));
    }
    t.push(Box::new(|c| push(c, "✓ client mode restored")));
    t
}

// ---------------- UI ----------------

struct App {
    st: Status,
    ctx: Ctx,
    queue: VecDeque<Task>,
    quit: bool,
    cfg: Cfg,
    pop: bool,
    pop_sel: usize,
    pop_edit: bool,
    pop_buf: String,
}


fn validate_field(i: usize, v: &str) -> Result<(), String> {
    match i {
        0 => { if v.is_empty() || v.chars().count() > 32 { Err("ssid needs 1–32 chars".into()) } else { Ok(()) } }
        1 => { let n = v.chars().count(); if (8..=63).contains(&n) { Ok(()) } else { Err(format!("WPA passphrase needs 8–63 chars, got {n}")) } }
        2 => { match v.parse::<u32>() { Ok(c) if (1..=13).contains(&c) => Ok(()), _ => Err("channel: 1–13 (2.4GHz)".into()) } }
        _ => { let p: Vec<&str> = v.split('.').collect();
               if p.len() == 3 && p.iter().all(|o| o.parse::<u8>().is_ok()) { Ok(()) } else { Err("subnet: three octets, e.g. 10.66.66".into()) } }
    }
}

fn field_get(c: &Cfg, i: usize) -> String {
    match i { 0 => c.ssid.clone(), 1 => c.pass.clone(), 2 => c.channel.clone(), _ => c.subnet.clone() }
}
fn field_set(c: &mut Cfg, i: usize, v: String) {
    match i { 0 => c.ssid = v, 1 => c.pass = v, 2 => c.channel = v, _ => c.subnet = v }
}
const FIELDS: [&str; 4] = ["ssid", "password", "channel", "subnet"];

fn centered(r: ratatui::layout::Rect, w: u16, h: u16) -> ratatui::layout::Rect {
    let w = w.min(r.width.saturating_sub(2));
    let h = h.min(r.height.saturating_sub(2));
    ratatui::layout::Rect {
        x: r.x + (r.width.saturating_sub(w)) / 2,
        y: r.y + (r.height.saturating_sub(h)) / 2,
        width: w,
        height: h,
    }
}

fn draw(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(9), Constraint::Min(5), Constraint::Length(1)])
        .split(f.area());
    let s = &app.st;
    let busy = !app.queue.is_empty();

    let dot_on = Style::default().fg(Color::Green);
    let dot_off = Style::default().fg(Color::DarkGray);
    let dim = Style::default().fg(Color::DarkGray);
    let link = |c: &'static str| Span::styled(c, dim);

    let badge = if s.on {
        Span::styled(" ⦿ BASE STATION ", Style::default().fg(Color::Black).bg(Color::Green).add_modifier(Modifier::BOLD))
    } else {
        Span::styled(" ○ CLIENT ", Style::default().fg(Color::Black).bg(Color::Blue).add_modifier(Modifier::BOLD))
    };

    // role line: the radio can wear exactly one hat at a time
    let role = vec![
        Span::styled("role    ", dim),
        Span::styled(if s.on { "client ○" } else { "client ⦿" }, if s.on { dot_off } else { dot_on }),
        link(" ──── ⌁ radio ⌁ ────"),
        Span::styled(if s.on { " base-station ⦿" } else { " base-station ○" }, if s.on { dot_on } else { dot_off }),
        if busy { Span::styled("   …switching", Style::default().fg(Color::Yellow)) } else { Span::raw("") },
    ];

    // uplink line: who carries whose traffic right now
    let uplink = if s.on {
        let ap = Span::styled(if s.on { "⦿" } else { "○" }, dot_on);
        let nat = Span::styled("⦿", dot_on);
        let sim = Span::styled(if s.wwan { "⦿" } else { "○" }, if s.wwan { dot_on } else { dot_off });
        let net = Span::styled(if s.wwan { "⦿" } else { "○" }, if s.wwan { dot_on } else { dot_off });
        vec![
            Span::styled("uplink  ", dim),
            Span::styled("phone", Style::default().fg(Color::Cyan)), link(" ─"), ap.clone(), link(" ap0 ─"),
            nat, link(" NAT ─"),
            Span::styled("SIM", Style::default().fg(Color::Cyan)), link("("), Span::styled(if s.wwan { "CMCC LTE" } else { "no network" }, dim), link(") ─"),
            sim, link(" internet "), net,
        ]
    } else {
        let w = Span::styled(if s.wifi { "⦿" } else { "○" }, if s.wifi { dot_on } else { dot_off });
        let sim = Span::styled("○", dot_off);
        vec![
            Span::styled("uplink  ", dim),
            Span::styled("laptop", Style::default().fg(Color::Cyan)), link(" ─"), w, link(" WiFi(lap*) ─ internet"),
            Span::raw("      "),
            Span::styled("SIM idle ", dim), sim,
        ]
    };

    let info = vec![
        Span::styled("when AP ", dim),
        Span::raw(format!("{} / {} · ch{} · pool {}.50-200 · gw .1 · dns 223.5.5.5", app.cfg.ssid, app.cfg.pass, app.cfg.channel, app.cfg.subnet)),
    ];
    let clients = vec![
        Span::styled("clients ", dim),
        Span::styled(s.clients.to_string(), Style::default().fg(Color::Cyan)),
        Span::raw("    "),
        Span::styled(format!("default route: {}", if s.default_via.is_empty() { "none" } else { s.default_via.as_str() }), dim),
    ];

    f.render_widget(
        Paragraph::new(vec![
            Line::from(vec![badge]),
            Line::from(role),
            Line::from(uplink),
            Line::from(info),
            Line::from(clients),
        ])
        .block(Block::bordered().title(" Status · a radio wears one hat at a time ")),
        chunks[0],
    );
    let tail: Vec<Line> = app.ctx.log.iter().rev().take(40).rev().cloned().map(Line::from).collect();
    f.render_widget(Paragraph::new(tail).block(Block::bordered().title(" Log ")), chunks[1]);
    f.render_widget(
        Paragraph::new("s ⇒ base-station · x ⇒ client · c config · q quit")
            .style(Style::default().fg(Color::DarkGray)),
        chunks[2],
    );

    if app.pop {
        let area = centered(chunks[1], 58, 10);
        f.render_widget(ratatui::widgets::Clear, area);
        let mut lines: Vec<Line> = Vec::new();
        for (i, name) in FIELDS.iter().enumerate() {
            let val = if app.pop_edit && i == app.pop_sel { app.pop_buf.clone() } else { field_get(&app.cfg, i) };
            let sty = if i == app.pop_sel {
                Style::default().add_modifier(Modifier::REVERSED)
            } else {
                Style::default()
            };
            lines.push(Line::from(Span::styled(format!(" {name:<9}= {val}"), sty)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            if app.pop_edit { "Enter commit · Esc abort" } else { "↑↓ field · Enter edit · Esc close (saved on edit)" },
            Style::default().fg(Color::DarkGray),
        )));
        let block = Block::bordered()
            .title(format!(" Config  {} ", cfg_path().display()))
            .border_style(Style::default().fg(Color::Cyan));
        let inner = block.inner(area);
        f.render_widget(block, area);
        f.render_widget(Paragraph::new(lines), inner);
        if app.pop_edit {
            let x = inner.x + 1 + (FIELDS[app.pop_sel].len() as u16 + 4 + app.pop_buf.chars().count() as u16).min(inner.width.saturating_sub(2));
            f.set_cursor_position((x, inner.y + app.pop_sel as u16));
        }
    }
}

fn drain(_tasks: Vec<Task>, log: &mut Vec<String>) {
    let mut ctx = Ctx { log: std::mem::take(log), fail: false };
    let mut tasks = _tasks.into_iter();
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
        Some("--on") => { drain(on_tasks(&cfg_load()), &mut log); log.iter().for_each(|l| println!("{l}")); return; }
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
        cfg: cfg_load(),
        pop: false,
        pop_sel: 0,
        pop_edit: false,
        pop_buf: String::new(),
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
                if app.pop {
                    match (app.pop_edit, k.code) {
                        (true, KeyCode::Esc) => { app.pop_edit = false; }
                        (true, KeyCode::Enter) => {
                            let v = app.pop_buf.trim().to_string();
                            match validate_field(app.pop_sel, &v) {
                                Ok(()) => {
                                    field_set(&mut app.cfg, app.pop_sel, v.clone());
                                    let _ = cfg_save(&app.cfg);
                                    app.ctx.log.push(format!("✓ config saved: {} = {}", FIELDS[app.pop_sel], v));
                                    app.pop_edit = false;
                                }
                                Err(e) => app.ctx.log.push(format!("✗ invalid {}: {e}", FIELDS[app.pop_sel])),
                            }
                        }
                        (true, KeyCode::Backspace) => { app.pop_buf.pop(); }
                        (true, KeyCode::Char(ch)) => app.pop_buf.push(ch),
                        (false, KeyCode::Esc) => app.pop = false,
                        (false, KeyCode::Down) | (false, KeyCode::Char('j')) => app.pop_sel = (app.pop_sel + 1) % 4,
                        (false, KeyCode::Up) | (false, KeyCode::Char('k')) => app.pop_sel = app.pop_sel.saturating_sub(1),
                        (false, KeyCode::Enter) => {
                            app.pop_buf = field_get(&app.cfg, app.pop_sel);
                            app.pop_edit = true;
                        }
                        _ => {}
                    }
                    continue;
                }
                match k.code {
                    KeyCode::Char('q') | KeyCode::Esc => app.quit = true,
                    KeyCode::Char('c') => {
                        app.pop = true;
                        app.pop_edit = false;
                    }
                    KeyCode::Char('s') if !app.st.on => {
                        app.ctx.log.push("── switch: client ⇒ BASE STATION ──".into());
                        for t in on_tasks(&app.cfg.clone()) { app.queue.push_back(t); }
                    }
                    KeyCode::Char('x') if app.st.on => {
                        app.ctx.log.push("── switch: BASE STATION ⇒ client ──".into());
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
