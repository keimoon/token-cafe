//! The window: ticks the scene in fixed 33 ms steps, renders the
//! 320x240 frame and shows it at the largest whole-number scale that fits,
//! letterboxed, so the pixel art stays crisp at any size or DPI. Only the
//! part of the window that changed since the buffer was last shown is
//! repainted and reported to the compositor: a quiet café costs little.

use std::collections::VecDeque;
use std::error::Error;
use std::num::NonZeroU32;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use softbuffer::{Context, Rect, Surface};
use token_cafe::assets::{H, W};
use token_cafe::scene::Scene;
use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, KeyEvent, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, NamedKey};
#[cfg(target_os = "linux")]
use winit::platform::wayland::WindowAttributesExtWayland;
use winit::window::{Fullscreen, Window, WindowId};

use crate::Update;

const FRAME_MS: u64 = 33;
const FRAME: Duration = Duration::from_millis(FRAME_MS);
/// Presents remembered for repainting an older buffer (Wayland keeps two).
const HISTORY: usize = 4;

/// A half-open box of frame pixels.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Area {
    x0: usize,
    y0: usize,
    x1: usize,
    y1: usize,
}

impl Area {
    const ALL: Area = Area { x0: 0, y0: 0, x1: W, y1: H };

    fn union(self, o: Area) -> Area {
        Area { x0: self.x0.min(o.x0), y0: self.y0.min(o.y0), x1: self.x1.max(o.x1), y1: self.y1.max(o.y1) }
    }
}

/// The pixels that differ between two frames, or None.
fn diff(a: &[u32], b: &[u32]) -> Option<Area> {
    let mut area: Option<Area> = None;
    for (y, (ra, rb)) in a.as_chunks::<W>().0.iter().zip(b.as_chunks::<W>().0.iter()).enumerate() {
        let Some(x0) = ra.iter().zip(rb).position(|(p, q)| p != q) else { continue };
        let x1 = W - ra.iter().zip(rb).rev().position(|(p, q)| p != q).unwrap();
        let row = Area { x0, y0: y, x1, y1: y + 1 };
        area = Some(area.map_or(row, |a| a.union(row)));
    }
    area
}

/// Where the frame sits in a w x h window: scale k, offset (ox, oy).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Fit {
    k: usize,
    ox: usize,
    oy: usize,
}

fn fit(w: usize, h: usize) -> Fit {
    let k = (w / W).min(h / H).max(1);
    Fit { k, ox: w.saturating_sub(W * k) / 2, oy: h.saturating_sub(H * k) / 2 }
}

/// Paint one area of the frame into a w x h window buffer, scaled by the fit;
/// a full paint also blacks out the letterbox. Returns the window pixels
/// touched, or None when the area falls outside the window.
fn paint(fb: &[u32], out: &mut [u32], w: usize, h: usize, area: Area, full: bool) -> Option<Rect> {
    let f = fit(w, h);
    if full {
        out.fill(0);
    }
    let (x0, x1) = ((f.ox + area.x0 * f.k).min(w), (f.ox + area.x1 * f.k).min(w));
    let (y0, y1) = ((f.oy + area.y0 * f.k).min(h), (f.oy + area.y1 * f.k).min(h));
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let mut row = vec![0u32; x1 - x0];
    let mut row_of = usize::MAX;
    for y in y0..y1 {
        let sy = (y - f.oy) / f.k;
        if sy != row_of {
            let src = &fb[sy * W..(sy + 1) * W];
            for (i, px) in row.iter_mut().enumerate() {
                *px = src[(x0 + i - f.ox) / f.k];
            }
            row_of = sy;
        }
        out[y * w + x0..y * w + x1].copy_from_slice(&row);
    }
    let nz = |v: usize| NonZeroU32::new(v as u32).unwrap();
    Some(Rect { x: x0 as u32, y: y0 as u32, width: nz(x1 - x0), height: nz(y1 - y0) })
}

struct Shown {
    window: Rc<Window>,
    surface: Surface<Rc<Window>, Rc<Window>>,
}

struct App {
    rx: Receiver<Update>,
    scale: u32,
    shown: Option<Shown>,
    scene: Scene,
    fb: Vec<u32>,
    on_screen: Vec<u32>,
    /// What each recent present changed, newest last; None = everything.
    history: VecDeque<Option<Area>>,
    size: (usize, usize),
    /// Real time handed to the scene so far, in whole milliseconds.
    last: Instant,
    acc: u64,
    next: Instant,
    stale: bool,
    occluded: bool,
}

pub fn run(rx: Receiver<Update>, scale: u32) -> Result<(), Box<dyn Error>> {
    let event_loop = EventLoop::new()?;
    let now = Instant::now();
    let mut app = App {
        rx,
        scale,
        shown: None,
        scene: Scene::new(),
        fb: vec![0; W * H],
        on_screen: vec![0; W * H],
        history: VecDeque::new(),
        size: (0, 0),
        last: now,
        acc: 0,
        next: now,
        stale: true,
        occluded: false,
    };
    event_loop.run_app(&mut app)?;
    Ok(())
}

impl App {
    /// Apply what the feeders sent, advance the scene and render it.
    fn step(&mut self) {
        while let Ok(u) = self.rx.try_recv() {
            match u {
                Update::Sessions(list) => self.scene.set_sessions(&list),
                Update::Usage(u) => self.scene.set_usage(u),
            }
        }
        let now = Instant::now();
        let ms = (now - self.last).as_millis() as u64;
        self.last += Duration::from_millis(ms);
        self.acc = (self.acc + ms).min(1000); // after a stall, do not fast-forward
        while self.acc >= FRAME_MS {
            self.scene.tick(FRAME_MS as u32);
            self.acc -= FRAME_MS;
        }
        self.scene.render(&mut self.fb, chrono::Local::now().time());
        if let Some(s) = &self.shown
            && !self.occluded
            && (self.stale || self.fb != self.on_screen)
        {
            s.window.request_redraw();
        }
    }

    fn present(&mut self) {
        let Some(s) = &mut self.shown else { return };
        let px = s.window.inner_size();
        let (Some(nw), Some(nh)) = (NonZeroU32::new(px.width), NonZeroU32::new(px.height)) else { return };
        let size = (px.width as usize, px.height as usize);
        if size != self.size {
            self.size = size;
            self.stale = true;
        }
        let changed = if self.stale { Some(None) } else { diff(&self.on_screen, &self.fb).map(Some) };
        let Some(changed) = changed else { return };
        if let Err(e) = s.surface.resize(nw, nh) {
            eprintln!("token-cafe: cannot resize the surface: {e}");
            return;
        }
        let mut buf = match s.surface.buffer_mut() {
            Ok(b) => b,
            Err(e) => {
                eprintln!("token-cafe: no surface buffer: {e}");
                return;
            }
        };
        // The buffer shows the frame from `age` presents ago (0 = unknown):
        // repaint what changed in the presents since, plus this frame.
        let age = buf.age() as usize;
        let mut area = changed;
        if age == 0 || age > self.history.len() + 1 {
            area = None;
        } else {
            for past in self.history.iter().rev().take(age - 1) {
                area = match (area, past) {
                    (Some(a), Some(p)) => Some(a.union(*p)),
                    _ => None,
                };
            }
        }
        let (w, h) = size;
        let result = match paint(&self.fb, &mut buf, w, h, area.unwrap_or(Area::ALL), area.is_none()) {
            Some(damage) if area.is_some() => buf.present_with_damage(&[damage]),
            _ => buf.present(),
        };
        if let Err(e) = result {
            eprintln!("token-cafe: cannot present: {e}");
            self.history.clear();
            self.stale = true;
            return;
        }
        self.history.push_back(changed);
        if self.history.len() > HISTORY {
            self.history.pop_front();
        }
        self.on_screen.copy_from_slice(&self.fb);
        self.stale = false;
    }

    fn key(&mut self, el: &ActiveEventLoop, key: Key) {
        let Some(s) = &self.shown else { return };
        match key.as_ref() {
            Key::Character("q") => el.exit(),
            Key::Character("f") | Key::Named(NamedKey::F11) => {
                let full = if s.window.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) };
                s.window.set_fullscreen(full);
            }
            Key::Named(NamedKey::Escape) => s.window.set_fullscreen(None),
            Key::Character(c) => {
                if let Some(k) = c.parse::<u32>().ok().filter(|k| (1..=6).contains(k)) {
                    let _ = s.window.request_inner_size(LogicalSize::new(W as u32 * k, H as u32 * k));
                }
            }
            _ => {}
        }
    }
}

impl ApplicationHandler for App {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.shown.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title("Token Café")
            .with_inner_size(LogicalSize::new(W as u32 * self.scale, H as u32 * self.scale))
            .with_min_inner_size(LogicalSize::new(W as u32, H as u32));
        // app_id (Wayland) / WM_CLASS (X11) "token-cafe", for window rules
        #[cfg(target_os = "linux")]
        let attrs = WindowAttributesExtWayland::with_name(attrs, "token-cafe", "token-cafe");
        let made = el.create_window(attrs).map_err(|e| e.to_string()).and_then(|window| {
            let window = Rc::new(window);
            let context = Context::new(window.clone()).map_err(|e| e.to_string())?;
            let surface = Surface::new(&context, window.clone()).map_err(|e| e.to_string())?;
            Ok(Shown { window, surface })
        });
        match made {
            Ok(s) => {
                s.window.request_redraw();
                self.shown = Some(s);
                self.stale = true;
            }
            Err(e) => {
                eprintln!("token-cafe: cannot open the window: {e}");
                el.exit();
            }
        }
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => el.exit(),
            WindowEvent::RedrawRequested => self.present(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.stale = true;
                if let Some(s) = &self.shown {
                    s.window.request_redraw();
                }
            }
            WindowEvent::Occluded(o) => {
                self.occluded = o;
                self.stale = true;
            }
            WindowEvent::KeyboardInput {
                event: KeyEvent { logical_key, state: ElementState::Pressed, repeat: false, .. },
                ..
            } => self.key(el, logical_key),
            _ => {}
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let now = Instant::now();
        if now >= self.next {
            self.step();
            self.next += FRAME;
            if self.next < now {
                self.next = now + FRAME;
            }
        }
        el.set_control_flow(ControlFlow::WaitUntil(self.next));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame() -> Vec<u32> {
        (0..W * H).map(|i| (i * 7 + i / W) as u32 & 0xffff).collect()
    }

    #[test]
    fn fit_letterboxes_at_whole_scales() {
        assert_eq!(fit(2540, 1356), Fit { k: 5, ox: 470, oy: 78 });
        assert_eq!(fit(960, 720), Fit { k: 3, ox: 0, oy: 0 });
        assert_eq!(fit(200, 100), Fit { k: 1, ox: 0, oy: 0 }); // smaller than the frame: clipped
    }

    #[test]
    fn diff_finds_the_changed_box() {
        let a = frame();
        let mut b = a.clone();
        assert_eq!(diff(&a, &b), None);
        b[10 * W + 20] ^= 1;
        b[30 * W + 5] ^= 1;
        assert_eq!(diff(&a, &b), Some(Area { x0: 5, y0: 10, x1: 21, y1: 31 }));
    }

    #[test]
    fn partial_paint_equals_full_paint() {
        let (w, h) = (1000, 760);
        let old = frame();
        let mut new = old.clone();
        for y in 40..60 {
            for x in 100..130 {
                new[y * W + x] = 0xff0000;
            }
        }
        let mut full = vec![7u32; w * h];
        paint(&new, &mut full, w, h, Area::ALL, true);
        let mut part = vec![7u32; w * h];
        paint(&old, &mut part, w, h, Area::ALL, true);
        let area = diff(&old, &new).unwrap();
        let damage = paint(&new, &mut part, w, h, area, false).unwrap();
        assert!(full == part, "partial repaint differs from a full one");
        let f = fit(w, h);
        assert_eq!((damage.x, damage.y), ((f.ox + 100 * f.k) as u32, (f.oy + 40 * f.k) as u32));
        assert_eq!((damage.width.get(), damage.height.get()), (30 * f.k as u32, 20 * f.k as u32));
        // corners of the scaled frame and the letterbox
        assert_eq!(full[f.oy * w + f.ox], new[0]);
        assert_eq!(full[(f.oy + H * f.k - 1) * w + f.ox + W * f.k - 1], new[W * H - 1]);
        assert_eq!(full[0], 0);
    }
}
