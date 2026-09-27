//! A tiny Game of Life that stands in for the spinner glyph next to a few
//! waiting verbs ("Thinking…", "Connecting…", "Installing…"). It is a 5x5
//! torus seeded with a glider, a spaceship or an oscillator, stepping about
//! seven generations a second, and it takes exactly the square the spinner
//! it replaces took. Dying cells fade out instead of blinking off.
//!
//! [`Grid`] is a bitboard (one `u32` per row), so a step allocates nothing.
//! Each spinner owns a small cached view, so its frames repaint only itself,
//! and they come from the shared pulse clock, which parks when nothing on
//! screen renews it: a hidden spinner costs nothing. Reduced motion draws a
//! still glider.

use std::time::Instant;

use gpui::{
    App, AppContext, Bounds, Context, Entity, Hsla, IntoElement, ParentElement, Render, RenderOnce,
    SharedString, Styled, Window, canvas, div, point, px, size,
};

/// Generations per second.
pub const GENERATIONS_PER_SEC: f32 = 7.0;
/// Largest grid side a [`Grid`] holds.
pub const MAX_SIDE: usize = 32;
/// Cells per side of the spinner.
pub const SPINNER_SIDE: usize = 5;
/// A run that has been repeating for this many generations gets a new seed.
const LOOP_AFTER: u32 = 42;
/// Recent states remembered for loop detection.
const HISTORY: usize = 16;

/// A toroidal Life grid, `w` x `h` cells, row-major bitboard.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Grid {
    w: usize,
    h: usize,
    rows: [u32; MAX_SIDE],
}

impl std::fmt::Debug for Grid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for y in 0..self.h {
            for x in 0..self.w {
                f.write_str(if self.get(x, y) { "#" } else { "." })?;
            }
            f.write_str("\n")?;
        }
        Ok(())
    }
}

impl Grid {
    pub fn new(w: usize, h: usize) -> Self {
        assert!((3..=MAX_SIDE).contains(&w) && (3..=MAX_SIDE).contains(&h));
        Self {
            w,
            h,
            rows: [0; MAX_SIDE],
        }
    }

    /// Place `cells` (x, y) offset by `(dx, dy)`, wrapping.
    pub fn with(mut self, cells: &[(usize, usize)], dx: usize, dy: usize) -> Self {
        for &(x, y) in cells {
            self.set((x + dx) % self.w, (y + dy) % self.h, true);
        }
        self
    }

    pub fn width(&self) -> usize {
        self.w
    }

    pub fn height(&self) -> usize {
        self.h
    }

    pub fn get(&self, x: usize, y: usize) -> bool {
        (self.rows[y] >> x) & 1 == 1
    }

    pub fn set(&mut self, x: usize, y: usize, alive: bool) {
        if alive {
            self.rows[y] |= 1 << x;
        } else {
            self.rows[y] &= !(1 << x);
        }
    }

    pub fn population(&self) -> u32 {
        self.rows[..self.h].iter().map(|r| r.count_ones()).sum()
    }

    fn mask(&self) -> u32 {
        if self.w == 32 {
            u32::MAX
        } else {
            (1 << self.w) - 1
        }
    }

    /// Rotate a row by one cell, wrapping at the grid's width.
    fn roll(&self, row: u32, left: bool) -> u32 {
        let w = self.w as u32;
        let m = self.mask();
        if left {
            ((row << 1) | (row >> (w - 1))) & m
        } else {
            ((row >> 1) | (row << (w - 1))) & m
        }
    }

    /// One generation, B3/S23, on a torus. Bit-parallel: each row's eight
    /// neighbor planes go through a small adder, one bit lane per cell.
    pub fn step(&self) -> Self {
        let mut next = *self;
        for y in 0..self.h {
            let up = self.rows[(y + self.h - 1) % self.h];
            let mid = self.rows[y];
            let down = self.rows[(y + 1) % self.h];
            let planes = [
                self.roll(up, true),
                up,
                self.roll(up, false),
                self.roll(mid, true),
                self.roll(mid, false),
                self.roll(down, true),
                down,
                self.roll(down, false),
            ];
            // Neighbor count per lane: (b1, b0) counts mod 4, b2 latches once
            // it reaches four (always dead next generation).
            let (mut b0, mut b1, mut b2) = (0u32, 0u32, 0u32);
            for p in planes {
                let c0 = b0 & p;
                b0 ^= p;
                let c1 = b1 & c0;
                b1 ^= c0;
                b2 |= c1;
            }
            let three = b0 & b1 & !b2;
            let two = !b0 & b1 & !b2;
            next.rows[y] = (three | (two & mid)) & self.mask();
        }
        next
    }

    fn fingerprint(&self) -> u64 {
        // FNV-1a over the used rows.
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for row in &self.rows[..self.h] {
            for byte in row.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x1_0000_01b3);
            }
        }
        hash
    }
}

/// Starting patterns, cycled through as runs die out or settle into a loop.
pub mod seeds {
    pub const GLIDER: &[(usize, usize)] = &[(1, 0), (2, 1), (0, 2), (1, 2), (2, 2)];
    pub const LWSS: &[(usize, usize)] = &[
        (1, 0),
        (4, 0),
        (0, 1),
        (0, 2),
        (4, 2),
        (0, 3),
        (1, 3),
        (2, 3),
        (3, 3),
    ];
    pub const BLINKER: &[(usize, usize)] = &[(0, 1), (1, 1), (2, 1)];
    pub const TOAD: &[(usize, usize)] = &[(1, 0), (2, 0), (3, 0), (0, 1), (1, 1), (2, 1)];
    pub const BEACON: &[(usize, usize)] = &[(0, 0), (1, 0), (0, 1), (3, 2), (2, 3), (3, 3)];
}

const SEEDS: &[&[(usize, usize)]] = &[
    seeds::GLIDER,
    seeds::TOAD,
    seeds::LWSS,
    seeds::BEACON,
    seeds::BLINKER,
];

fn seeded(w: usize, h: usize, seed: usize) -> Grid {
    let cells = SEEDS[seed % SEEDS.len()];
    Grid::new(w, h).with(cells, (w / 2).saturating_sub(2), (h / 2).saturating_sub(2))
}

/// One spinner's evolving state: the current and previous generation (for
/// the fade), and enough history to notice a loop.
#[derive(Clone)]
pub struct Run {
    pub grid: Grid,
    pub prev: Grid,
    seed: usize,
    since_seed: u32,
    history: [u64; HISTORY],
    history_at: usize,
}

impl Run {
    pub fn new(w: usize, h: usize, seed: usize) -> Self {
        let grid = seeded(w, h, seed);
        Self {
            grid,
            prev: grid,
            seed,
            since_seed: 0,
            history: [0; HISTORY],
            history_at: 0,
        }
    }

    /// Advance one generation, reseeding when the grid empties or has been
    /// cycling for a while.
    pub fn advance(&mut self) {
        let next = self.grid.step();
        let fingerprint = next.fingerprint();
        let looped = self.history.contains(&fingerprint);
        self.history[self.history_at] = fingerprint;
        self.history_at = (self.history_at + 1) % HISTORY;
        self.prev = self.grid;
        self.grid = next;
        self.since_seed += 1;
        if next.population() == 0 || (looped && self.since_seed >= LOOP_AFTER) {
            self.seed += 1;
            // `prev` keeps the old pattern so it fades out under the new one.
            self.grid = seeded(self.grid.w, self.grid.h, self.seed);
            self.since_seed = 0;
            self.history = [0; HISTORY];
        }
    }
}

/// Opacity of a cell at `t` (0..1) through the current generation: alive
/// cells full, newborns fading in quickly, the just-died fading out. Pure.
pub fn cell_opacity(alive: bool, was_alive: bool, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    match (alive, was_alive) {
        (true, true) => 1.0,
        (true, false) => 0.35 + 0.65 * (t * 2.0).min(1.0),
        (false, true) => 0.55 * (1.0 - t),
        (false, false) => 0.0,
    }
}

/// The spinner: a `side_px` square, the same square the spinner glyph it
/// replaces occupied. `key` names the instance so its run survives
/// re-renders.
pub fn life_spinner(key: impl Into<SharedString>, side_px: f32, color: Hsla) -> LifeSpinner {
    LifeSpinner {
        key: key.into(),
        side_px,
        color,
    }
}

#[derive(IntoElement)]
pub struct LifeSpinner {
    key: SharedString,
    side_px: f32,
    color: Hsla,
}

impl RenderOnce for LifeSpinner {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        // Same pattern as the mini spinner: a cached child view, so each
        // frame repaints these cells and not the row around them.
        let view = window.with_global_id(self.key.into(), |id, window| {
            window.with_element_state(id, |previous: Option<Entity<LifeSpinnerView>>, _| {
                let view = previous.unwrap_or_else(|| {
                    cx.new(|_| LifeSpinnerView {
                        side_px: self.side_px,
                        color: self.color,
                        run: Run::new(SPINNER_SIDE, SPINNER_SIDE, 0),
                        started: Instant::now(),
                        generation: 0,
                    })
                });
                view.update(cx, |view, cx| {
                    if view.side_px != self.side_px || view.color != self.color {
                        view.side_px = self.side_px;
                        view.color = self.color;
                        cx.notify();
                    }
                });
                (view.clone(), view)
            })
        });
        view.cached(
            gpui::StyleRefinement::default()
                .w(px(self.side_px))
                .h(px(self.side_px)),
        )
    }
}

pub struct LifeSpinnerView {
    side_px: f32,
    color: Hsla,
    run: Run,
    started: Instant,
    generation: u64,
}

impl Render for LifeSpinnerView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (grid, prev, t) = if cx.reduce_motion() {
            let still = seeded(SPINNER_SIDE, SPINNER_SIDE, 0);
            (still, still, 1.0)
        } else {
            crate::motion::pulse_lease_slow(cx.entity_id(), cx);
            let elapsed = self.started.elapsed().as_secs_f32() * GENERATIONS_PER_SEC;
            let target = elapsed as u64;
            if target > self.generation + 8 {
                // Back from being hidden: skip ahead instead of replaying.
                self.generation = target - 1;
            }
            while self.generation < target {
                self.run.advance();
                self.generation += 1;
            }
            (self.run.grid, self.run.prev, elapsed.fract())
        };
        let side = self.side_px;
        let color = self.color;
        let pitch = side / SPINNER_SIDE as f32;
        let cell = pitch * 0.8;
        let inset = (pitch - cell) / 2.0;
        let radius = cell * 0.3;
        div().size(px(side)).child(
            canvas(
                |_, _, _| (),
                move |bounds: Bounds<gpui::Pixels>, _, window, _| {
                    for y in 0..SPINNER_SIDE {
                        for x in 0..SPINNER_SIDE {
                            let alpha = cell_opacity(grid.get(x, y), prev.get(x, y), t);
                            if alpha <= 0.01 {
                                continue;
                            }
                            let origin = bounds.origin
                                + point(px(x as f32 * pitch + inset), px(y as f32 * pitch + inset));
                            window.paint_quad(
                                gpui::fill(
                                    Bounds::new(origin, size(px(cell), px(cell))),
                                    color.opacity(color.a * alpha),
                                )
                                .corner_radii(px(radius)),
                            );
                        }
                    }
                },
            )
            .size_full(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(grid: &Grid) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        for y in 0..grid.height() {
            for x in 0..grid.width() {
                if grid.get(x, y) {
                    out.push((x, y));
                }
            }
        }
        out
    }

    #[test]
    fn blinker_has_period_two() {
        let start = Grid::new(8, 8).with(seeds::BLINKER, 2, 2);
        let one = start.step();
        assert_ne!(one, start);
        assert_eq!(cells(&one), vec![(3, 2), (3, 3), (3, 4)]);
        assert_eq!(one.step(), start);
    }

    #[test]
    fn glider_moves_one_diagonal_every_four_generations_on_a_torus() {
        for (w, h) in [(SPINNER_SIDE, SPINNER_SIDE), (8, 8), (10, 12), (32, 32)] {
            let start = Grid::new(w, h).with(seeds::GLIDER, 0, 0);
            let mut grid = start;
            for lap in 1..=(w.max(h) * 2) {
                for _ in 0..4 {
                    grid = grid.step();
                }
                let expected = Grid::new(w, h).with(seeds::GLIDER, lap % w, lap % h);
                assert_eq!(grid, expected, "{w}x{h} after {lap} laps");
            }
        }
    }

    #[test]
    fn still_lifes_stay_and_lonely_cells_die() {
        let block = Grid::new(6, 6).with(&[(1, 1), (2, 1), (1, 2), (2, 2)], 0, 0);
        assert_eq!(block.step(), block);
        let lonely = Grid::new(6, 6).with(&[(3, 3)], 0, 0);
        assert_eq!(lonely.step().population(), 0);
    }

    #[test]
    fn edges_wrap() {
        // A blinker straddling the left/right seam still oscillates.
        let seam = Grid::new(8, 8).with(&[(7, 3), (0, 3), (1, 3)], 0, 0);
        assert_eq!(cells(&seam.step()), vec![(0, 2), (0, 3), (0, 4)]);
        assert_eq!(seam.step().step(), seam);
    }

    #[test]
    fn runs_reseed_when_they_die_or_loop() {
        // A lone cell dies at once and the run moves to the next seed.
        let mut run = Run::new(SPINNER_SIDE, SPINNER_SIDE, 0);
        run.grid = Grid::new(SPINNER_SIDE, SPINNER_SIDE).with(&[(4, 4)], 0, 0);
        run.advance();
        assert!(run.grid.population() > 0);
        assert_eq!(run.seed, 1);
        // A blinker loops; after LOOP_AFTER generations it is replaced.
        let blinker = SEEDS.iter().position(|s| *s == seeds::BLINKER).unwrap();
        let mut run = Run::new(SPINNER_SIDE, SPINNER_SIDE, blinker);
        assert_eq!(cells(&run.grid).len(), 3);
        for _ in 0..LOOP_AFTER {
            run.advance();
        }
        assert_eq!(run.seed, blinker + 1, "blinker should have been reseeded");
        // Every seed keeps the spinner alive on its 5x5 torus: nothing is
        // ever drawn empty.
        let mut run = Run::new(SPINNER_SIDE, SPINNER_SIDE, 0);
        for _ in 0..5000 {
            run.advance();
            assert!(run.grid.population() > 0);
        }
    }

    #[test]
    fn fades_are_monotonic() {
        assert_eq!(cell_opacity(true, true, 0.3), 1.0);
        assert_eq!(cell_opacity(false, false, 0.3), 0.0);
        assert!(cell_opacity(false, true, 0.1) > cell_opacity(false, true, 0.9));
        assert!(cell_opacity(true, false, 0.1) < cell_opacity(true, false, 0.9));
        assert_eq!(cell_opacity(false, true, 1.0), 0.0);
    }
}
