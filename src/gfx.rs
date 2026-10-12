//! Software drawing into the 320x240 frame (0x00RRGGBB pixels): rectangles,
//! shadows, palette sprites and the 5x7 font, the primitives design/cafe.js
//! draws with. Every call clips to the screen and to the current clip
//! rectangle.

use crate::assets::{BASE_PAL, FONT, FONT_HAS, H, Pal, Sprite, W};

impl Pal {
    pub fn base() -> Pal {
        BASE_PAL
    }

    pub fn set(&mut self, slot: usize, c: u32) {
        self.c[slot] = c;
        self.set |= 1 << slot;
    }
}

pub struct Gfx<'a> {
    fb: &'a mut [u32],
    cx0: i32,
    cy0: i32,
    cx1: i32,
    cy1: i32,
}

pub fn text_width(s: &str, k: i32) -> i32 {
    let n = s.chars().count() as i32;
    if n > 0 { (n * 6 - 1) * k } else { 0 }
}

fn glyph(ch: u8) -> &'static [u8; 7] {
    let mut c = ch.to_ascii_uppercase() as usize;
    if !(32..=127).contains(&c) || !FONT_HAS[c - 32] {
        c = b'?' as usize;
    }
    &FONT[c - 32]
}

impl<'a> Gfx<'a> {
    pub fn new(fb: &'a mut [u32]) -> Self {
        assert_eq!(fb.len(), W * H);
        Gfx { fb, cx0: 0, cy0: 0, cx1: W as i32, cy1: H as i32 }
    }

    /// Half-open box.
    pub fn clip(&mut self, x0: i32, y0: i32, x1: i32, y1: i32) {
        self.cx0 = x0.max(0);
        self.cy0 = y0.max(0);
        self.cx1 = x1.min(W as i32);
        self.cy1 = y1.min(H as i32);
    }

    pub fn noclip(&mut self) {
        self.clip(0, 0, W as i32, H as i32);
    }

    pub fn px(&mut self, x: i32, y: i32, c: u32) {
        if x < self.cx0 || y < self.cy0 || x >= self.cx1 || y >= self.cy1 {
            return;
        }
        self.fb[y as usize * W + x as usize] = c;
    }

    pub fn rect(&mut self, x: i32, y: i32, w: i32, h: i32, c: u32) {
        let (x0, y0) = (x.max(self.cx0), y.max(self.cy0));
        let (x1, y1) = ((x + w).min(self.cx1), (y + h).min(self.cy1));
        for j in y0..y1 {
            for i in x0..x1 {
                self.fb[j as usize * W + i as usize] = c;
            }
        }
    }

    /// Multiply the pixels under a box by k (shadows).
    pub fn darken(&mut self, x: i32, y: i32, w: i32, h: i32, k: f64) {
        let (x0, y0) = (x.max(self.cx0), y.max(self.cy0));
        let (x1, y1) = ((x + w).min(self.cx1), (y + h).min(self.cy1));
        let scale = |v: u32| ((v & 255) as f64 * k) as u32;
        for j in y0..y1 {
            for i in x0..x1 {
                let p = &mut self.fb[j as usize * W + i as usize];
                *p = (scale(*p >> 16) << 16) | (scale(*p >> 8) << 8) | scale(*p);
            }
        }
    }

    /// Sprites use palette slots; None means the base palette.
    pub fn blit(&mut self, s: &Sprite, x: i32, y: i32, pal: Option<&Pal>, flip: bool) {
        let pal = pal.unwrap_or(&BASE_PAL);
        let w = s.w as usize;
        for j in 0..s.h as usize {
            let row = &s.px[j * w..(j + 1) * w];
            for i in 0..w {
                let slot = row[if flip { w - 1 - i } else { i }] as usize;
                if slot == 0 || (pal.set >> slot) & 1 == 0 {
                    continue;
                }
                self.px(x + i as i32, y + j as i32, pal.c[slot]);
            }
        }
    }

    /// k-times enlarged sprite, optionally limited to the box [x0, y0, x1, y1).
    pub fn blit_scaled(&mut self, s: &Sprite, x: i32, y: i32, pal: Option<&Pal>, k: i32, bx: Option<[i32; 4]>) {
        let pal = pal.unwrap_or(&BASE_PAL);
        let w = s.w as usize;
        for j in 0..s.h as usize {
            for i in 0..w {
                let slot = s.px[j * w + i] as usize;
                if slot == 0 || (pal.set >> slot) & 1 == 0 {
                    continue;
                }
                for v in 0..k {
                    for u in 0..k {
                        let (px, py) = (x + i as i32 * k + u, y + j as i32 * k + v);
                        if let Some(b) = bx
                            && (px < b[0] || py < b[1] || px >= b[2] || py >= b[3])
                        {
                            continue;
                        }
                        self.px(px, py, pal.c[slot]);
                    }
                }
            }
        }
    }

    /// 5x7 capitals, 6 px advance per character at scale 1; lower case is
    /// upper-cased. The shadow is drawn 1 px down-right first.
    pub fn text(&mut self, s: &str, x: i32, y: i32, c: u32, k: i32, shadow: Option<u32>) {
        let passes: &[(u32, i32)] = match shadow {
            Some(sh) => &[(sh, 1), (c, 0)],
            None => &[(c, 0)],
        };
        for &(col, o) in passes {
            for (n, ch) in s.chars().enumerate() {
                let g = glyph(if ch.is_ascii() { ch as u8 } else { b'?' });
                for (j, bits) in g.iter().enumerate() {
                    for i in 0..5 {
                        if (bits >> (4 - i)) & 1 == 0 {
                            continue;
                        }
                        self.rect(x + (n as i32 * 6 + i) * k + o, y + j as i32 * k + o, k, k, col);
                    }
                }
            }
        }
    }
}
