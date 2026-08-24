//! UI themes — one struct describing every palette slot, six built-ins,
//! swappable at runtime via `/theme`.

use ratatui::style::Color;

/// Every color the TUI, markdown renderer and syntax highlighter use.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Theme {
    pub name: &'static str,
    /// Primary accent (tool-ok, edit marks, composer focus).
    pub accent: Color,
    /// Secondary accent (file headers, tool-call dot).
    pub accent2: Color,
    pub heading: Color,
    pub bullet: Color,
    pub text: Color,
    pub dim: Color,
    pub gray: Color,
    pub success: Color,
    pub error: Color,
    pub warning: Color,
    pub user: Color,
    pub mode_plan: Color,
    pub mode_build: Color,
    pub mode_full: Color,
    pub code_fg: Color,
    pub code_bg: Color,
    pub rule: Color,
    pub add_bg: Color,
    pub del_bg: Color,
    // Syntax tokens.
    pub kw: Color,
    pub string: Color,
    pub comment: Color,
    pub num: Color,
    pub ty: Color,
    pub func: Color,
    pub mac: Color,
    pub op: Color,
    /// Banner gradient stops: top / middle / bottom.
    pub banner: [Color; 3],
}

const fn rgb(r: u8, g: u8, b: u8) -> Color {
    Color::Rgb(r, g, b)
}

/// Current look — greens flowing into cyan/blue.
pub static LAUDA: Theme = Theme {
    name: "lauda",
    accent: Color::LightGreen,
    accent2: Color::Cyan,
    heading: Color::LightBlue,
    bullet: Color::LightMagenta,
    text: Color::White,
    dim: Color::DarkGray,
    gray: Color::Gray,
    success: Color::LightGreen,
    error: Color::LightRed,
    warning: Color::LightYellow,
    user: Color::LightCyan,
    mode_plan: Color::LightBlue,
    mode_build: Color::LightGreen,
    mode_full: Color::Yellow,
    code_fg: Color::LightYellow,
    code_bg: rgb(35, 35, 50),
    rule: rgb(95, 95, 120),
    add_bg: rgb(24, 48, 30),
    del_bg: rgb(56, 26, 30),
    kw: Color::LightMagenta,
    string: Color::LightGreen,
    comment: Color::DarkGray,
    num: Color::LightYellow,
    ty: Color::Yellow,
    func: Color::LightBlue,
    mac: Color::LightRed,
    op: Color::Cyan,
    banner: [Color::LightGreen, Color::LightCyan, Color::Blue],
};

/// Sakura — soft rose and plum, made for the petals effect.
pub static CHERRY: Theme = Theme {
    name: "cherry",
    accent: rgb(255, 158, 189),
    accent2: rgb(255, 199, 216),
    heading: rgb(255, 183, 214),
    bullet: rgb(255, 145, 175),
    text: rgb(255, 236, 242),
    dim: rgb(120, 90, 105),
    gray: rgb(190, 155, 170),
    success: rgb(160, 235, 180),
    error: rgb(255, 110, 130),
    warning: rgb(255, 205, 140),
    user: rgb(255, 210, 226),
    mode_plan: rgb(198, 166, 255),
    mode_build: rgb(255, 158, 189),
    mode_full: rgb(255, 196, 120),
    code_fg: rgb(255, 200, 220),
    code_bg: rgb(48, 26, 38),
    rule: rgb(120, 70, 92),
    add_bg: rgb(52, 28, 40),
    del_bg: rgb(70, 28, 36),
    kw: rgb(255, 121, 198),
    string: rgb(195, 232, 141),
    comment: rgb(108, 88, 100),
    num: rgb(247, 190, 120),
    ty: rgb(255, 214, 153),
    func: rgb(173, 205, 255),
    mac: rgb(255, 158, 120),
    op: rgb(255, 173, 205),
    banner: [rgb(255, 158, 189), rgb(255, 199, 216), rgb(186, 142, 255)],
};

/// Deep blue night sky.
pub static MIDNIGHT: Theme = Theme {
    name: "midnight",
    accent: rgb(126, 192, 255),
    accent2: rgb(112, 214, 255),
    heading: rgb(148, 176, 255),
    bullet: rgb(171, 157, 242),
    text: rgb(220, 228, 240),
    dim: rgb(90, 100, 125),
    gray: rgb(150, 162, 185),
    success: rgb(134, 236, 172),
    error: rgb(255, 128, 128),
    warning: rgb(255, 209, 130),
    user: rgb(140, 225, 255),
    mode_plan: rgb(148, 176, 255),
    mode_build: rgb(126, 222, 168),
    mode_full: rgb(255, 209, 130),
    code_fg: rgb(196, 214, 240),
    code_bg: rgb(18, 24, 44),
    rule: rgb(60, 72, 105),
    add_bg: rgb(20, 46, 34),
    del_bg: rgb(56, 26, 32),
    kw: rgb(198, 160, 255),
    string: rgb(134, 236, 172),
    comment: rgb(90, 100, 125),
    num: rgb(255, 213, 128),
    ty: rgb(126, 222, 234),
    func: rgb(126, 178, 255),
    mac: rgb(255, 145, 165),
    op: rgb(112, 214, 255),
    banner: [rgb(90, 130, 255), rgb(126, 192, 255), rgb(60, 68, 130)],
};

/// Cool blue-slate nordic palette.
pub static NORD: Theme = Theme {
    name: "nord",
    accent: rgb(136, 192, 208),
    accent2: rgb(129, 161, 193),
    heading: rgb(129, 161, 193),
    bullet: rgb(180, 142, 173),
    text: rgb(236, 239, 244),
    dim: rgb(76, 86, 106),
    gray: rgb(143, 152, 168),
    success: rgb(163, 190, 140),
    error: rgb(191, 97, 106),
    warning: rgb(235, 203, 139),
    user: rgb(136, 192, 208),
    mode_plan: rgb(129, 161, 193),
    mode_build: rgb(163, 190, 140),
    mode_full: rgb(235, 203, 139),
    code_fg: rgb(216, 222, 233),
    code_bg: rgb(46, 52, 64),
    rule: rgb(76, 86, 106),
    add_bg: rgb(46, 62, 50),
    del_bg: rgb(66, 42, 46),
    kw: rgb(129, 161, 193),
    string: rgb(163, 190, 140),
    comment: rgb(96, 106, 126),
    num: rgb(208, 135, 112),
    ty: rgb(235, 203, 139),
    func: rgb(136, 192, 208),
    mac: rgb(191, 97, 106),
    op: rgb(136, 192, 208),
    banner: [rgb(163, 190, 140), rgb(136, 192, 208), rgb(94, 129, 172)],
};

/// Purple-forward classic.
pub static DRACULA: Theme = Theme {
    name: "dracula",
    accent: rgb(189, 147, 249),
    accent2: rgb(139, 233, 253),
    heading: rgb(255, 184, 108),
    bullet: rgb(255, 121, 198),
    text: rgb(248, 248, 242),
    dim: rgb(98, 114, 164),
    gray: rgb(139, 143, 177),
    success: rgb(80, 250, 123),
    error: rgb(255, 85, 85),
    warning: rgb(241, 250, 140),
    user: rgb(139, 233, 253),
    mode_plan: rgb(189, 147, 249),
    mode_build: rgb(80, 250, 123),
    mode_full: rgb(241, 250, 140),
    code_fg: rgb(248, 248, 242),
    code_bg: rgb(40, 42, 54),
    rule: rgb(68, 71, 90),
    add_bg: rgb(38, 54, 44),
    del_bg: rgb(58, 38, 44),
    kw: rgb(255, 121, 198),
    string: rgb(241, 250, 140),
    comment: rgb(98, 114, 164),
    num: rgb(189, 147, 249),
    ty: rgb(139, 233, 253),
    func: rgb(80, 250, 123),
    mac: rgb(255, 85, 85),
    op: rgb(255, 184, 108),
    banner: [rgb(255, 121, 198), rgb(189, 147, 249), rgb(139, 233, 253)],
};

/// High-contrast retro warm.
pub static MONOKAI: Theme = Theme {
    name: "monokai",
    accent: rgb(166, 226, 46),
    accent2: rgb(102, 217, 239),
    heading: rgb(253, 151, 31),
    bullet: rgb(249, 38, 114),
    text: rgb(248, 248, 240),
    dim: rgb(117, 113, 94),
    gray: rgb(150, 145, 130),
    success: rgb(166, 226, 46),
    error: rgb(249, 38, 114),
    warning: rgb(230, 219, 116),
    user: rgb(102, 217, 239),
    mode_plan: rgb(102, 217, 239),
    mode_build: rgb(166, 226, 46),
    mode_full: rgb(230, 219, 116),
    code_fg: rgb(248, 248, 240),
    code_bg: rgb(39, 40, 34),
    rule: rgb(90, 89, 78),
    add_bg: rgb(48, 54, 34),
    del_bg: rgb(64, 32, 40),
    kw: rgb(249, 38, 114),
    string: rgb(230, 219, 116),
    comment: rgb(117, 113, 94),
    num: rgb(174, 129, 255),
    ty: rgb(102, 217, 239),
    func: rgb(166, 226, 46),
    mac: rgb(253, 151, 31),
    op: rgb(253, 151, 31),
    banner: [rgb(249, 38, 114), rgb(253, 151, 31), rgb(166, 226, 46)],
};

/// Warm sun-baked classic.
pub static SOLARIZED: Theme = Theme {
    name: "solarized",
    accent: rgb(181, 137, 0),
    accent2: rgb(38, 139, 210),
    heading: rgb(42, 161, 152),
    bullet: rgb(211, 54, 130),
    text: rgb(238, 232, 213),
    dim: rgb(88, 110, 117),
    gray: rgb(147, 161, 161),
    success: rgb(133, 153, 0),
    error: rgb(220, 50, 47),
    warning: rgb(203, 75, 22),
    user: rgb(38, 139, 210),
    mode_plan: rgb(38, 139, 210),
    mode_build: rgb(133, 153, 0),
    mode_full: rgb(203, 75, 22),
    code_fg: rgb(238, 232, 213),
    code_bg: rgb(0, 43, 54),
    rule: rgb(88, 110, 117),
    add_bg: rgb(30, 52, 38),
    del_bg: rgb(62, 32, 30),
    kw: rgb(133, 153, 0),
    string: rgb(42, 161, 152),
    comment: rgb(88, 110, 117),
    num: rgb(211, 54, 130),
    ty: rgb(181, 137, 0),
    func: rgb(38, 139, 210),
    mac: rgb(220, 50, 47),
    op: rgb(147, 161, 161),
    banner: [rgb(181, 137, 0), rgb(42, 161, 152), rgb(38, 139, 210)],
};

/// Retro-groove warm browns and neons.
pub static GRUVBOX: Theme = Theme {
    name: "gruvbox",
    accent: rgb(184, 187, 38),
    accent2: rgb(142, 192, 124),
    heading: rgb(254, 128, 25),
    bullet: rgb(211, 134, 155),
    text: rgb(235, 219, 178),
    dim: rgb(146, 131, 116),
    gray: rgb(168, 153, 132),
    success: rgb(152, 151, 26),
    error: rgb(251, 73, 52),
    warning: rgb(250, 189, 47),
    user: rgb(131, 165, 152),
    mode_plan: rgb(131, 165, 152),
    mode_build: rgb(184, 187, 38),
    mode_full: rgb(250, 189, 47),
    code_fg: rgb(235, 219, 178),
    code_bg: rgb(40, 40, 40),
    rule: rgb(124, 111, 100),
    add_bg: rgb(48, 52, 34),
    del_bg: rgb(66, 36, 32),
    kw: rgb(251, 73, 52),
    string: rgb(184, 187, 38),
    comment: rgb(146, 131, 116),
    num: rgb(254, 128, 25),
    ty: rgb(250, 189, 47),
    func: rgb(184, 187, 38),
    mac: rgb(211, 134, 155),
    op: rgb(142, 192, 124),
    banner: [rgb(250, 189, 47), rgb(184, 187, 38), rgb(131, 165, 152)],
};

/// Neon dusk city lights.
pub static TOKYO: Theme = Theme {
    name: "tokyo",
    accent: rgb(122, 162, 247),
    accent2: rgb(125, 207, 255),
    heading: rgb(187, 154, 247),
    bullet: rgb(255, 123, 173),
    text: rgb(220, 224, 240),
    dim: rgb(90, 100, 145),
    gray: rgb(150, 160, 200),
    success: rgb(115, 218, 202),
    error: rgb(247, 118, 142),
    warning: rgb(224, 175, 104),
    user: rgb(125, 207, 255),
    mode_plan: rgb(187, 154, 247),
    mode_build: rgb(115, 218, 202),
    mode_full: rgb(224, 175, 104),
    code_fg: rgb(220, 224, 240),
    code_bg: rgb(26, 30, 56),
    rule: rgb(70, 78, 120),
    add_bg: rgb(28, 50, 48),
    del_bg: rgb(58, 32, 46),
    kw: rgb(187, 154, 247),
    string: rgb(158, 206, 106),
    comment: rgb(90, 100, 145),
    num: rgb(255, 158, 100),
    ty: rgb(45, 219, 178),
    func: rgb(122, 162, 247),
    mac: rgb(247, 118, 142),
    op: rgb(125, 207, 255),
    banner: [rgb(255, 123, 173), rgb(187, 154, 247), rgb(122, 162, 247)],
};

/// Muted deep-forest greens.
pub static EVERFOREST: Theme = Theme {
    name: "everforest",
    accent: rgb(167, 192, 128),
    accent2: rgb(115, 170, 140),
    heading: rgb(215, 153, 33),
    bullet: rgb(214, 153, 164),
    text: rgb(211, 216, 190),
    dim: rgb(101, 109, 87),
    gray: rgb(150, 156, 140),
    success: rgb(167, 192, 128),
    error: rgb(228, 44, 44),
    warning: rgb(219, 157, 61),
    user: rgb(115, 170, 140),
    mode_plan: rgb(115, 170, 140),
    mode_build: rgb(167, 192, 128),
    mode_full: rgb(219, 157, 61),
    code_fg: rgb(211, 216, 190),
    code_bg: rgb(43, 48, 45),
    rule: rgb(90, 99, 88),
    add_bg: rgb(46, 58, 44),
    del_bg: rgb(64, 40, 38),
    kw: rgb(228, 44, 44),
    string: rgb(167, 192, 128),
    comment: rgb(101, 109, 87),
    num: rgb(219, 157, 61),
    ty: rgb(115, 170, 140),
    func: rgb(167, 192, 128),
    mac: rgb(214, 153, 164),
    op: rgb(115, 170, 140),
    banner: [rgb(215, 153, 33), rgb(167, 192, 128), rgb(115, 170, 140)],
};

/// Glowing coal-bed reds and oranges.
pub static EMBER: Theme = Theme {
    name: "ember",
    accent: rgb(255, 149, 60),
    accent2: rgb(255, 110, 64),
    heading: rgb(255, 190, 100),
    bullet: rgb(255, 120, 90),
    text: rgb(245, 230, 220),
    dim: rgb(130, 85, 65),
    gray: rgb(195, 150, 125),
    success: rgb(255, 190, 100),
    error: rgb(255, 80, 60),
    warning: rgb(255, 149, 60),
    user: rgb(255, 170, 120),
    mode_plan: rgb(255, 170, 120),
    mode_build: rgb(255, 149, 60),
    mode_full: rgb(255, 210, 130),
    code_fg: rgb(255, 214, 180),
    code_bg: rgb(52, 30, 22),
    rule: rgb(120, 70, 50),
    add_bg: rgb(56, 40, 26),
    del_bg: rgb(64, 26, 22),
    kw: rgb(255, 110, 64),
    string: rgb(255, 190, 100),
    comment: rgb(130, 85, 65),
    num: rgb(255, 210, 130),
    ty: rgb(255, 149, 60),
    func: rgb(255, 190, 100),
    mac: rgb(255, 80, 60),
    op: rgb(255, 170, 120),
    banner: [rgb(255, 210, 130), rgb(255, 149, 60), rgb(200, 60, 40)],
};

/// Pale arctic blues and whites.
pub static ICE: Theme = Theme {
    name: "ice",
    accent: rgb(160, 214, 255),
    accent2: rgb(198, 231, 255),
    heading: rgb(190, 225, 255),
    bullet: rgb(170, 205, 240),
    text: rgb(238, 246, 255),
    dim: rgb(105, 125, 150),
    gray: rgb(170, 190, 210),
    success: rgb(180, 235, 220),
    error: rgb(255, 140, 150),
    warning: rgb(230, 240, 180),
    user: rgb(198, 231, 255),
    mode_plan: rgb(190, 225, 255),
    mode_build: rgb(180, 235, 220),
    mode_full: rgb(230, 240, 180),
    code_fg: rgb(222, 238, 255),
    code_bg: rgb(24, 36, 50),
    rule: rgb(80, 100, 125),
    add_bg: rgb(30, 52, 58),
    del_bg: rgb(58, 34, 42),
    kw: rgb(150, 200, 255),
    string: rgb(185, 235, 220),
    comment: rgb(105, 125, 150),
    num: rgb(230, 240, 180),
    ty: rgb(198, 231, 255),
    func: rgb(160, 214, 255),
    mac: rgb(255, 140, 150),
    op: rgb(198, 231, 255),
    banner: [rgb(198, 231, 255), rgb(160, 214, 255), rgb(110, 150, 200)],
};

pub static ALL: &[&Theme] = &[&LAUDA, &CHERRY, &MIDNIGHT, &NORD, &DRACULA, &MONOKAI, &SOLARIZED, &GRUVBOX, &TOKYO, &EVERFOREST, &EMBER, &ICE];
pub static DEFAULT: &Theme = &LAUDA;

pub fn names() -> Vec<&'static str> {
    ALL.iter().map(|t| t.name).collect()
}

/// Currently active theme (defaults to Lauda until [`set`] succeeds).
pub fn get() -> &'static Theme {
    ACTIVE.with(|c| *c.borrow()).unwrap_or(DEFAULT)
}

/// Switch the active theme by name. Returns false for unknown names.
pub fn set(name: &str) -> bool {
    let found = ALL.iter().find(|t| t.name.eq_ignore_ascii_case(name.trim())).copied();
    // A failed lookup must leave the current choice untouched.
    if let Some(t) = found {
        ACTIVE.with(|c| *c.borrow_mut() = Some(t));
    }
    found.is_some()
}

thread_local! {
    static ACTIVE: std::cell::RefCell<Option<&'static Theme>> =
        const { std::cell::RefCell::new(None) };
}

/// Interpolate the banner gradient stops into `rows` colors (RGB lerp).
pub fn banner_gradient(rows: usize) -> Vec<Color> {
    let [top, mid, bot] = get().banner;
    let stops = [top, mid, bot];
    let mut out = Vec::with_capacity(rows);
    for i in 0..rows {
        let pos = if rows <= 1 { 0.0 } else { i as f32 / (rows - 1) as f32 };
        let seg = (pos * 2.0).min(1.999);
        let idx = seg as usize;
        let t = seg - idx as f32;
        out.push(lerp(stops[idx], stops[idx + 1], t));
    }
    out
}

fn lerp(a: Color, b: Color, t: f32) -> Color {
    match (a, b) {
        (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) => Color::Rgb(
            (r1 as f32 + (r2 as f32 - r1 as f32) * t) as u8,
            (g1 as f32 + (g2 as f32 - g1 as f32) * t) as u8,
            (b1 as f32 + (b2 as f32 - b1 as f32) * t) as u8,
        ),
        _ => {
            // Named colors can't lerp — snap halfway.
            if t < 0.5 { a } else { b }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_themes_resolve_and_names_unique() {
        let mut seen = std::collections::HashSet::new();
        for t in ALL {
            assert!(seen.insert(t.name), "duplicate theme {}", t.name);
            assert!(set(t.name));
            assert_eq!(get().name, t.name);
        }
        assert_eq!(names().len(), ALL.len());
    }

    #[test]
    fn unknown_theme_falls_back_to_default_without_swapping() {
        set("cherry");
        assert!(!set("nope"));
        assert_eq!(get().name, "cherry", "failed set must not change theme");
        set("lauda");
        assert!(!get().name.is_empty());
    }

    #[test]
    fn gradient_has_one_entry_per_row() {
        set("lauda");
        assert_eq!(banner_gradient(13).len(), 13);
        assert_eq!(banner_gradient(6).len(), 6);
    }

    #[test]
    fn themes_differ_in_syntax_palette() {
        set("lauda");
        let lauda_kw = get().kw;
        set("dracula");
        let dracula_kw = get().kw;
        assert_ne!(lauda_kw, dracula_kw);
        set("lauda");
    }
}
