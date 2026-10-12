//! Renders the README's screenshots from the real scene code:
//!   cargo run --release --example screenshots
//! writes docs/cafe.png, docs/night.png and docs/arrivals.gif.

use std::borrow::Cow;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;

use chrono::NaiveTime;
use token_cafe::assets::{H, W};
use token_cafe::scene::{ForcedPose, Kind, Scene, Session, State, Usage, Why};

const STEP: u32 = 20; // ms per scene tick

fn s(id: &str, name: &str, kind: Kind, state: State, why: Why, pose: ForcedPose, age_s: u32) -> Session {
    Session { id: id.into(), name: name.into(), kind, state, why, pose, age_s }
}

fn run(scene: &mut Scene, ms: u32) {
    for _ in 0..ms / STEP {
        scene.tick(STEP);
    }
}

/// The frame as 8-bit RGB, k times enlarged.
fn rgb(scene: &Scene, now: NaiveTime, k: usize) -> Vec<u8> {
    let mut fb = vec![0u32; W * H];
    scene.render(&mut fb, now);
    let mut out = Vec::with_capacity(W * H * k * k * 3);
    for y in 0..H * k {
        for x in 0..W * k {
            let p = fb[(y / k) * W + x / k];
            out.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, p as u8]);
        }
    }
    out
}

fn png(path: &str, data: &[u8], k: usize) {
    let mut enc = png::Encoder::new(BufWriter::new(File::create(path).unwrap()), (W * k) as u32, (H * k) as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header().unwrap().write_image_data(data).unwrap();
    println!("{path}");
}

/// Pixel art has few colours: one exact global palette for every frame.
/// After the first, each frame carries only the box that changed, with
/// unchanged pixels transparent, so the file stays small.
fn gif(path: &str, frames: &[Vec<u8>], k: usize, delay_cs: u16) {
    const CLEAR: u8 = 0;
    let mut index: HashMap<[u8; 3], u8> = HashMap::new();
    let mut palette = vec![0, 0, 0]; // CLEAR
    let indexed: Vec<Vec<u8>> = frames
        .iter()
        .map(|f| {
            f.as_chunks::<3>()
                .0
                .iter()
                .map(|c| {
                    let key = [c[0], c[1], c[2]];
                    *index.entry(key).or_insert_with(|| {
                        palette.extend_from_slice(&key);
                        u8::try_from(palette.len() / 3 - 1).expect("more than 256 colours")
                    })
                })
                .collect()
        })
        .collect();
    let (w, h) = (W * k, H * k);
    let mut enc = gif::Encoder::new(BufWriter::new(File::create(path).unwrap()), w as u16, h as u16, &palette).unwrap();
    enc.set_repeat(gif::Repeat::Infinite).unwrap();
    for (n, buf) in indexed.iter().enumerate() {
        let (mut x0, mut y0, mut x1, mut y1) = (0, 0, w, h);
        let mut part: Cow<[u8]> = Cow::Borrowed(buf);
        let mut transparent = None;
        if n > 0 {
            let prev = &indexed[n - 1];
            let changed: Vec<usize> = (0..w * h).filter(|&i| buf[i] != prev[i]).collect();
            if changed.is_empty() {
                (x0, y0, x1, y1) = (0, 0, 1, 1);
            } else {
                x0 = changed.iter().map(|i| i % w).min().unwrap();
                x1 = changed.iter().map(|i| i % w).max().unwrap() + 1;
                (y0, y1) = (changed[0] / w, changed[changed.len() - 1] / w + 1);
            }
            let mut sub = Vec::with_capacity((x1 - x0) * (y1 - y0));
            for y in y0..y1 {
                for x in x0..x1 {
                    let i = y * w + x;
                    sub.push(if buf[i] == prev[i] { CLEAR } else { buf[i] });
                }
            }
            part = Cow::Owned(sub);
            transparent = Some(CLEAR);
        }
        let frame = gif::Frame {
            left: x0 as u16,
            top: y0 as u16,
            width: (x1 - x0) as u16,
            height: (y1 - y0) as u16,
            delay: delay_cs,
            dispose: gif::DisposalMethod::Keep,
            transparent,
            buffer: part,
            ..Default::default()
        };
        enc.write_frame(&frame).unwrap();
    }
    println!("{path} ({} frames, {} colours)", frames.len(), palette.len() / 3);
}

fn main() {
    use {ForcedPose as P, Kind::*, State::*};
    std::fs::create_dir_all("docs").unwrap();
    let late = NaiveTime::from_hms_opt(21, 47, 0).unwrap();
    let usage = Usage { tokens: 1_624_000_000.0, usd: 693.70 };

    // Every pose at once, someone waiting on a permission prompt.
    let mut cafe = Scene::new();
    cafe.set_usage(usage);
    cafe.set_sessions(&[
        s("b07e11a2", "api-server", Claude, Work, Why::None, P::Auto, 0),
        s("24eb1748", "checkout-flow-1c", Claude, Wait, Why::Perm, P::Auto, 40),
        s("516264dc", "docs-8a", Claude, Work, Why::None, P::Think, 0),
        s("1a342e2a", "migrate-34", Claude, Idle, Why::None, P::Sleep, 0),
        s("c0dex9f1", "infra", Codex, Wait, Why::Ask, P::Auto, 10),
        s("01a10a65", "web-app", Codex, Idle, Why::None, P::Sip, 0),
        s("9a7c33d0", "billing", Claude, Idle, Why::None, P::Stretch, 0),
        s("01a11478", "ml-pipeline-6e", Codex, Work, Why::None, P::Auto, 0),
    ]);
    run(&mut cafe, 2600);
    png("docs/cafe.png", &rgb(&cafe, late, 3), 3);

    // A quiet night: the poses come from how long each has been idle.
    let mut night = Scene::new();
    night.set_usage(Usage { tokens: 5_250_000.0, usd: 41.2 });
    night.set_sessions(&[
        s("7e0c11aa", "api-server", Claude, Work, Why::None, P::Auto, 0),
        s("3fa0b2c4", "notes", Claude, Idle, Why::None, P::Auto, 1500),
        s("88d1e0f7", "web-app", Codex, Idle, Why::None, P::Auto, 300),
        s("51c2a9d3", "scratch", Claude, Idle, Why::None, P::Auto, 2400),
    ]);
    run(&mut night, 6500); // the clock colon is lit
    png("docs/night.png", &rgb(&night, NaiveTime::from_hms_opt(3, 12, 0).unwrap(), 3), 3);

    // Newcomers walk in, someone who finished walks out.
    let mut walk = Scene::new();
    walk.set_usage(usage);
    let regulars = [
        s("b07e11a2", "api-server", Claude, Work, Why::None, P::Auto, 0),
        s("c0dex9f1", "infra", Codex, Idle, Why::None, P::Sip, 0),
    ];
    let leaver = s("1a342e2a", "hotfix-34", Claude, Idle, Why::None, P::Stretch, 0);
    walk.set_sessions(&[regulars[0].clone(), regulars[1].clone(), leaver]);
    run(&mut walk, 1000);
    let mut frames = Vec::new();
    for _ in 0..8 {
        run(&mut walk, 80);
        frames.push(rgb(&walk, late, 2));
    }
    walk.set_sessions(&[
        regulars[0].clone(),
        regulars[1].clone(),
        s("e5a7f00d", "search-ui", Claude, Work, Why::None, P::Auto, 0),
        s("9b3c4d21", "payments-2b", Codex, Work, Why::None, P::Auto, 0),
        s("4f6e8a10", "onboarding", Claude, Wait, Why::Ask, P::Auto, 0),
    ]);
    for _ in 0..95 {
        run(&mut walk, 80);
        frames.push(rgb(&walk, late, 2));
    }
    gif("docs/arrivals.gif", &frames, 2, 8);
}
