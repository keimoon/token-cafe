//! The café: seats, patrons, walking, poses, particles and the message
//! window, behaving as on the Token Café design page (design/cafe.js). The
//! collector only says which sessions exist and what state each is in;
//! everything that moves is decided here.

use std::collections::HashMap;
use std::f64::consts::PI;

use chrono::{NaiveTime, Timelike};

use crate::assets::{self as a, Pal, Sprite, base, c, slot};
use crate::gfx::{Gfx, text_width};

pub const SEATS: usize = 8;
const TASK_LINE_LEN: usize = 19; // between the portrait and scrollbar touch target
const PLATE_LEN: usize = 8; // shown on the desk nameplate

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Claude,
    Codex,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Work,
    Idle,
    Wait,
}

/// Why a waiting session waits: a permission prompt ("!") or anything else ("?").
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    None,
    Perm,
    Ask,
}

/// A forced pose, for the demo; Auto lets the scene choose.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ForcedPose {
    Auto,
    Think,
    Stretch,
    Sip,
    Sleep,
}

#[derive(Clone, Debug)]
pub struct Session {
    /// Stable per session: the patron's looks are a hash of it.
    pub id: String,
    pub name: String,
    pub kind: Kind,
    pub state: State,
    pub why: Why,
    pub pose: ForcedPose,
    /// How long the session has been in this state, so a restarted app
    /// still knows who has been idle long enough to doze off.
    pub age_s: u32,
}

/// Only sessions associated with a desktop app receive a navigation target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionTarget {
    Claude(String),
    Codex(String),
}

impl SessionTarget {
    pub fn url(&self) -> Option<String> {
        let (id, prefix) = match self {
            Self::Claude(id) => (id.strip_prefix("local_")?, "claude://code/continue?session=local_"),
            Self::Codex(id) => (id.as_str(), "codex://threads/"),
        };
        // Session IDs are data, never arbitrary URLs or shell arguments.
        if id.is_empty() || id.len() > 64 || !id.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return None;
        }
        Some(format!("{prefix}{id}"))
    }
}

/// Short, evidence-backed display fields, separate from the animated session state.
#[derive(Clone, Debug)]
pub struct SessionDetail {
    pub target: Option<SessionTarget>,
    pub id: String,
    pub task: String,
}

/// This month's totals for the tab on the wall, Claude and Codex summed.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Usage {
    pub tokens: f64,
    pub usd: f64,
}

/* ---- layout, as in design/cafe.js --------------------------------------- */

const COLS: i32 = 4;
const STATION_W: i32 = 80;
const STATION_H: i32 = 72;
const FLOOR_Y: i32 = 48;
const SIT_X: i32 = 32; // seated base box inside a station
const SIT_Y: i32 = 6;
const CHAIR_X: i32 = 30;
const CHAIR_Y: i32 = 2;
const DESK_X: i32 = 12;
const DESK_Y: i32 = 30;
const DESK_W: i32 = 56;
const LID_X: i32 = 34;
const LID_Y: i32 = 24;
const BX: i32 = 3; // base box offset inside character sprites
const BY: i32 = 6;
const WALK_SPEED: f64 = 60.0; // px per second
const THINK_EVERY: i64 = 11000;
const THINK_FOR: i64 = 2600;
const FOCUS_MS: i64 = 3500;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Seated,
    Arriving,
    Leaving,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Pose {
    Type,
    Think,
    Raise,
    Stretch,
    Sip,
    Sleep,
}

struct Agent {
    id: String,
    name: String,
    codex: bool,
    state: State,
    why: Why,
    force: ForcedPose,
    pal: Pal,
    style: usize,
    variant: u32,
    phase: i64,
    seat: usize,
    mode: Mode,
    x: f64,
    y: f64,
    dist: f64,
    dir: Dir,
    path: Vec<[f64; 2]>,
    path_i: usize,
    since: i64,
    zacc: i64,
    seen: bool,
    gone: bool,
}

impl Agent {
    fn remaining(&self) -> usize {
        self.path.len() - self.path_i
    }

    fn start_walk(&mut self, path: &[[f64; 2]], mode: Mode) {
        self.mode = mode;
        self.x = path[0][0];
        self.y = path[0][1];
        self.dist = 0.0;
        self.path = path[1..].to_vec();
        self.path_i = 0;
    }
}

#[derive(Clone, Copy)]
enum PKind {
    Z,
    Steam,
    Key,
}

#[derive(Clone, Copy)]
struct Particle {
    kind: PKind,
    x: f64,
    y: f64,
    age: i64,
    life: i64,
    seat: usize,
    glyph: usize,
    color: u32,
}

impl Particle {
    fn new(kind: PKind, x: f64, y: f64, life: i64) -> Self {
        Particle { kind, x, y, age: 0, life, seat: 0, glyph: 0, color: 0 }
    }
}

pub struct Scene {
    /// Milliseconds of animation so far.
    t: i64,
    /// In arrival order.
    agents: Vec<Agent>,
    particles: Vec<Particle>,
    focus: usize,
    focus_at: i64,
    overflow: usize,
    /// No list yet: the first one seats everyone at once.
    first: bool,
    usage: Option<Usage>,
    sessions: Vec<Session>,
    details: Vec<SessionDetail>,
    pages: HashMap<String, Vec<String>>,
    unavailable: [String; 1],
    selected: Option<String>,
    task_page: usize,
    bg: Box<[u32]>,
}

struct Geom {
    sx: i32,
    sy: i32,
    col: i32,
}

fn seat_geom(i: usize) -> Geom {
    let (col, row) = (i as i32 % COLS, i as i32 / COLS);
    Geom { sx: col * STATION_W, sy: FLOOR_Y + row * STATION_H, col }
}

/// Feet positions from the door (under the message window) to a chair.
fn path_to(i: usize) -> [[f64; 2]; 5] {
    let g = seat_geom(i);
    let gap = (if g.col < COLS / 2 { g.sx + STATION_W } else { g.sx }) as f64;
    let aisle = (g.sy + 66) as f64;
    let (sx, sy) = (g.sx as f64, g.sy as f64);
    [[160.0, 262.0], [160.0, aisle], [gap, aisle], [gap, sy + 28.0], [sx + 40.0, sy + 28.0]]
}

fn seat_props(i: usize) -> &'static [(&'static Sprite, i32, i32)] {
    const P: [&[(&Sprite, i32, i32)]; SEATS] = [
        &[(&a::PROP_PLANT, 15, 25)],
        &[(&a::PROP_DUCK, 56, 29)],
        &[(&a::PROP_NOODLES, 16, 29), (&a::PROP_CAN, 61, 29)],
        &[(&a::PROP_BOOKS, 54, 30)],
        &[(&a::PROP_CAN, 16, 29)],
        &[(&a::PROP_PLANT, 57, 25)],
        &[(&a::PROP_DUCK, 15, 29)],
        &[(&a::PROP_NOODLES, 56, 29)],
    ];
    P[i]
}

/* ---- looks -------------------------------------------------------------- */

fn fnv1a(s: &str) -> u32 {
    s.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619))
}

struct Look {
    pal: Pal,
    style: usize,
    variant: u32,
    phase: i64,
}

/// Skin, hair colour and style, shirt and idle preference: a hash of the
/// session id, so a patron looks the same every time.
fn look_for(id: &str) -> Look {
    let h = fnv1a(id);
    let sk = a::SKINS[h as usize % a::SKINS.len()];
    let hr = a::HAIRS[(h >> 3) as usize % a::HAIRS.len()];
    let sh = a::SHIRTS[(h >> 7) as usize % a::SHIRTS.len()];
    let mut pal = Pal::base();
    pal.set(slot::S, sk[0]);
    pal.set(slot::s, sk[1]);
    pal.set(slot::H, hr[0]);
    pal.set(slot::h, hr[1]);
    pal.set(slot::J, hr[2]);
    pal.set(slot::C, sh[0]);
    pal.set(slot::c, sh[1]);
    pal.set(slot::V, a::LOOK_V);
    pal.set(slot::P, a::PANTS[(h >> 11) as usize % a::PANTS.len()]);
    pal.set(slot::F, a::LOOK_F);
    Look { pal, style: (h >> 20) as usize % a::POSES.len(), variant: (h >> 13) % 3, phase: ((h >> 16) % 9000) as i64 }
}

/// 'mithrandir-1c' -> 'mithr-1c' for n=8: keep the session suffix.
pub fn shorten(name: &str, n: usize) -> String {
    if name.chars().count() <= n {
        return name.to_string();
    }
    if let Some((base, suf)) = name.rsplit_once('-') {
        let sl = suf.chars().count();
        if !base.is_empty() && (1..=3).contains(&sl) {
            let cut: String = base.chars().take(n - 1 - sl).collect();
            return format!("{}-{suf}", cut.trim_end_matches(['-', '_', ' ']));
        }
    }
    name.chars().take(n).collect()
}

fn jround(v: f64) -> i32 {
    (v + 0.5).floor() as i32 // Math.round
}

fn pose_of(t: i64, a: &Agent) -> Pose {
    match a.state {
        State::Wait => return Pose::Raise,
        State::Work => {
            let ph = (t + a.phase) % THINK_EVERY;
            return if a.force == ForcedPose::Think || ph < THINK_FOR { Pose::Think } else { Pose::Type };
        }
        State::Idle => {}
    }
    match a.force {
        ForcedPose::Think => return Pose::Think,
        ForcedPose::Stretch => return Pose::Stretch,
        ForcedPose::Sip => return Pose::Sip,
        ForcedPose::Sleep => return Pose::Sleep,
        ForcedPose::Auto => {}
    }
    // idle: the longer the human leaves them, the deeper the slack
    let idle = t - a.since;
    if idle > 10 * 60000 {
        Pose::Sleep
    } else if idle > 2 * 60000 || a.variant == 2 {
        Pose::Sip
    } else {
        Pose::Stretch
    }
}

/* ---- the tab: this month's tokens and dollars, both tools summed -------- */

const TAB_X: i32 = 237;
const TAB_Y: i32 = 5;
const TAB_W: i32 = 74;

/// At most 5 characters (6 for "$10.0K"); floor() rounding as in cafe.js.
fn tenths(v: f64, unit: char) -> String {
    let t = (v + 0.5).floor() as i64;
    format!("{}.{}{}", t / 10, t % 10, unit)
}

pub fn fmt_tokens(v: f64) -> String {
    if v < 1e3 {
        format!("{}", v.floor() as i64)
    } else if v < 1e6 {
        format!("{}K", (v / 1e3 + 0.5).floor() as i64)
    } else if v < 1e7 {
        tenths(v / 1e5, 'M')
    } else if v < 1e9 {
        format!("{}M", (v / 1e6 + 0.5).floor() as i64)
    } else if v < 1e11 {
        tenths(v / 1e8, 'B')
    } else {
        format!("{}B", (v / 1e9 + 0.5).floor() as i64)
    }
}

pub fn fmt_usd(v: f64) -> String {
    if v < 999.5 {
        format!("${}", (v + 0.5).floor() as i64)
    } else if v < 9999.5 {
        format!("${}", tenths(v / 100.0, 'K'))
    } else {
        format!("${}K", (v / 1000.0 + 0.5).floor() as i64)
    }
}

enum Item {
    Chair(usize),
    Desk(usize),
    Agent(usize),
}

impl Default for Scene {
    fn default() -> Self {
        Self::new()
    }
}

fn ellipsize(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        text.to_owned()
    } else {
        format!("{}...", text.chars().take(width.saturating_sub(3)).collect::<String>())
    }
}

/// Word-wrap in character cells; never scroll or cut a multi-byte UTF-8 character.
fn task_pages(text: &str) -> Vec<String> {
    let mut pages = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > TASK_LINE_LEN {
            pages.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        for c in word.chars() {
            if line.chars().count() == TASK_LINE_LEN {
                pages.push(std::mem::take(&mut line));
            }
            line.push(c);
        }
    }
    if !line.is_empty() {
        pages.push(line);
    }
    if pages.is_empty() {
        pages.push("TASK UNAVAILABLE".into());
    }
    pages.chunks(2).map(|lines| lines.join("\n")).collect()
}

impl Scene {
    pub fn new() -> Self {
        let bg =
            a::BG.as_chunks::<3>().0.iter().map(|p| (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32).collect();
        Scene {
            t: 0,
            agents: Vec::new(),
            particles: Vec::new(),
            focus: 0,
            focus_at: 0,
            overflow: 0,
            first: true,
            usage: None,
            sessions: Vec::new(),
            details: Vec::new(),
            pages: HashMap::new(),
            unavailable: ["TASK UNAVAILABLE".into()],
            selected: None,
            task_page: 0,
            bg,
        }
    }

    fn seat_free(&self, seat: usize) -> bool {
        !self.agents.iter().any(|a| a.seat == seat)
    }

    /// Reconcile with the collector's full session list. The first list
    /// seats everyone at once; later newcomers walk in and leavers walk out.
    /// Whoever finds no free seat waits outside.
    pub fn set_sessions(&mut self, list: &[Session]) {
        self.sessions = list.to_vec();
        if self.selected.as_ref().is_some_and(|id| !list.iter().any(|s| &s.id == id)) {
            self.clear_selection();
        }
        let instant = std::mem::replace(&mut self.first, false);
        for a in &mut self.agents {
            a.seen = false;
        }
        let mut overflow = 0;
        for s in list {
            let mut fresh = false;
            let k = match self.agents.iter().position(|a| a.id == s.id) {
                Some(k) => k,
                None => {
                    let Some(seat) = (0..SEATS).find(|&i| self.seat_free(i)) else {
                        overflow += 1;
                        continue;
                    };
                    let look = look_for(&s.id);
                    let mut a = Agent {
                        id: s.id.clone(),
                        name: String::new(),
                        codex: false,
                        state: State::Work,
                        why: Why::None,
                        force: ForcedPose::Auto,
                        pal: look.pal,
                        style: look.style,
                        variant: look.variant,
                        phase: look.phase,
                        seat,
                        mode: Mode::Seated,
                        x: 0.0,
                        y: 0.0,
                        dist: 0.0,
                        dir: Dir::Up,
                        path: Vec::new(),
                        path_i: 0,
                        since: 0,
                        zacc: 0,
                        seen: false,
                        gone: false,
                    };
                    if !instant {
                        a.start_walk(&path_to(seat), Mode::Arriving);
                    }
                    fresh = true;
                    self.agents.push(a);
                    self.agents.len() - 1
                }
            };
            let a = &mut self.agents[k];
            a.seen = true;
            if fresh || a.state != s.state || a.why != s.why {
                a.since = self.t - s.age_s as i64 * 1000;
            }
            a.name = s.name.clone();
            a.codex = s.kind == Kind::Codex;
            a.state = s.state;
            a.why = s.why;
            a.force = s.pose;
        }
        for a in &mut self.agents {
            if a.seen || a.mode == Mode::Leaving {
                continue;
            }
            let full = path_to(a.seat);
            let back: Vec<[f64; 2]> = if a.mode == Mode::Arriving {
                // turn round where they stand
                let k = full.len() - 1 - a.remaining();
                std::iter::once([a.x, a.y]).chain(full[..=k].iter().rev().copied()).collect()
            } else {
                full.iter().rev().copied().collect()
            };
            a.start_walk(&back, Mode::Leaving);
        }
        self.overflow = overflow;
    }

    pub fn set_details(&mut self, details: Vec<SessionDetail>) {
        let old = self.details.iter().find(|d| Some(&d.id) == self.selected.as_ref()).map(|d| &d.task);
        let new = details.iter().find(|d| Some(&d.id) == self.selected.as_ref()).map(|d| &d.task);
        if old != new {
            self.task_page = 0;
        }
        self.pages.retain(|id, _| details.iter().any(|d| &d.id == id));
        for detail in &details {
            if !self.details.iter().any(|old| old.id == detail.id && old.task == detail.task) {
                self.pages.insert(detail.id.clone(), task_pages(&detail.task));
            }
        }
        self.details = details;
    }

    pub fn clear_selection(&mut self) {
        self.selected = None;
        self.task_page = 0;
    }

    pub fn select_next(&mut self) {
        self.select_patron(true);
    }

    pub fn select_patron(&mut self, forward: bool) {
        if self.sessions.is_empty() {
            return;
        }
        let next = self
            .sessions
            .iter()
            .position(|s| Some(&s.id) == self.selected.as_ref())
            .map_or(if forward { 0 } else { self.sessions.len() - 1 }, |i| {
                (i + if forward { 1 } else { self.sessions.len() - 1 }) % self.sessions.len()
            });
        self.selected = Some(self.sessions[next].id.clone());
        self.task_page = 0;
    }

    /// Arrow navigation opens focus first and stops at document boundaries.
    pub fn page_task_key(&mut self, forward: bool) {
        if self.selected.is_none() {
            self.select_next();
            return;
        }
        let last = self.selected_task_pages().len() - 1;
        self.task_page = if forward {
            self.task_page.saturating_add(1).min(last)
        } else {
            self.task_page.saturating_sub(1).min(last)
        };
    }

    pub fn page_task(&mut self, forward: bool) {
        let n = self.selected_task_pages().len();
        self.task_page = if forward { (self.task_page + 1) % n } else { (self.task_page + n - 1) % n };
    }

    fn selected_task_pages(&self) -> &[String] {
        self.selected.as_ref().and_then(|id| self.pages.get(id)).map_or(&self.unavailable, Vec::as_slice)
    }

    pub fn scrollbar_hit(&self, x: i32, y: i32) -> bool {
        self.selected.is_some() && x >= 276 && y >= a::WIN_Y
    }

    pub fn scroll_task_to(&mut self, y: i32) {
        let n = self.selected_task_pages().len();
        let thumb_h = (36 / n as i32).max(8);
        let travel = 36 - thumb_h;
        if travel > 0 {
            let position = (y - a::WIN_Y - 6 - thumb_h / 2).clamp(0, travel);
            self.task_page = ((position * (n - 1) as i32 + travel / 2) / travel) as usize;
        }
    }

    pub fn patron_at(&self, x: i32, y: i32) -> Option<&str> {
        if !(0..320).contains(&x) || !(FLOOR_Y..a::WIN_Y).contains(&y) {
            return None;
        }
        let seat = ((y - FLOOR_Y) / STATION_H * COLS + x / STATION_W) as usize;
        self.agents.iter().find(|ag| ag.seat == seat && ag.seen && ag.mode != Mode::Leaving).map(|ag| ag.id.as_str())
    }

    pub fn session_target(&self, id: &str) -> Option<&SessionTarget> {
        self.details.iter().find(|d| d.id == id).and_then(|d| d.target.as_ref())
    }

    pub fn focus_patron(&mut self, id: String) {
        if self.selected.as_ref() != Some(&id) {
            self.task_page = 0;
        }
        self.selected = Some(id);
    }

    /// Coordinates are in the original 320x240 frame, after removing letterboxing.
    pub fn click(&mut self, x: i32, y: i32) {
        if y >= a::WIN_Y {
            if self.selected.is_none() || x < 276 {
                self.select_next();
            } else {
                self.scroll_task_to(y);
            }
            return;
        }
        if y < FLOOR_Y {
            return;
        }
        let seat = ((y - FLOOR_Y) / STATION_H * COLS + x / STATION_W) as usize;
        if let Some(ag) = self.agents.iter().find(|ag| ag.seat == seat && ag.seen && ag.mode != Mode::Leaving) {
            if self.selected.as_ref() == Some(&ag.id) {
                self.clear_selection();
            } else {
                self.selected = Some(ag.id.clone());
                self.task_page = 0;
            }
        } else {
            self.clear_selection();
        }
    }

    pub fn set_usage(&mut self, usage: Usage) {
        self.usage = Some(usage);
    }

    /// Seated agents waiting for the human, longest wait first.
    fn waiting_list(&self) -> Vec<usize> {
        let mut w: Vec<usize> = (0..self.agents.len())
            .filter(|&i| self.agents[i].mode == Mode::Seated && self.agents[i].state == State::Wait)
            .collect();
        w.sort_by_key(|&i| self.agents[i].since);
        w
    }

    pub fn tick(&mut self, dt: u32) {
        self.t += dt as i64;
        let t = self.t;
        for a in &mut self.agents {
            if a.mode == Mode::Seated {
                continue;
            }
            let mut step = WALK_SPEED * dt as f64 / 1000.0;
            while step > 0.0 && a.remaining() > 0 {
                let [tx, ty] = a.path[a.path_i];
                let (dx, dy) = (tx - a.x, ty - a.y);
                let d = dx.hypot(dy);
                if d > 0.0 {
                    a.dir = if dx.abs() > dy.abs() {
                        if dx > 0.0 { Dir::Right } else { Dir::Left }
                    } else if dy > 0.0 {
                        Dir::Down
                    } else {
                        Dir::Up
                    };
                }
                if d <= step {
                    a.x = tx;
                    a.y = ty;
                    step -= d;
                    a.dist += d;
                    a.path_i += 1;
                } else {
                    a.x += (dx / d) * step;
                    a.y += (dy / d) * step;
                    a.dist += step;
                    step = 0.0;
                }
            }
            if a.remaining() == 0 {
                if a.mode == Mode::Arriving {
                    a.mode = Mode::Seated;
                    a.since = t;
                } else {
                    a.gone = true;
                }
            }
        }
        self.agents.retain(|a| !a.gone);
        for p in &mut self.particles {
            p.age += dt as i64;
        }
        self.particles.retain(|p| p.age < p.life);
        for a in &mut self.agents {
            if a.mode != Mode::Seated {
                continue;
            }
            let pose = pose_of(t, a);
            let g = seat_geom(a.seat);
            a.zacc += dt as i64;
            if pose == Pose::Sleep && a.zacc > 900 {
                a.zacc = 0;
                self.particles.push(Particle::new(PKind::Z, (g.sx + 46) as f64, (g.sy + 12) as f64, 2200));
            }
            if pose == Pose::Sip && a.zacc > 500 {
                a.zacc = 0;
                let up = ((t + a.phase) / 1600) % 2 == 0;
                let mut p = Particle::new(
                    PKind::Steam,
                    (g.sx + SIT_X + if up { 10 } else { 11 }) as f64,
                    (g.sy + SIT_Y + if up { 6 } else { 11 }) as f64,
                    1400,
                );
                p.seat = a.seat;
                self.particles.push(p);
            }
            if pose == Pose::Type && a.zacc > 1300 + a.phase % 700 {
                a.zacc = 0;
                let x = (g.sx as i64 + 33 + (t / 7) % 12) as f64;
                let mut p = Particle::new(PKind::Key, x, (g.sy + LID_Y - 4) as f64, 1100);
                p.glyph = ((t / 13) % 5) as usize;
                p.color = a::LAPTOP[a.codex as usize][3];
                self.particles.push(p);
            }
        }
        let nw = self.waiting_list().len();
        if self.t - self.focus_at > FOCUS_MS {
            self.focus_at = self.t;
            self.focus += 1;
        }
        if self.focus >= nw {
            self.focus = 0;
        }
    }

    /* ---- render ---------------------------------------------------------- */

    fn draw_stars(&self, g: &mut Gfx) {
        const STARS: [(i32, i32); 7] = [(14, 9), (22, 14), (31, 8), (40, 12), (17, 18), (46, 7), (27, 19)];
        for (n, &(x, y)) in STARS.iter().enumerate() {
            let v = (self.t as f64 / 700.0 + (n * 3) as f64) as i32;
            g.px(x, y, if v % 5 == 0 { c::STAR_DIM } else { c::STAR });
        }
    }

    fn draw_sign(&self, g: &mut Gfx) {
        const SIGN: &str = "TOKEN CAFE";
        let k = 2;
        let x0 = (a::W as i32 - text_width(SIGN, k)) >> 1;
        let y0 = 8;
        // one letter flickers now and then
        let cycle = self.t % 9000;
        let dead = if cycle > 7200 && cycle < 7700 && (cycle / 90) % 2 == 0 { 7 } else { usize::MAX };
        for pass in 0..2 {
            for (n, ch) in SIGN.bytes().enumerate() {
                let gl = &a::FONT[(ch - 32) as usize];
                let off = n == dead;
                for (j, bits) in gl.iter().enumerate() {
                    for i in 0..5 {
                        if (bits >> (4 - i)) & 1 == 0 {
                            continue;
                        }
                        let (x, y) = (x0 + (n as i32 * 6 + i) * k, y0 + j as i32 * k);
                        if pass == 0 {
                            if !off {
                                g.rect(x - 1, y - 1, k + 2, k + 2, c::NEON_GLOW);
                            }
                        } else {
                            let col = if off {
                                c::NEON_OFF
                            } else if (i + j as i32) % 4 == 0 {
                                c::NEON_CORE
                            } else {
                                c::NEON
                            };
                            g.rect(x, y, k, k, col);
                        }
                    }
                }
            }
        }
        let sub = "OPEN 24H";
        g.text(sub, (a::W as i32 - text_width(sub, 1)) >> 1, y0 + 18, c::CYAN, 1, Some(c::CYAN_GLOW));
    }

    /// The chalkboard and its labels are in the background; only the values move.
    fn draw_tab(&self, g: &mut Gfx) {
        let (tok, usd) = match self.usage {
            Some(u) => (fmt_tokens(u.tokens), fmt_usd(u.usd)),
            None => ("--".into(), "--".into()),
        };
        g.text(&tok, TAB_X + TAB_W - 3 - text_width(&tok, 1), TAB_Y + 5, c::CHALK_HI, 1, None);
        g.text(&usd, TAB_X + TAB_W - 3 - text_width(&usd, 1), TAB_Y + 14, c::CHALK_GOLD, 1, None);
    }

    fn draw_clock(&self, g: &mut Gfx, cx: i32, cy: i32, now: NaiveTime) {
        for j in -6..=6 {
            for i in -6..=6 {
                let d = i * i + j * j;
                if d <= 36 {
                    g.px(cx + i, cy + j, if d > 25 { c::CLOCK_RIM } else { c::CLOCK_FACE });
                }
            }
        }
        g.px(cx, cy - 4, base::K);
        g.px(cx + 4, cy, base::K);
        g.px(cx, cy + 4, base::K);
        g.px(cx - 4, cy, base::K);
        let (h, m) = (now.hour(), now.minute());
        let hm = ((h % 12) as f64 + m as f64 / 60.0) / 12.0 * PI * 2.0;
        let mm = m as f64 / 60.0 * PI * 2.0;
        for r in 0..=4 {
            g.px(cx + jround(mm.sin() * r as f64), cy - jround(mm.cos() * r as f64), base::K);
        }
        for r in 0..=2 {
            g.px(cx + jround(hm.sin() * r as f64), cy - jround(hm.cos() * r as f64), base::X);
        }
    }

    fn draw_chair(&self, g: &mut Gfx, i: usize) {
        let geo = seat_geom(i);
        let mut p = Pal::base();
        let ch = a::CHAIRS[i % a::CHAIRS.len()];
        p.set(slot::A, ch[0]);
        p.set(slot::a, ch[1]);
        g.blit(&a::CHAIR, geo.sx + CHAIR_X, geo.sy + CHAIR_Y, Some(&p), false);
    }

    fn draw_closed_laptop(&self, g: &mut Gfx, ag: &Agent, x: i32, y: i32) {
        let l = a::LAPTOP[ag.codex as usize];
        g.rect(x, y, 14, 4, base::K);
        g.rect(x + 1, y + 1, 12, 1, l[0]);
        g.rect(x + 1, y + 2, 12, 1, l[1]);
    }

    /// Claude's lid shows its asterisk spinner, Codex's a blinking prompt.
    fn draw_laptop(&self, g: &mut Gfx, ag: &Agent, x: i32, y: i32) {
        let t = self.t;
        let l = a::LAPTOP[ag.codex as usize];
        let mut p = Pal::base();
        p.set(slot::L, l[0]);
        p.set(slot::l, l[1]);
        p.set(slot::A, l[2]);
        p.set(slot::a, l[3]);
        g.blit(&a::LID, x, y, Some(&p), false);
        g.rect(x - 1, y + 11, 15, 2, base::K);
        g.rect(x, y + 11, 13, 1, l[1]);
        let logo = if ag.codex {
            &a::PROMPT[if ag.state == State::Work && (t / 450) % 2 == 0 { 0 } else { 1 }]
        } else if ag.state == State::Work {
            const SEQ: [usize; 6] = [0, 1, 2, 3, 2, 1];
            &a::SPARK[SEQ[((t / 130) % 6) as usize]]
        } else {
            &a::SPARK[3]
        };
        if ag.state == State::Wait && (t / 300) % 2 != 0 {
            p.set(slot::A, base::Y);
            p.set(slot::a, base::W);
        }
        g.blit(logo, x + 3, y + 2, Some(&p), false);
    }

    fn draw_plate(&self, g: &mut Gfx, ag: Option<&Agent>, cx: i32, y: i32) {
        let Some(ag) = ag else {
            // an empty seat keeps a blank brass plate
            g.rect(cx - 12, y + 2, 24, 7, base::K);
            g.rect(cx - 11, y + 3, 22, 5, c::DESK_FRONT_LO);
            return;
        };
        let name = shorten(&ag.name, PLATE_LEN).to_ascii_uppercase();
        let w = text_width(&name, 1) + 6;
        let x = cx - (w >> 1);
        let blink = ag.state == State::Wait && (self.t / 400) % 2 == 0;
        g.rect(x, y, w, 11, base::K);
        let face = if blink {
            base::Y
        } else if ag.codex {
            c::PLATE_CODEX
        } else {
            c::PLATE_CLAUDE
        };
        g.rect(x + 1, y + 1, w - 2, 9, face);
        g.rect(x + 1, y + 9, w - 2, 1, c::PLATE_LO);
        g.text(&name, x + 3, y + 2, if ag.state == State::Idle { a::IDLE_TEXT } else { base::K }, 1, None);
    }

    fn draw_desk(&self, g: &mut Gfx, i: usize) {
        let geo = seat_geom(i);
        let (x, y, w) = (geo.sx + DESK_X, geo.sy + DESK_Y, DESK_W);
        g.darken(x + 2, y + 21, w - 2, 3, 0.55);
        g.rect(x, y, w, 9, base::K);
        g.rect(x + 1, y + 1, w - 2, 7, c::DESK_TOP);
        g.rect(x + 1, y + 1, w - 2, 1, c::DESK_HI);
        for n in 0..3 {
            g.rect(x + 7 + n * 16, y + 3 + (n & 1) * 2, 8, 1, c::DESK_GRAIN);
        }
        g.rect(x + 1, y + 7, w - 2, 1, c::DESK_EDGE);
        g.rect(x, y + 8, w, 13, base::K);
        g.rect(x + 1, y + 8, w - 2, 12, c::DESK_FRONT);
        g.rect(x + 1, y + 18, w - 2, 2, c::DESK_FRONT_LO);
        for &(s, px, py) in seat_props(i) {
            g.blit(s, geo.sx + px, geo.sy + py, None, false);
        }
        let ag = self.agents.iter().find(|a| a.seat == i);
        if let Some(ag) = ag.filter(|a| a.mode == Mode::Seated) {
            if pose_of(self.t, ag) == Pose::Sleep {
                self.draw_closed_laptop(g, ag, geo.sx + LID_X - 1, geo.sy + DESK_Y + 2);
            } else {
                self.draw_laptop(g, ag, geo.sx + LID_X, geo.sy + LID_Y);
            }
        }
        self.draw_plate(g, ag, geo.sx + 40, y + 9);
    }

    fn draw_seated(&self, g: &mut Gfx, ag: &Agent) {
        let geo = seat_geom(ag.seat);
        let p = &a::POSES[ag.style];
        let t = self.t + ag.phase;
        let spr = match pose_of(self.t, ag) {
            Pose::Type => &p.typing[((t / 170) % 2) as usize],
            Pose::Think => &p.think,
            Pose::Raise => &p.raise[((t / 320) % 2) as usize],
            Pose::Stretch => &p.stretch,
            Pose::Sip => &p.sip[((t / 1600) % 2) as usize],
            Pose::Sleep => &p.sleep[((t / 1100) % 2) as usize],
        };
        g.blit(spr, geo.sx + SIT_X - BX, geo.sy + SIT_Y - BY, Some(&ag.pal), false);
    }

    fn draw_walker(&self, g: &mut Gfx, ag: &Agent) {
        let p = &a::POSES[ag.style];
        let f = (ag.dist / 7.0) as i32 % 4;
        let mut flip = false;
        let spr = match ag.dir {
            Dir::Up => {
                if f % 2 == 0 {
                    &p.stand_u
                } else {
                    &p.walk_u[if f == 1 { 0 } else { 1 }]
                }
            }
            Dir::Down => {
                if f % 2 == 0 {
                    &p.stand_d
                } else {
                    &p.walk_d[if f == 1 { 0 } else { 1 }]
                }
            }
            Dir::Left | Dir::Right => {
                flip = ag.dir == Dir::Left;
                if f % 2 == 0 { &p.stand_s } else { &p.walk_s[0] }
            }
        };
        let (x, y) = (jround(ag.x) - 8, jround(ag.y) - 24);
        g.blit(spr, x - BX, y - BY + if f % 2 != 0 { -1 } else { 0 }, Some(&ag.pal), flip);
    }

    fn draw_overlays(&self, g: &mut Gfx) {
        let t = self.t;
        let wl = self.waiting_list();
        let focused = if let Some(id) = &self.selected {
            self.agents.iter().position(|ag| &ag.id == id && ag.seen)
        } else {
            wl.get(self.focus).copied()
        };
        for (k, ag) in self.agents.iter().enumerate() {
            if ag.mode != Mode::Seated {
                continue;
            }
            let geo = seat_geom(ag.seat);
            match pose_of(t, ag) {
                Pose::Raise => {
                    let bob = ((t / 260) % 2) as i32;
                    let b = if ag.why == Why::Ask { &a::BUBBLE_ASK } else { &a::BUBBLE_BANG };
                    g.blit(b, geo.sx + 29, geo.sy + SIT_Y - 14 - bob, None, false);
                }
                Pose::Think => g.blit(&a::BUBBLE_THINK, geo.sx + 45, geo.sy + SIT_Y - 11, None, false),
                _ => {}
            }
            if focused == Some(k) {
                g.blit(&a::CURSOR, geo.sx + 16 + ((t / 220) % 2) as i32, geo.sy + SIT_Y + 6, None, false);
            }
        }
        for p in &self.particles {
            let k = p.age as f64 / p.life as f64;
            match p.kind {
                PKind::Z => {
                    let x = jround(p.x + k * 10.0 + (k * 6.0).sin() * 2.0);
                    let y = jround(p.y - k * 20.0);
                    g.blit(if k < 0.45 { &a::ZSMALL } else { &a::ZBIG }, x, y, None, false);
                }
                PKind::Steam => {
                    let x = jround(p.x + (k * 7.0 + p.seat as f64).sin() * 1.5);
                    let y = jround(p.y - k * 9.0);
                    g.px(x, y, if k < 0.5 { base::W } else { base::w });
                }
                PKind::Key => {
                    let mut pal = Pal::base();
                    pal.set(slot::A, p.color);
                    g.blit(&a::KEYS[p.glyph], jround(p.x), jround(p.y - k * 10.0), Some(&pal), false);
                }
            }
        }
    }

    /// Shared portrait and text layout for automatic alerts and manual focus.
    fn draw_dialogue(&self, g: &mut Gfx, id: &str, state: State, title: &str, text: &str) {
        let (fx, fy, fs) = (8, a::WIN_Y + 7, 34);
        let agent = self.agents.iter().find(|ag| ag.id == id && ag.seen);
        let look = look_for(id);
        let pal = agent.map_or(&look.pal, |ag| &ag.pal);
        let chair = agent.map_or(0, |ag| ag.seat % a::CHAIRS.len());
        g.rect(fx, fy, fs, fs, c::WIN_BORDER_LO);
        g.rect(fx + 1, fy + 1, fs - 2, fs - 2, c::WIN1);
        g.rect(fx + 1, fy + 19, fs - 2, fs - 20, a::CHAIRS[chair][0]);
        let head = match state {
            State::Wait => &a::HEAD_SHOUT,
            State::Idle => &a::HEAD_CLOSED,
            State::Work => &a::HEAD_OPEN,
        };
        g.blit_scaled(head, fx + 3, fy + 5, Some(pal), 2, Some([fx + 1, fy + 1, fx + fs - 1, fy + fs - 1]));
        g.text(&ellipsize(title, 40), 50, a::WIN_Y + 4, c::GOLD, 1, Some(c::TEXT_SHADOW));
        for (row, line) in text.lines().take(2).enumerate() {
            g.text(line, 50, a::WIN_Y + 14 + row as i32 * 16, c::TEXT, 2, Some(c::TEXT_SHADOW));
        }
    }

    /// The message window: whoever waits for the human (portrait, name and
    /// what they want, cycling through them), else the party status.
    fn draw_message(&self, g: &mut Gfx, now: NaiveTime) {
        let t = self.t;
        let (x, y, w, h) = (0, a::WIN_Y, a::W as i32, a::H as i32 - a::WIN_Y);
        let wl = self.waiting_list();
        if let Some(&k) = wl.get(self.focus).filter(|_| self.selected.is_none()) {
            let ag = &self.agents[k];
            let says = if ag.why == Why::Ask { "HAS A QUESTION" } else { "NEEDS YOUR OK!" };
            self.draw_dialogue(g, &ag.id, ag.state, &ellipsize(&ag.name, 33), says);
            if wl.len() > 1 {
                let n = format!("{}/{}", self.focus + 1, wl.len());
                g.text(&n, x + w - 10 - text_width(&n, 1), y + 9, c::TEXT_DIM, 1, Some(c::TEXT_SHADOW));
            }
            if (t / 400) % 2 == 0 {
                let (ax, ay) = (x + w - 16, y + h - 13);
                for j in 0..4 {
                    g.rect(ax + j, ay + j, 7 - 2 * j, 1, c::TEXT);
                }
            }
            return;
        }
        if let Some(session) = self.sessions.iter().find(|s| Some(&s.id) == self.selected.as_ref()) {
            let pages = self.selected_task_pages();
            let page = self.task_page % pages.len();
            let prefix = if session.state == State::Idle { "LAST" } else { "TASK" };
            let name = if wl.is_empty() {
                format!("{prefix} {}", session.name)
            } else {
                format!("!{} WAIT {prefix} {}", wl.len(), session.name)
            };
            self.draw_dialogue(g, &session.id, session.state, &name, &pages[page]);
            // Vertical scrollbar; the surrounding 44x48 area is its hit target.
            let track_y = y + 6;
            let track_h = 36;
            let thumb_h = (track_h / pages.len() as i32).max(8);
            let travel = track_h - thumb_h;
            let offset = if pages.len() > 1 { travel * page as i32 / (pages.len() - 1) as i32 } else { 0 };
            let sx = x + w - 14;
            // Recessed blue rail tucked inside the panel's right border.
            g.rect(sx - 1, track_y - 1, 10, track_h + 2, 0x10205b);
            g.rect(sx, track_y, 8, track_h, 0x1a3280);
            g.rect(sx + 8, track_y, 1, track_h, 0x687fc1);
            g.rect(sx, track_y + track_h, 9, 1, 0x687fc1);
            // Raised thumb with a bright upper edge and paired grip marks.
            let ty = track_y + offset;
            g.rect(sx, ty, 8, thumb_h, 0x536cab);
            g.rect(sx, ty, 8, 1, 0xe0e8ff);
            g.rect(sx, ty + 1, 1, thumb_h - 2, 0xc0cff5);
            g.rect(sx + 1, ty + 1, 6, thumb_h - 3, 0xa1b6e8);
            g.rect(sx + 2, ty + thumb_h / 2 - 1, 4, 1, 0x536cab);
            g.rect(sx + 2, ty + thumb_h / 2 + 1, 4, 1, 0x536cab);
            return;
        }
        // nobody waiting: party status
        let (mut working, mut idle, mut seated) = (0, 0, 0);
        for ag in self.agents.iter().filter(|a| a.mode == Mode::Seated) {
            seated += 1;
            working += (ag.state == State::Work) as usize;
            idle += (ag.state == State::Idle) as usize;
        }
        let mut cx = x + 12;
        let mut lp = Pal::base();
        lp.set(slot::A, a::ICON_LAPTOP_A);
        lp.set(slot::a, a::ICON_LAPTOP_a);
        for (icon, pal, count) in [(&a::ICON_LAPTOP, Some(&lp), working), (&a::ICON_CUP, None, idle)] {
            g.blit_scaled(icon, cx, y + 9, pal, 2, None);
            cx += icon.w as i32 * 2 + 4;
            let s = count.to_string();
            g.text(&s, cx, y + 10, c::TEXT, 2, Some(c::TEXT_SHADOW));
            cx += text_width(&s, 2) + 14;
        }
        let colon = if (t / 500) % 2 != 0 { ':' } else { ' ' };
        let clock = format!("{:02}{colon}{:02}", now.hour(), now.minute());
        g.text(&clock, x + w - 12 - text_width(&clock, 2), y + 10, c::GOLD, 2, Some(c::TEXT_SHADOW));
        let line = if self.overflow > 0 {
            format!("+{} OUTSIDE. TAP HERE FOR DETAILS", self.overflow)
        } else if seated == 0 {
            "THE CAFE IS EMPTY. NO AGENTS ONLINE.".into()
        } else {
            "TAP A PATRON OR HERE FOR DETAILS".into()
        };
        g.text(&line, x + 12, y + 31, c::TEXT_DIM, 1, Some(c::TEXT_SHADOW));
    }

    /// Draw the café as of the last tick, with the clocks at local time `now`.
    pub fn render(&self, fb: &mut [u32], now: NaiveTime) {
        fb.copy_from_slice(&self.bg);
        let mut g = Gfx::new(fb);
        // the room's window is part of the background, so the scene stays above it
        g.clip(0, 0, a::W as i32, a::WIN_Y);
        self.draw_stars(&mut g);
        self.draw_sign(&mut g);
        self.draw_clock(&mut g, 86, 17, now);
        self.draw_tab(&mut g);
        // depth-sorted: chairs, desks and patrons by their feet
        let mut items: Vec<(f64, usize, Item)> = Vec::with_capacity(SEATS * 2 + self.agents.len());
        for i in 0..SEATS {
            let sy = seat_geom(i).sy as f64;
            items.push((sy + 27.0, 2 * i, Item::Chair(i)));
            items.push((sy + 51.0, 2 * i + 1, Item::Desk(i)));
        }
        for (k, ag) in self.agents.iter().enumerate() {
            let d = if ag.mode == Mode::Seated { (seat_geom(ag.seat).sy + 28) as f64 } else { ag.y };
            items.push((d, 100 + k, Item::Agent(k)));
        }
        items.sort_by(|p, q| p.0.total_cmp(&q.0).then(p.1.cmp(&q.1)));
        for (_, _, item) in &items {
            match *item {
                Item::Chair(i) => self.draw_chair(&mut g, i),
                Item::Desk(i) => self.draw_desk(&mut g, i),
                Item::Agent(k) => {
                    let ag = &self.agents[k];
                    if ag.mode == Mode::Seated { self.draw_seated(&mut g, ag) } else { self.draw_walker(&mut g, ag) }
                }
            }
        }
        self.draw_overlays(&mut g);
        g.noclip();
        self.draw_message(&mut g, now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, state: State) -> Session {
        Session {
            id: id.into(),
            name: format!("Moon {id}"),
            kind: Kind::Codex,
            state,
            why: Why::None,
            pose: ForcedPose::Auto,
            age_s: 0,
        }
    }

    #[test]
    fn navigation_targets_validate_ids_and_exclude_empty_seats() {
        assert_eq!(SessionTarget::Codex("abc-123".into()).url().as_deref(), Some("codex://threads/abc-123"));
        for id in ["", "abc?x=y", "../x", "a b"] {
            assert!(SessionTarget::Codex(id.into()).url().is_none());
        }
        assert!(SessionTarget::Claude("cli-uuid".into()).url().is_none());
        let mut scene = Scene::new();
        scene.set_sessions(&[session("a", State::Work)]);
        assert_eq!(scene.patron_at(40, 80), Some("a"));
        assert_eq!(scene.patron_at(140, 80), None);
        assert_eq!(scene.patron_at(40, 220), None);
        scene.click(40, 80);
        scene.focus_patron("a".into());
        assert_eq!(scene.selected.as_deref(), Some("a"));
        scene.click(40, 80);
        assert!(scene.selected.is_none());
        scene.set_sessions(&[]);
        assert_eq!(scene.patron_at(40, 80), None);
    }

    #[test]
    fn cached_pages_refresh_evict_and_clamp_scroll_boundaries() {
        let mut scene = Scene::new();
        scene.set_sessions(&[session("a", State::Work)]);
        let details = vec![SessionDetail {
            target: None,
            id: "a".into(),
            task: "one two three four five six seven eight nine ten eleven twelve thirteen fourteen".into(),
        }];
        scene.set_details(details.clone());
        scene.select_next();
        scene.scroll_task_to(999);
        assert_eq!(scene.task_page, scene.selected_task_pages().len() - 1);
        let allocation = scene.pages["a"].as_ptr();
        scene.set_details(details);
        assert_eq!(scene.pages["a"].as_ptr(), allocation);
        scene.scroll_task_to(-999);
        assert_eq!(scene.task_page, 0);
        scene.set_details(vec![SessionDetail { target: None, id: "a".into(), task: "Short".into() }]);
        scene.scroll_task_to(999);
        assert_eq!(scene.task_page, 0);
        assert_eq!(scene.selected_task_pages(), ["Short"]);
        scene.set_details(vec![]);
        assert!(scene.pages.is_empty());
        assert_eq!(scene.selected_task_pages(), ["TASK UNAVAILABLE"]);
        scene.set_sessions(&[]);
        assert!(scene.selected.is_none());
        let mut frame = vec![0; a::W * a::H];
        scene.render(&mut frame, NaiveTime::from_hms_opt(12, 0, 0).unwrap());
    }

    #[test]
    fn touch_targets_open_cycle_page_and_toggle_focus() {
        let mut scene = Scene::new();
        scene.set_sessions(&(0..9).map(|i| session(&i.to_string(), State::Work)).collect::<Vec<_>>());
        scene.click(140, 220);
        assert_eq!(scene.selected.as_deref(), Some("0"));
        scene.click(40, 80);
        assert!(scene.selected.is_none());
        scene.click(40, 80);
        assert_eq!(scene.selected.as_deref(), Some("0"));
        scene.set_details(vec![SessionDetail {
            target: None,
            id: "0".into(),
            task: "A long request that spans several pages of readable text".into(),
        }]);
        scene.click(290, 239);
        assert_eq!(scene.task_page, scene.selected_task_pages().len() - 1);
        for _ in 0..8 {
            scene.click(140, 220);
        }
        assert_eq!(scene.selected.as_deref(), Some("8")); // touch can reach overflow
        scene.click(140, 220);
        assert_eq!(scene.selected.as_deref(), Some("0"));
        scene.set_sessions(&[session("0", State::Work)]);
        scene.click(280, 170);
        assert!(scene.selected.is_none()); // empty desk clears
    }

    #[test]
    fn up_down_open_focus_clamp_pages_and_preserve_selected_patron() {
        let mut scene = Scene::new();
        scene.page_task_key(true);
        assert!(scene.selected.is_none());
        scene.set_sessions(&[session("a", State::Work), session("b", State::Idle)]);
        scene.set_details(vec![SessionDetail {
            target: None,
            id: "a".into(),
            task: "One two three four five six seven eight nine ten eleven twelve".into(),
        }]);
        scene.page_task_key(false);
        assert_eq!(scene.selected.as_deref(), Some("a"));
        assert_eq!(scene.task_page, 0);
        scene.page_task_key(false);
        assert_eq!(scene.task_page, 0);
        let last = scene.selected_task_pages().len() - 1;
        for _ in 0..20 {
            scene.page_task_key(true);
        }
        assert_eq!(scene.task_page, last);
        assert_eq!(scene.selected.as_deref(), Some("a"));
        for _ in 0..20 {
            scene.page_task_key(false);
        }
        assert_eq!(scene.task_page, 0);
        scene.select_next();
        scene.page_task_key(true);
        scene.page_task_key(false);
        assert_eq!(scene.selected.as_deref(), Some("b"));
        assert_eq!(scene.task_page, 0);
        scene.clear_selection();
        scene.page_task_key(true);
        assert_eq!(scene.task_page, 0);
        scene.set_details(vec![SessionDetail { target: None, id: "a".into(), task: "Short".into() }]);
        scene.page_task_key(true);
        assert_eq!(scene.task_page, 0);
    }

    #[test]
    fn banner_and_arrows_keep_highlight_and_footer_on_same_identity() {
        let mut scene = Scene::new();
        scene.set_sessions(&[session("a", State::Work), session("b", State::Work), session("c", State::Idle)]);
        scene.set_details(vec![SessionDetail {
            target: None,
            id: "b".into(),
            task: "Unique task for patron B".into(),
        }]);
        scene.click(100, 220); // open A
        scene.click(100, 220); // next B
        assert_eq!(scene.selected.as_deref(), Some("b"));
        assert_eq!(scene.agents.iter().find(|a| Some(&a.id) == scene.selected.as_ref()).unwrap().seat, 1);
        assert_eq!(scene.selected_task_pages()[0], "Unique task for\npatron B");
        scene.select_patron(false);
        assert_eq!(scene.selected.as_deref(), Some("a"));
        scene.select_patron(false);
        assert_eq!(scene.selected.as_deref(), Some("c"));
        scene.select_patron(true);
        assert_eq!(scene.selected.as_deref(), Some("a"));
        scene.select_patron(true);
        assert_eq!(scene.selected.as_deref(), Some("b"));
        scene.click(290, 239);
        assert_eq!(scene.selected.as_deref(), Some("b"));
        assert_eq!(scene.task_page, scene.selected_task_pages().len() - 1); // scrollbar changes text only
    }

    #[test]
    fn selection_tracks_identity_and_updates_without_resetting_pages() {
        let mut scene = Scene::new();
        scene.set_sessions(&[session("a", State::Work), session("b", State::Idle)]);
        scene.set_details(vec![SessionDetail {
            target: None,
            id: "b".into(),
            task: "A sufficiently long request to require multiple readable pages".into(),
        }]);
        scene.click(120, 80);
        assert_eq!(scene.selected.as_deref(), Some("b"));
        scene.page_task(true);
        assert_eq!(scene.task_page, 1);
        scene.set_sessions(&[session("b", State::Work), session("a", State::Idle)]);
        scene.set_details(scene.details.clone());
        assert_eq!(scene.selected.as_deref(), Some("b"));
        assert_eq!(scene.task_page, 1);
        scene.set_details(vec![SessionDetail { target: None, id: "b".into(), task: "New request".into() }]);
        assert_eq!(scene.task_page, 0);
        scene.set_sessions(&[session("a", State::Work)]);
        assert!(scene.selected.is_none());
    }

    #[test]
    fn keyboard_selection_includes_overflow_and_page_wrap_handles_unicode() {
        let mut scene = Scene::new();
        scene.set_sessions(&(0..10).map(|i| session(&i.to_string(), State::Work)).collect::<Vec<_>>());
        for _ in 0..10 {
            scene.select_next();
        }
        assert_eq!(scene.selected.as_deref(), Some("9"));
        scene.select_next();
        assert_eq!(scene.selected.as_deref(), Some("0"));
        let pages = task_pages(&format!("{} other words", "é".repeat(49)));
        assert!(
            pages.iter().all(|p| p.lines().count() <= 2 && p.lines().all(|line| line.chars().count() <= TASK_LINE_LEN))
        );
        assert_eq!(pages.concat().chars().filter(|c| *c == 'é').count(), 49);
        scene.clear_selection();
        scene.page_task(false);
        assert_eq!(scene.task_page, 0);
    }

    #[test]
    fn waiting_alert_remains_available_while_manual_focus_is_usable() {
        let mut scene = Scene::new();
        scene.set_sessions(&[session("a", State::Wait), session("b", State::Work)]);
        let mut before = vec![0; a::W * a::H];
        let mut selected = before.clone();
        let now = NaiveTime::from_hms_opt(12, 0, 0).unwrap();
        scene.render(&mut before, now);
        scene.click(120, 80);
        scene.render(&mut selected, now);
        assert_ne!(&before[a::WIN_Y as usize * a::W..], &selected[a::WIN_Y as usize * a::W..]);
        scene.click(120, 80);
        scene.render(&mut selected, now);
        assert_eq!(&before[a::WIN_Y as usize * a::W..], &selected[a::WIN_Y as usize * a::W..]);
        scene.click(120, 80);
        scene.set_sessions(&[session("a", State::Idle), session("b", State::Work)]);
        scene.render(&mut selected, now);
        assert_ne!(&before[a::WIN_Y as usize * a::W..], &selected[a::WIN_Y as usize * a::W..]);
        assert_eq!(scene.selected.as_deref(), Some("b"));
    }

    #[test]
    fn shorten_keeps_the_suffix() {
        assert_eq!(shorten("esp-ee", 8), "esp-ee");
        assert_eq!(shorten("mithrandir-1c", 8), "mithr-1c");
        assert_eq!(shorten("checkout-flow-1c", 12), "checkout-1c"); // no "checkout--1c"
        assert_eq!(shorten("observability", 12), "observabilit");
    }
}
