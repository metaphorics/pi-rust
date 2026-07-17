//! Animated Armin XBM easter egg.
use crate::modes::interactive::theme::{ThemeColor, theme};
use pi_tui::component::{Component, RenderStatus};
use pi_tui::line::Line;
use std::time::{SystemTime, UNIX_EPOCH};
const WIDTH: usize = 31;
const HEIGHT: usize = 36;
const DISPLAY_HEIGHT: usize = HEIGHT.div_ceil(2);
const BITS: [u8; 144] = [
    0xff, 0xff, 0xff, 0x7f, 0xff, 0xf0, 0xff, 0x7f, 0xff, 0xed, 0xff, 0x7f, 0xff, 0xdb, 0xff, 0x7f,
    0xff, 0xb7, 0xff, 0x7f, 0xff, 0x77, 0xfe, 0x7f, 0x3f, 0xf8, 0xfe, 0x7f, 0xdf, 0xff, 0xfe, 0x7f,
    0xdf, 0x3f, 0xfc, 0x7f, 0x9f, 0xc3, 0xfb, 0x7f, 0x6f, 0xfc, 0xf4, 0x7f, 0xf7, 0x0f, 0xf7, 0x7f,
    0xf7, 0xff, 0xf7, 0x7f, 0xf7, 0xff, 0xe3, 0x7f, 0xf7, 0x07, 0xe8, 0x7f, 0xef, 0xf8, 0x67, 0x70,
    0x0f, 0xff, 0xbb, 0x6f, 0xf1, 0x00, 0xd0, 0x5b, 0xfd, 0x3f, 0xec, 0x53, 0xc1, 0xff, 0xef, 0x57,
    0x9f, 0xfd, 0xee, 0x5f, 0x9f, 0xfc, 0xae, 0x5f, 0x1f, 0x78, 0xac, 0x5f, 0x3f, 0x00, 0x50, 0x6c,
    0x7f, 0x00, 0xdc, 0x77, 0xff, 0xc0, 0x3f, 0x78, 0xff, 0x01, 0xf8, 0x7f, 0xff, 0x03, 0x9c, 0x78,
    0xff, 0x07, 0x8c, 0x7c, 0xff, 0x0f, 0xce, 0x78, 0xff, 0xff, 0xcf, 0x7f, 0xff, 0xff, 0xcf, 0x78,
    0xff, 0xff, 0xdf, 0x78, 0xff, 0xff, 0xdf, 0x7d, 0xff, 0xff, 0x3f, 0x7e, 0xff, 0xff, 0xff, 0x7f,
];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArminEffect {
    Typewriter,
    Scanline,
    Rain,
    Fade,
    Crt,
    Glitch,
    Dissolve,
}
#[derive(Clone, Debug)]
enum State {
    Typewriter(usize),
    Scanline(usize),
    Rain(Vec<(isize, usize)>),
    Fade(Vec<(usize, usize)>, usize),
    Crt(usize),
    Glitch(usize),
    Dissolve(Vec<(usize, usize)>, usize),
}
fn rand(seed: &mut u64, n: usize) -> usize {
    if n == 0 {
        return 0;
    }
    *seed ^= *seed << 13;
    *seed ^= *seed >> 7;
    *seed ^= *seed << 17;
    *seed as usize % n
}
fn shuffle<T>(v: &mut [T], seed: &mut u64) {
    for i in (1..v.len()).rev() {
        let j = rand(seed, i + 1);
        v.swap(i, j)
    }
}
fn positions() -> Vec<(usize, usize)> {
    (0..DISPLAY_HEIGHT)
        .flat_map(|r| (0..WIDTH).map(move |x| (r, x)))
        .collect()
}
fn pixel(x: usize, y: usize) -> bool {
    y < HEIGHT && ((BITS[y * WIDTH.div_ceil(8) + x / 8] >> (x % 8)) & 1) == 0
}
fn cell(x: usize, r: usize) -> char {
    match (pixel(x, r * 2), pixel(x, r * 2 + 1)) {
        (true, true) => '█',
        (true, false) => '▀',
        (false, true) => '▄',
        _ => ' ',
    }
}
fn final_grid() -> Vec<Vec<char>> {
    (0..DISPLAY_HEIGHT)
        .map(|r| (0..WIDTH).map(|x| cell(x, r)).collect())
        .collect()
}
fn empty() -> Vec<Vec<char>> {
    vec![vec![' '; WIDTH]; DISPLAY_HEIGHT]
}
fn seed() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(1, |d| d.as_nanos() as u64)
        .max(1)
}
pub struct Armin {
    effect: ArminEffect,
    final_grid: Vec<Vec<char>>,
    grid: Vec<Vec<char>>,
    state: State,
    seed: u64,
    version: usize,
    cached_version: Option<usize>,
    cached_width: Option<u16>,
    cached: Vec<Line>,
    status: RenderStatus,
}
impl Armin {
    #[must_use]
    pub fn new() -> Self {
        let mut s = seed();
        let e = match rand(&mut s, 7) {
            0 => ArminEffect::Typewriter,
            1 => ArminEffect::Scanline,
            2 => ArminEffect::Rain,
            3 => ArminEffect::Fade,
            4 => ArminEffect::Crt,
            5 => ArminEffect::Glitch,
            _ => ArminEffect::Dissolve,
        };
        Self::with_effect(e, s)
    }
    #[must_use]
    pub fn with_effect(effect: ArminEffect, mut seed: u64) -> Self {
        seed = seed.max(1);
        let final_grid = final_grid();
        let mut grid = empty();
        let state = match effect {
            ArminEffect::Typewriter => State::Typewriter(0),
            ArminEffect::Scanline => State::Scanline(0),
            ArminEffect::Rain => State::Rain(
                (0..WIDTH)
                    .map(|_| (-(rand(&mut seed, DISPLAY_HEIGHT * 2) as isize), 0))
                    .collect(),
            ),
            ArminEffect::Fade => {
                let mut p = positions();
                shuffle(&mut p, &mut seed);
                State::Fade(p, 0)
            }
            ArminEffect::Crt => State::Crt(0),
            ArminEffect::Glitch => State::Glitch(0),
            ArminEffect::Dissolve => {
                let noise = [' ', '░', '▒', '▓', '█', '▀', '▄'];
                for row in &mut grid {
                    for c in row {
                        *c = noise[rand(&mut seed, noise.len())]
                    }
                }
                let mut p = positions();
                shuffle(&mut p, &mut seed);
                State::Dissolve(p, 0)
            }
        };
        Self {
            effect,
            final_grid,
            grid,
            state,
            seed,
            version: 0,
            cached_version: None,
            cached_width: None,
            cached: Vec::new(),
            status: RenderStatus::Changed,
        }
    }
    #[must_use]
    pub fn effect(&self) -> ArminEffect {
        self.effect
    }
    pub fn tick(&mut self) -> bool {
        let done = match &mut self.state {
            State::Typewriter(pos) => {
                for _ in 0..3 {
                    let r = *pos / WIDTH;
                    let x = *pos % WIDTH;
                    if r >= DISPLAY_HEIGHT {
                        break;
                    }
                    self.grid[r][x] = self.final_grid[r][x];
                    *pos += 1
                }
                *pos >= WIDTH * DISPLAY_HEIGHT
            }
            State::Scanline(row) => {
                if *row >= DISPLAY_HEIGHT {
                    true
                } else {
                    self.grid[*row].clone_from(&self.final_grid[*row]);
                    *row += 1;
                    false
                }
            }
            State::Rain(drops) => {
                self.grid = empty();
                let mut done = true;
                for (x, (y, settled)) in drops.iter_mut().enumerate() {
                    for r in DISPLAY_HEIGHT.saturating_sub(*settled)..DISPLAY_HEIGHT {
                        self.grid[r][x] = self.final_grid[r][x]
                    }
                    if *settled >= DISPLAY_HEIGHT {
                        continue;
                    }
                    done = false;
                    let end = DISPLAY_HEIGHT.saturating_sub(*settled);
                    let target = (0..end).rev().find(|r| self.final_grid[*r][x] != ' ');
                    *y += 1;
                    if *y >= 0 && (*y as usize) < DISPLAY_HEIGHT {
                        if target.is_some_and(|r| *y as usize >= r) {
                            let r = target.expect("target exists");
                            *settled = DISPLAY_HEIGHT - r;
                            *y = -(rand(&mut self.seed, 5) as isize) - 1
                        } else {
                            self.grid[*y as usize][x] = '▓'
                        }
                    }
                }
                done
            }
            State::Fade(p, i) => {
                for _ in 0..15 {
                    let Some(&(r, x)) = p.get(*i) else {
                        break;
                    };
                    self.grid[r][x] = self.final_grid[r][x];
                    *i += 1
                }
                *i >= p.len()
            }
            State::Crt(exp) => {
                self.grid = empty();
                let mid = DISPLAY_HEIGHT / 2;
                for r in mid.saturating_sub(*exp)..=(mid + *exp).min(DISPLAY_HEIGHT - 1) {
                    self.grid[r].clone_from(&self.final_grid[r])
                }
                *exp += 1;
                *exp > DISPLAY_HEIGHT
            }
            State::Glitch(phase) => {
                if *phase < 8 {
                    let mut g = Vec::with_capacity(DISPLAY_HEIGHT);
                    for source in &self.final_grid {
                        let mut row = source.clone();
                        let off = rand(&mut self.seed, 7) as isize - 3;
                        if rand(&mut self.seed, 10) < 3 {
                            if off >= 0 {
                                row.rotate_left(off as usize)
                            } else {
                                row.rotate_right((-off) as usize)
                            }
                        } else if rand(&mut self.seed, 10) < 2 {
                            row = self.final_grid[rand(&mut self.seed, DISPLAY_HEIGHT)].clone()
                        }
                        g.push(row)
                    }
                    self.grid = g;
                    *phase += 1;
                    false
                } else {
                    self.grid.clone_from(&self.final_grid);
                    true
                }
            }
            State::Dissolve(p, i) => {
                for _ in 0..20 {
                    let Some(&(r, x)) = p.get(*i) else {
                        break;
                    };
                    self.grid[r][x] = self.final_grid[r][x];
                    *i += 1
                }
                *i >= p.len()
            }
        };
        self.version = self.version.wrapping_add(1);
        self.cached_width = None;
        done
    }
    pub fn dispose(&mut self) {}
}
impl Default for Armin {
    fn default() -> Self {
        Self::new()
    }
}
impl Component for Armin {
    fn render(&mut self, width: u16) -> &[Line] {
        if self.cached_width == Some(width) && self.cached_version == Some(self.version) {
            self.status = RenderStatus::Unchanged;
            return &self.cached;
        }
        let w = usize::from(width);
        let available = w.saturating_sub(1);
        let mut lines = Vec::with_capacity(DISPLAY_HEIGHT + 1);
        for row in &self.grid {
            let clipped = row.iter().take(available).collect::<String>();
            lines.push(format!(
                " {}{}",
                theme().fg(ThemeColor::Accent, &clipped),
                " ".repeat(w.saturating_sub(1 + clipped.chars().count()))
            ))
        }
        let msg = "ARMIN SAYS HI";
        lines.push(format!(
            " {}{}",
            theme().fg(ThemeColor::Accent, msg),
            " ".repeat(w.saturating_sub(1 + msg.len()))
        ));
        self.cached = lines.into_iter().map(|s| Line::from_ansi(&s)).collect();
        self.cached_width = Some(width);
        self.cached_version = Some(self.version);
        self.status = RenderStatus::Changed;
        &self.cached
    }
    fn invalidate(&mut self) {
        self.cached_width = None;
        self.status = RenderStatus::Changed
    }
    fn last_render_status(&self) -> RenderStatus {
        self.status
    }
}
