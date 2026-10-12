//! Token Café in a desktop window. A thread reads this machine's Claude Code
//! and Codex sessions every second, another re-counts this month's tokens
//! every minute, and the window animates the café at 30 fps.
//!
//! Usage:
//!   token-cafe              the live café
//!   token-cafe --demo       a fixed list that shows every pose
//!   token-cafe --print      print the sessions and this month's usage, and exit
//!   token-cafe --scale N    start at N times 320x240 (default 3)
//!   token-cafe --always-on-top  keep the café above other windows
//! Keys: 1-6 window size, f / F11 fullscreen, Esc leave fullscreen, t toggle always on top, q quit.

mod app;

use std::sync::mpsc::{Sender, channel};
use std::thread;
use std::time::Duration;

use token_cafe::collect::Collector;
use token_cafe::scene::{ForcedPose, Kind, Session, State, Usage, Why};
use token_cafe::usage::{Counter, human};

const POLL: Duration = Duration::from_secs(1); // how often the sessions are read
const USAGE_EVERY: Duration = Duration::from_secs(60); // how often this month's tokens are re-counted

/// What the feeder threads hand the window.
pub enum Update {
    Sessions(Vec<Session>),
    Usage(Usage),
}

fn log(msg: &str) {
    println!("{} {msg}", chrono::Local::now().format("%H:%M:%S"));
}

/// "name=wait/perm"
fn describe(s: &Session) -> String {
    let state = match s.state {
        State::Work => "work",
        State::Idle => "idle",
        State::Wait => "wait",
    };
    let why = match s.why {
        Why::None => "",
        Why::Perm => "/perm",
        Why::Ask => "/ask",
    };
    format!("{}={state}{why}", s.name)
}

fn sessions_feed(tx: Sender<Update>) {
    let mut collector = Collector::new();
    let mut last = None;
    loop {
        let list = collector.sessions();
        let seen: Vec<String> = list.iter().map(|s| format!("{}:{}", s.id, describe(s))).collect();
        if last.as_ref() != Some(&seen) {
            let names: Vec<String> = list.iter().map(describe).collect();
            log(&format!("sessions: {}", if names.is_empty() { "none".into() } else { names.join(", ") }));
            last = Some(seen);
        }
        if tx.send(Update::Sessions(list)).is_err() {
            return;
        }
        thread::sleep(POLL);
    }
}

/// A changed transcript is re-read whole, which takes a moment for a long
/// session, so this runs apart from the session poll.
fn usage_feed(tx: Sender<Update>) {
    let mut counter = Counter::new();
    let mut first = true;
    loop {
        let u = counter.month();
        if first {
            log(&format!("this month: {} tokens, ${:.2}", human(u.tokens), u.usd));
            first = false;
        }
        if tx.send(Update::Usage(Usage { tokens: u.tokens as f64, usd: u.usd })).is_err() {
            return;
        }
        thread::sleep(USAGE_EVERY);
    }
}

fn demo_feed(tx: Sender<Update>) {
    let s = |id: &str, name: &str, kind: Kind, state: State, why: Why, pose: ForcedPose| Session {
        id: id.into(),
        name: name.into(),
        kind,
        state,
        why,
        pose,
        age_s: 0,
    };
    use {ForcedPose as P, Kind::*, State::*};
    let list = vec![
        s("b07e11a2", "api-server", Claude, Work, Why::None, P::Auto),
        s("24eb1748", "checkout-flow-1c", Claude, Wait, Why::Perm, P::Auto),
        s("516264dc", "docs-8a", Claude, Work, Why::None, P::Think),
        s("1a342e2a", "migrate-34", Claude, Idle, Why::None, P::Sleep),
        s("c0dex9f1", "infra", Codex, Wait, Why::Ask, P::Auto),
        s("01a10a65", "web-app", Codex, Idle, Why::None, P::Sip),
        s("9a7c33d0", "billing", Claude, Idle, Why::None, P::Stretch),
        s("01a11478", "ml-pipeline-6e", Codex, Work, Why::None, P::Auto),
    ];
    let _ = tx.send(Update::Usage(Usage { tokens: 1_624_000_000.0, usd: 693.70 }));
    while tx.send(Update::Sessions(list.clone())).is_ok() {
        thread::sleep(POLL);
    }
}

fn print_once() {
    let list = Collector::new().sessions();
    if list.is_empty() {
        println!("no sessions");
    }
    for s in &list {
        let kind = if s.kind == Kind::Codex { "codex" } else { "claude" };
        let a = s.age_s;
        let age = if a < 60 {
            format!("{a}s")
        } else if a < 3600 {
            format!("{}m", a / 60)
        } else {
            format!("{}h{:02}m", a / 3600, a / 60 % 60)
        };
        println!("{:<8}  {:<6}  {:<28}  for {age}", s.id, kind, describe(s));
    }
    let u = Counter::new().month();
    println!("this month: {} tokens ({}), ${:.2}", human(u.tokens), u.tokens, u.usd);
}

fn usage_text() -> &'static str {
    "usage: token-cafe [--demo | --print] [--scale N] [--always-on-top]\n\
     \n  --demo     a fixed list that shows every pose\
     \n  --print    print the sessions and this month's usage, and exit\
     \n  --scale N  start at N times 320x240 (default 3)\
     \n  --always-on-top  keep the café above other windows\
     \n\nkeys: 1-6 window size, f / F11 fullscreen, Esc leave fullscreen, t toggle always on top, q quit"
}

fn main() {
    let (mut demo, mut print, mut scale) = (false, false, 3u32);
    let mut always_on_top = false;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--always-on-top" => always_on_top = true,
            "--demo" => demo = true,
            "--print" => print = true,
            "--scale" => match args.next().and_then(|v| v.parse().ok()).filter(|&k| (1..=16).contains(&k)) {
                Some(k) => scale = k,
                None => return eprintln!("--scale wants a whole number from 1 to 16\n\n{}", usage_text()),
            },
            "-h" | "--help" => return println!("{}", usage_text()),
            other => return eprintln!("unknown argument {other}\n\n{}", usage_text()),
        }
    }
    if print {
        return print_once();
    }
    let (tx, rx) = channel();
    if demo {
        thread::spawn(move || demo_feed(tx));
    } else {
        let tx2 = tx.clone();
        thread::spawn(move || sessions_feed(tx));
        thread::spawn(move || usage_feed(tx2));
    }
    if let Err(e) = app::run(rx, scale, always_on_top) {
        eprintln!("token-cafe: {e}");
        std::process::exit(1);
    }
}
