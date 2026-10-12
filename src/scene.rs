//! The café: seats, patrons, walking, poses, particles and the message
//! window, behaving as on the Token Café design page (design/cafe.js). The
//! collector only says which sessions exist and what state each is in;
//! everything that moves is decided here.

use std::f64::consts::PI;

use chrono::{NaiveTime, Timelike};

use crate::assets::{self as a, Pal, Sprite, base, c, slot};
use crate::gfx::{Gfx, text_width};

pub const SEATS: usize = 8;
const NAME_LEN: usize = 12; // shown in the message window
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
        let focused = wl.get(self.focus).copied();
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

    /// The message window: whoever waits for the human (portrait, name and
    /// what they want, cycling through them), else the party status.
    fn draw_message(&self, g: &mut Gfx, now: NaiveTime) {
        let t = self.t;
        let (x, y, w, h) = (0, a::WIN_Y, a::W as i32, a::H as i32 - a::WIN_Y);
        let wl = self.waiting_list();
        if let Some(&k) = wl.get(self.focus) {
            let ag = &self.agents[k];
            // portrait: the agent's head at 2x in an inset frame
            let (fx, fy, fs) = (x + 8, y + 7, 34);
            g.rect(fx, fy, fs, fs, c::WIN_BORDER_LO);
            g.rect(fx + 1, fy + 1, fs - 2, fs - 2, c::WIN1);
            g.rect(fx + 1, fy + 1 + 18, fs - 2, fs - 20, a::CHAIRS[ag.seat % a::CHAIRS.len()][0]);
            let bx = [fx + 1, fy + 1, fx + fs - 1, fy + fs - 1];
            g.blit_scaled(&a::HEAD_SHOUT, fx + 3, fy + 5, Some(&ag.pal), 2, Some(bx));
            let tx = fx + fs + 8;
            let name = shorten(&ag.name, NAME_LEN).to_ascii_uppercase();
            g.text(&name, tx, y + 9, c::GOLD, 2, Some(c::TEXT_SHADOW));
            let says = if ag.why == Why::Ask { "HAS A QUESTION" } else { "NEEDS YOUR OK!" };
            g.text(says, tx, y + 27, c::TEXT, 2, Some(c::TEXT_SHADOW));
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
        let mut line = if seated == 0 {
            "THE CAFE IS EMPTY. NO AGENTS ONLINE.".to_string()
        } else if idle == 0 && working == 1 {
            "ONE AGENT IS HARD AT WORK.".to_string()
        } else if idle == 0 {
            format!("ALL {working} AGENTS ARE HARD AT WORK.")
        } else if working == 0 {
            "EVERYONE IS ON A BREAK. YOUR MOVE.".to_string()
        } else {
            format!("{idle} ON A BREAK, {working} AT WORK.")
        };
        if self.overflow > 0 {
            line = format!("+{} WAITING OUTSIDE. {line}", self.overflow);
        }
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
    use super::shorten;

    #[test]
    fn shorten_keeps_the_suffix() {
        assert_eq!(shorten("esp-ee", 8), "esp-ee");
        assert_eq!(shorten("mithrandir-1c", 8), "mithr-1c");
        assert_eq!(shorten("checkout-flow-1c", 12), "checkout-1c"); // no "checkout--1c"
        assert_eq!(shorten("observability", 12), "observabilit");
    }
}
