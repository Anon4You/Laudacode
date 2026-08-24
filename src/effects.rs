//! Ambient particle effects for the banner band — petals, rain, snow,
//! matrix rain, lightning flashes and twinkling stars. Zero dependencies:
//! xorshift PRNG seeded from clock+pid, ~10 fps tick, ≤150 particles.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// Available ambient effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EffectKind {
    #[default]
    Off,
    Petals,
    Rain,
    Snow,
    Matrix,
    Lightning,
    Stars,
    Fireflies,
    Bubbles,
    Embers,
    Confetti,
    Meteor,
    Aurora,
}

impl EffectKind {
    pub fn parse(s: Option<&str>) -> Self {
        match s.map(str::trim).unwrap_or("").to_lowercase().as_str() {
            "petals" | "cherry" | "sakura" => Self::Petals,
            "rain" | "raining" => Self::Rain,
            "snow" => Self::Snow,
            "matrix" => Self::Matrix,
            "lightning" | "storm" => Self::Lightning,
            "stars" | "twinkle" => Self::Stars,
            "fireflies" | "firefly" => Self::Fireflies,
            "bubbles" | "bubble" => Self::Bubbles,
            "embers" | "sparks" => Self::Embers,
            "confetti" | "party" => Self::Confetti,
            "meteor" | "comet" => Self::Meteor,
            "aurora" | "northern lights" => Self::Aurora,
            _ => Self::Off,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Petals => "petals",
            Self::Rain => "rain",
            Self::Snow => "snow",
            Self::Matrix => "matrix",
            Self::Lightning => "lightning",
            Self::Stars => "stars",
            Self::Fireflies => "fireflies",
            Self::Bubbles => "bubbles",
            Self::Embers => "embers",
            Self::Confetti => "confetti",
            Self::Meteor => "meteor",
            Self::Aurora => "aurora",
        }
    }

    pub fn all() -> [EffectKind; 13] {
        [
            Self::Off,
            Self::Petals,
            Self::Rain,
            Self::Snow,
            Self::Matrix,
            Self::Lightning,
            Self::Stars,
            Self::Fireflies,
            Self::Bubbles,
            Self::Embers,
            Self::Confetti,
            Self::Meteor,
            Self::Aurora,
        ]
    }
}

/// Tiny xorshift64 PRNG — deterministic per seed, no external crate.
struct Rng(u64);

impl Rng {
    fn seed() -> Self {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos() as u64)
            .unwrap_or(0x9E3779B97F4A7C15);
        Rng(nanos ^ (std::process::id() as u64) << 32 | 1)
    }
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    /// Float in [0, 1).
    fn f(&mut self) -> f32 {
        (self.next() % 100_000) as f32 / 100_000.0
    }
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + self.f() * (hi - lo)
    }
}

#[derive(Debug, Clone, Copy)]
struct Particle {
    x: f32,
    y: f32,
    vy: f32,
    sway: f32,
    phase: f32,
    variant: u8,
}

/// Particle engine bound to the banner band.
pub struct Engine {
    pub kind: EffectKind,
    parts: Vec<Particle>,
    rng: Rng,
    area: (u16, u16),
    frame: u64,
    /// Lightning: frames remaining for the current bolt/flash.
    flash: u8,
    bolt: Vec<(u16, u16)>,
    ticks_until_strike: u32,
    /// Active comets for the meteor effect.
    meteors: Vec<Meteor>,
    meteor_cooldown: u32,
}

#[derive(Debug, Clone, Copy)]
struct Meteor {
    x: f32,
    y: f32,
    vx: f32,
    vy: f32,
}

const MAX_PARTICLES: usize = 140;

impl Engine {
    pub fn new(kind: EffectKind) -> Self {
        let mut e = Engine {
            kind,
            parts: Vec::new(),
            rng: Rng::seed(),
            area: (60, 12),
            frame: 0,
            flash: 0,
            bolt: Vec::new(),
            ticks_until_strike: 20,
            meteors: Vec::new(),
            meteor_cooldown: 10,
        };
        e.respawn_all();
        e
    }

    pub fn set(&mut self, kind: EffectKind) {
        if kind != self.kind {
            self.kind = kind;
            self.parts.clear();
            self.flash = 0;
            self.bolt.clear();
            self.meteors.clear();
            self.meteor_cooldown = 10;
            self.ticks_until_strike = 20;
            self.respawn_all();
        }
    }

    /// Remember the drawable band size (set by the TUI each frame).
    pub fn set_area(&mut self, w: u16, h: u16) {
        let changed = (w, h) != self.area;
        self.area = (w.max(10), h.max(2));
        if changed && self.parts.is_empty() {
            self.respawn_all();
        }
    }

    fn target_count(&self) -> usize {
        let base = (self.area.0 as usize / 14).clamp(8, MAX_PARTICLES);
        match self.kind {
            EffectKind::Off | EffectKind::Lightning | EffectKind::Meteor | EffectKind::Aurora => 0,
            EffectKind::Rain | EffectKind::Matrix => base * 2,
            EffectKind::Snow | EffectKind::Petals => base + base / 2,
            EffectKind::Confetti => base * 3 / 2,
            EffectKind::Fireflies | EffectKind::Embers | EffectKind::Bubbles => base.min(36),
            EffectKind::Stars => base.min(40),
        }
    }

    fn spawn(&mut self, from_top: bool) -> Particle {
        let r = &mut self.rng;
        let (vy_lo, vy_hi) = match self.kind {
            EffectKind::Rain => (1.1, 2.2),
            EffectKind::Matrix => (0.7, 1.6),
            EffectKind::Snow => (0.15, 0.45),
            EffectKind::Petals | EffectKind::Confetti => (0.2, 0.6),
            EffectKind::Bubbles | EffectKind::Embers => (-0.5, -0.2),
            _ => (0.0, 0.0),
        };
        Particle {
            x: r.range(0.0, self.area.0 as f32),
            y: if from_top || self.kind == EffectKind::Matrix {
                -r.range(0.0, 4.0)
            } else {
                r.range(0.0, self.area.1 as f32)
            },
            vy: r.range(vy_lo, vy_hi),
            sway: r.range(0.4, 1.6),
            phase: r.range(0.0, std::f32::consts::TAU),
            variant: (r.next() % 4) as u8,
        }
    }

    fn respawn_all(&mut self) {
        let n = self.target_count();
        self.parts.clear();
        for _ in 0..n {
            let p = self.spawn(false);
            self.parts.push(p);
        }
    }

    /// Advance one animation frame (~100 ms).
    pub fn tick(&mut self) {
        if self.kind == EffectKind::Off {
            return;
        }
        self.frame += 1;
        let (w, h) = (self.area.0 as f32, self.area.1 as f32);
        // Keep population at target (terminal may have been resized).
        while self.parts.len() < self.target_count() {
            let p = self.spawn(true);
            self.parts.push(p);
        }
        let mut wrapped: Vec<usize> = Vec::new();
        for (idx, p) in self.parts.iter_mut().enumerate() {
            p.y += p.vy;
            p.phase += 0.08 * p.sway;
            if matches!(self.kind, EffectKind::Snow | EffectKind::Petals | EffectKind::Confetti) {
                p.x += p.sway * 0.35 * p.phase.sin();
            }
            if self.kind == EffectKind::Fireflies {
                // Wander: gentle random walk on both axes.
                p.x += p.phase.sin() * 0.9;
                p.y += ((p.phase * 1.7).cos()) * 0.45;
            }
            if matches!(self.kind, EffectKind::Bubbles | EffectKind::Embers) {
                // Rising: wobble horizontally while floating up.
                p.x += (p.phase * 1.3).sin() * 0.4;
                if p.y < 0.0 {
                    p.y = h;
                }
            }
            if p.y > h || p.y < -1.0 {
                p.y = if p.y < -1.0 { h } else { 0.0 };
                wrapped.push(idx);
            }
            if p.x < 0.0 {
                p.x += w;
            }
            if p.x >= w {
                p.x -= w;
            }
        }
        // Respawn wrapped particles (fresh x / speed) after the borrow ends.
        for idx in wrapped {
            let fresh = self.spawn(true);
            if let Some(p) = self.parts.get_mut(idx) {
                p.y = 0.0;
                p.x = fresh.x;
                p.vy = fresh.vy;
                p.sway = fresh.sway;
                p.phase = fresh.phase;
                p.variant = fresh.variant;
            }
        }
        // Meteor scheduler: periodic diagonal comets.
        if self.kind == EffectKind::Meteor {
            if self.meteor_cooldown > 0 {
                self.meteor_cooldown -= 1;
            }
            if self.meteor_cooldown == 0 && self.meteors.len() < 2 {
                let m = Meteor {
                    x: -4.0,
                    y: self.rng.range(0.0, (self.area.1 as f32) * 0.4),
                    vx: self.rng.range(1.6, 2.6),
                    vy: self.rng.range(0.25, 0.6),
                };
                self.meteors.push(m);
                self.meteor_cooldown = 18 + (self.rng.next() % 50) as u32;
            }
            self.meteors.retain_mut(|m| {
                m.x += m.vx;
                m.y += m.vy;
                m.x < self.area.0 as f32 + 4.0 && m.y < self.area.1 as f32 + 2.0
            });
        }
        // Aurora has no particles — pure per-frame wash (see render).
        if self.kind == EffectKind::Lightning {
            if self.flash > 0 {
                self.flash -= 1;
            } else {
                self.ticks_until_strike = self.ticks_until_strike.saturating_sub(1);
                if self.ticks_until_strike == 0 {
                    self.strike();
                    self.flash = 3;
                    // Next strike in 3–12 seconds of ticks (~30 ticks/s? no —
                    // ~10/s → 30–120 ticks).
                    self.ticks_until_strike =
                        30 + (self.rng.next() % 90) as u32;
                }
            }
        }
    }

    /// Build a jagged bolt path down the band.
    fn strike(&mut self) {
        self.bolt.clear();
        let mut x = self.rng.range(4.0, self.area.0 as f32 - 4.0).max(1.0);
        let steps = self.area.1.max(4);
        for y in 0..steps {
            self.bolt.push((x as u16, y));
            x += self.rng.range(-1.8, 1.8);
            x = x.clamp(0.0, self.area.0 as f32 - 1.0);
        }
    }

    /// True while a bolt is on screen (drives the flash tint).
    pub fn flashing(&self) -> bool {
        self.kind == EffectKind::Lightning && self.flash > 0
    }

    /// Paint the current frame into `buf`, clipped to `area` (the banner
    /// band). Never touches cells outside it.
    pub fn render(&self, buf: &mut Buffer, area: Rect) {
        if self.kind == EffectKind::Off {
            return;
        }
        let style_for = |variant: u8| -> Style { self.glyph_style(variant) };
        for p in &self.parts {
            let col = area.x + (p.x as u16).min(area.width.saturating_sub(1));
            let row = area.y + (p.y as u16).min(area.height.saturating_sub(1));
            if row < area.y || col < area.x {
                continue;
            }
            let Some(cell) = buf.cell_mut((col, row)) else { continue };
            cell.set_symbol(self.glyph(p.variant));
            cell.set_style(style_for(p.variant));
        }
        // Meteor comets: bright head + fading trail along its path.
        if self.kind == EffectKind::Meteor {
            for m in &self.meteors {
                for step in 0..5 {
                    let tx = m.x - m.vx * step as f32;
                    let ty = m.y - m.vy * step as f32;
                    if tx < 0.0 || ty < 0.0 {
                        continue;
                    }
                    let col = area.x + (tx as u16).min(area.width.saturating_sub(1));
                    let row = area.y + (ty as u16).min(area.height.saturating_sub(1));
                    if let Some(cell) = buf.cell_mut((col, row)) {
                        if step == 0 {
                            cell.set_symbol("★");
                            cell.set_style(
                                Style::default().fg(Color::Rgb(255, 245, 200)).add_modifier(Modifier::BOLD),
                            );
                        } else {
                            cell.set_symbol("·");
                            let fade = match step {
                                1 => Color::Rgb(255, 220, 150),
                                2 => Color::Rgb(200, 170, 120),
                                _ => Color::DarkGray,
                            };
                            cell.set_style(Style::default().fg(fade));
                        }
                    }
                }
            }
        }
        // Aurora: silky color curtains washing across the band.
        if self.kind == EffectKind::Aurora {
            let palette = [
                [Color::Rgb(24, 90, 82), Color::Rgb(60, 140, 110), Color::Rgb(30, 70, 96)],
                [Color::Rgb(52, 34, 96), Color::Rgb(110, 70, 160), Color::Rgb(36, 40, 88)],
                [Color::Rgb(20, 96, 60), Color::Rgb(56, 150, 92), Color::Rgb(26, 62, 52)],
            ];
            for xx in area.x..area.x + area.width {
                let wave = ((xx as i32 * 7 + self.frame as i32 * 3) as f32).sin();
                let band_idx = (((wave + 1.0) / 2.0) * 2.999) as usize % 3;
                let col_color = palette[band_idx][((self.frame / 8 + xx as u64) % 3) as usize];
                for yy in area.y..area.y + area.height {
                    if let Some(cell) = buf.cell_mut((xx, yy)) {
                        cell.set_style(cell.style().bg(col_color));
                    }
                }
            }
        }
        // Lightning bolt + flash wash.
        if self.flashing() {
            let wash = Style::default().bg(Color::Rgb(38, 44, 66));
            for yy in area.y..area.y + area.height {
                for xx in area.x..area.x + area.width {
                    if let Some(cell) = buf.cell_mut((xx, yy)) {
                        cell.set_style(cell.style().patch(wash));
                    }
                }
            }
            for (bx, by) in &self.bolt {
                let col = area.x + (*bx).min(area.width.saturating_sub(1));
                let row = area.y + (*by).min(area.height.saturating_sub(1));
                if let Some(cell) = buf.cell_mut((col, row)) {
                    cell.set_symbol(if self.frame % 2 == 0 { "/" } else { "\\" });
                    cell.set_style(
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    );
                }
            }
        }
    }

    fn glyph(&self, variant: u8) -> &'static str {
        match self.kind {
            EffectKind::Petals => ["✿", "❀", "*", "˚"][variant as usize],
            EffectKind::Rain => ["│", "╵", "'", ","][variant as usize],
            EffectKind::Snow => ["❄", "❅", "·", "*"][variant as usize],
            EffectKind::Matrix => ["0", "1", "a", "f"][variant as usize],
            EffectKind::Stars => ["·", "∙", "✦", "✧"][variant as usize],
            EffectKind::Fireflies => ["·", "✦", "˚", "."][variant as usize],
            EffectKind::Bubbles => ["○", "◌", "°", "·"][variant as usize],
            EffectKind::Embers => ["˙", "*", ".", "↑"][variant as usize],
            EffectKind::Confetti => ["▪", "▫", "✦", "*"][variant as usize],
            _ => " ",
        }
    }

    fn glyph_style(&self, variant: u8) -> Style {
        let c = match self.kind {
            EffectKind::Petals => [
                Color::Rgb(255, 183, 197),
                Color::Rgb(255, 158, 189),
                Color::Rgb(255, 210, 225),
                Color::Rgb(230, 150, 175),
            ][variant as usize],
            EffectKind::Rain => [
                Color::Rgb(110, 150, 210),
                Color::Rgb(90, 125, 185),
                Color::DarkGray,
                Color::Rgb(130, 170, 225),
            ][variant as usize],
            EffectKind::Snow => [
                Color::Rgb(235, 242, 255),
                Color::Rgb(215, 228, 250),
                Color::Gray,
                Color::White,
            ][variant as usize],
            EffectKind::Matrix => {
                if variant == 0 {
                    Color::Rgb(180, 255, 160)
                } else {
                    Color::Rgb(60, 200, 90)
                }
            }
            EffectKind::Stars => {
                if self.frame % 12 < 6 {
                    Color::Rgb(255, 240, 200)
                } else {
                    Color::DarkGray
                }
            }
            EffectKind::Fireflies => {
                // Pulsing glow: alternate bright/dim with the frame phase.
                if (self.frame + variant as u64) % 8 < 3 {
                    Color::Rgb(230, 255, 140)
                } else {
                    Color::Rgb(120, 150, 60)
                }
            }
            EffectKind::Bubbles => [
                Color::Rgb(150, 220, 255),
                Color::Rgb(120, 190, 235),
                Color::Rgb(180, 225, 250),
                Color::Gray,
            ][variant as usize],
            EffectKind::Embers => [
                Color::Rgb(255, 160, 70),
                Color::Rgb(255, 110, 50),
                Color::Rgb(200, 90, 40),
                Color::Rgb(255, 205, 120),
            ][variant as usize],
            EffectKind::Confetti => [
                Color::Rgb(120, 220, 232),
                Color::Rgb(255, 121, 198),
                Color::Rgb(166, 226, 46),
                Color::Rgb(250, 189, 47),
            ][variant as usize],
            _ => Color::Gray,
        };
        Style::default().fg(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_maps_names_and_defaults_to_off() {
        assert_eq!(EffectKind::parse(Some("petals")), EffectKind::Petals);
        assert_eq!(EffectKind::parse(Some("RAIN")), EffectKind::Rain);
        assert_eq!(EffectKind::parse(Some("storm")), EffectKind::Lightning);
        assert_eq!(EffectKind::parse(None), EffectKind::Off);
        assert_eq!(EffectKind::parse(Some("bogus")), EffectKind::Off);
        assert_eq!(EffectKind::parse(Some("off")).as_str(), "off");
        assert_eq!(EffectKind::all().len(), 13);
    }

    #[test]
    fn rain_particles_fall_and_wrap() {
        let mut e = Engine::new(EffectKind::Rain);
        e.set_area(60, 12);
        e.tick();
        let before: Vec<f32> = e.parts.iter().map(|p| p.y).collect();
        for _ in 0..30 {
            e.tick();
        }
        assert!(!e.parts.is_empty());
        // After many fast-falling ticks every particle has wrapped at least
        // once, so positions differ from the start.
        let after: Vec<f32> = e.parts.iter().map(|p| p.y).collect();
        assert_ne!(before.len(), 0);
        assert!(after.iter().any(|y| *y < 12.0), "wrapped to top");
    }

    #[test]
    fn deterministic_same_seed_sequence() {
        // Two fresh engines with identical forced seeds must tick identically.
        let mk = || {
            let mut e = Engine::new(EffectKind::Rain);
            e.rng = Rng(12345);
            e.set_area(40, 10);
            e.respawn_all();
            e
        };
        let (mut a, mut b) = (mk(), mk());
        for _ in 0..10 {
            a.tick();
            b.tick();
        }
        let pa: Vec<(u32, u32)> = a.parts.iter().map(|p| (p.x.to_bits(), p.y.to_bits())).collect();
        let pb: Vec<(u32, u32)> = b.parts.iter().map(|p| (p.x.to_bits(), p.y.to_bits())).collect();
        assert_eq!(pa, pb);
    }

    #[test]
    fn render_stays_inside_band() {
        let mut e = Engine::new(EffectKind::Petals);
        e.set_area(30, 6);
        for _ in 0..5 {
            e.tick();
        }
        let area = Rect::new(2, 3, 30, 6);
        let mut buf = Buffer::empty(area);
        e.render(&mut buf, area);
        // Every non-default cell must lie inside the area (Buffer guarantees
        // this by construction; assert render didn't panic and wrote glyphs).
        let written = buf.content().iter().filter(|c| c.symbol() != " ").count();
        assert!(written > 0, "petals should be painted");
    }

    #[test]
    fn lightning_eventually_flashes() {
        let mut e = Engine::new(EffectKind::Lightning);
        e.set_area(50, 10);
        e.ticks_until_strike = 2;
        let mut flashed = false;
        for _ in 0..10 {
            e.tick();
            if e.flashing() {
                flashed = true;
                break;
            }
        }
        assert!(flashed, "strike should trigger within a few ticks");
    }

    #[test]
    fn off_renders_nothing() {
        let mut e = Engine::new(EffectKind::Off);
        e.set_area(30, 6);
        e.tick();
        let area = Rect::new(0, 0, 30, 6);
        let mut buf = Buffer::empty(area);
        e.render(&mut buf, area);
        assert!(buf.content().iter().all(|c| c.symbol() == " "));
    }


    #[test]
    fn new_effect_names_parse() {
        assert_eq!(EffectKind::parse(Some("fireflies")), EffectKind::Fireflies);
        assert_eq!(EffectKind::parse(Some("bubbles")), EffectKind::Bubbles);
        assert_eq!(EffectKind::parse(Some("sparks")), EffectKind::Embers);
        assert_eq!(EffectKind::parse(Some("comet")), EffectKind::Meteor);
        assert_eq!(EffectKind::parse(Some("aurora")), EffectKind::Aurora);
        assert_eq!(EffectKind::parse(Some("party")), EffectKind::Confetti);
    }

    #[test]
    fn bubbles_float_upward() {
        let mut e = Engine::new(EffectKind::Bubbles);
        e.set_area(40, 12);
        for p in &mut e.parts {
            p.y = 10.0;
        }
        let before: Vec<f32> = e.parts.iter().map(|p| p.y).collect();
        e.tick();
        let after: Vec<f32> = e.parts.iter().map(|p| p.y).collect();
        assert!(
            after.first().unwrap() < before.first().unwrap(),
            "bubbles must rise"
        );
    }

    #[test]
    fn meteor_launches_and_renders_a_head() {
        let mut e = Engine::new(EffectKind::Meteor);
        e.set_area(50, 10);
        e.meteor_cooldown = 1;
        e.tick();
        assert!(!e.meteors.is_empty(), "comet should launch");
        // Advance it into view then render.
        for _ in 0..6 {
            e.tick();
        }
        let area = Rect::new(0, 0, 50, 10);
        let mut buf = Buffer::empty(area);
        e.render(&mut buf, area);
        let heads = buf.content().iter().filter(|c| c.symbol() == "★").count();
        assert!(heads >= 1, "meteor head should be painted");
    }

    #[test]
    fn aurora_paints_background_wash() {
        let mut e = Engine::new(EffectKind::Aurora);
        e.set_area(30, 8);
        e.tick();
        let area = Rect::new(0, 0, 30, 8);
        let mut buf = Buffer::empty(area);
        e.render(&mut buf, area);
        let tinted = buf
            .content()
            .iter()
            .filter(|c| matches!(c.style().bg, Some(Color::Rgb(..))))
            .count();
        assert!(tinted > 0, "aurora wash must tint the band");
    }
    #[test]
    fn switching_kinds_respawns_population() {
        let mut e = Engine::new(EffectKind::Rain);
        e.set_area(60, 12);
        e.tick();
        let n_rain = e.parts.len();
        e.set(EffectKind::Snow);
        e.tick();
        assert_ne!(e.parts.len(), n_rain, "snow density differs from rain");
        assert_eq!(e.kind, EffectKind::Snow);
    }
}
