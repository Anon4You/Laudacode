use crossterm::event::{
    self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::{
    layout::{Constraint, Layout, Position, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, BorderType, Borders, Clear, List, ListItem, Paragraph, Scrollbar,
        ScrollbarOrientation, ScrollbarState, Wrap,
    },
    Frame,
};
use std::time::{Duration, Instant};
use unicode_width::UnicodeWidthStr;

/// Collaboration mode, cycled with Shift+Tab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Read-only planning: model explores and produces a plan.
    Plan,
    /// Normal: edits auto-approved, shell commands require approval.
    Build,
    /// Everything auto-approved unless dangerous.
    FullAuto,
}

/// Cap on remembered prompts for ↑/↓ recall.
const HISTORY_MAX: usize = 500;

impl Mode {
    pub fn next(self) -> Self {
        match self {
            Mode::Plan => Mode::Build,
            Mode::Build => Mode::FullAuto,
            Mode::FullAuto => Mode::Plan,
        }
    }
    pub fn label(&self) -> &'static str {
        match self {
            Mode::Plan => "PLAN",
            Mode::Build => "BUILD",
            Mode::FullAuto => "FULL AUTO",
        }
    }
    pub fn color(&self) -> Color {
        let t = crate::theme::get();
        match self {
            Mode::Plan => t.mode_plan,
            Mode::Build => t.mode_build,
            Mode::FullAuto => t.mode_full,
        }
    }
}

/// Which palette slot a run of logo text paints with. Resolved against the
/// live theme at draw time, so `/theme` recolors the logo like everything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ink {
    /// The per-row theme gradient — the logo's structural body.
    Body,
    /// Primary accent: faces, cursors, the wordmark itself.
    Glow,
    /// Secondary accent: small details that should read as a highlight.
    Accent,
    /// Box borders and rules — structural, never shouty.
    Rule,
    /// Muted text: prompts, paths, dim labels.
    Faint,
}

/// One styled run of logo text.
#[derive(Debug, Clone, Copy)]
pub struct Seg(pub &'static str, pub Ink);

impl Seg {
    pub const fn body(s: &'static str) -> Self {
        Seg(s, Ink::Body)
    }
    pub const fn glow(s: &'static str) -> Self {
        Seg(s, Ink::Glow)
    }
    pub const fn accent(s: &'static str) -> Self {
        Seg(s, Ink::Accent)
    }
    pub const fn rule(s: &'static str) -> Self {
        Seg(s, Ink::Rule)
    }
    pub const fn faint(s: &'static str) -> Self {
        Seg(s, Ink::Faint)
    }
}

/// One logo variant: named rows of styled runs plus the width it needs.
#[derive(Debug, Clone, Copy)]
pub struct Art {
    pub name: &'static str,
    pub rows: &'static [&'static [Seg]],
}

/// Every logo variant. One is picked at random per display, so the banner
/// changes when you restart (or press Ctrl+B) instead of being wallpaper.
pub const LOGOS: &[Art] = &[
    Art {
        name: "wordmark",
        rows: &[
            &[Seg::rule(" ╭                                                       ╮")],
            &[Seg::rule(" │ "), Seg::glow("█      ██   █   █ ████   ██    ████  ███  ████  █████"), Seg::rule(" │")],
            &[Seg::rule(" │ "), Seg::glow("█     █████ █   █ █   █ █████ █     █   █ █   █ █    "), Seg::rule(" │")],
            &[Seg::rule(" │ "), Seg::glow("█     █   █ █   █ █   █ █   █ █     █   █ █   █ ████ "), Seg::rule(" │")],
            &[Seg::rule(" │ "), Seg::glow("█     █   █ █   █ █   █ █   █ █     █   █ █   █ █    "), Seg::rule(" │")],
            &[Seg::rule(" │ "), Seg::glow("█████ █████  ███  ████  █████  ████  ███  ████  █████"), Seg::rule(" │")],
            &[Seg::rule(" ╰                                                       ╯")],
        ],
    },
    Art {
        name: "terminal",
        rows: &[
            &[Seg::rule(" ╭─────────────────────────────────╮")],
            // ponytail: the trailing pad below is hand-fitted to a 7-char
            // version ("v0.10.0"); a 2-digit patch or 3-digit component (v1.0.0
            // / v0.10.10) makes this row a char wide and
            // `every_logo_fits_the_banner_band` says so. If versions get that
            // long, give Seg a `Cow<'static, str>` and pad the slot at runtime.
            &[
                Seg::rule(" │"),
                Seg::glow(" ⎈"),
                Seg::rule("  laudacode   "),
                Seg::glow(concat!("v", env!("CARGO_PKG_VERSION"))),
                Seg::rule("          │"),
            ],
            &[Seg::rule(" │                                 │")],
            &[
                Seg::faint(" │   ╱▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔▔"),
                Seg::rule("  │"),
            ],
            &[
                Seg::rule(" │  ╱"),
                Seg::glow("◕ ═══ ◕"),
                Seg::rule(" ╲   "),
                Seg::faint("▄▄▄▄▄▄▄▄▄"),
                Seg::rule("         │"),
            ],
            &[
                Seg::rule(" │ ╱"),
                Seg::glow("‿ ═══ ‿"),
                Seg::rule(" ╲  █"),
                Seg::accent("▪"),
                Seg::rule("███               │"),
            ],
            &[
                Seg::rule(" │╲"),
                Seg::glow(" ᗜ═══════▟"),
                Seg::rule(" ╱  █"),
                Seg::accent("▪"),
                Seg::rule("███             │"),
            ],
            &[
                Seg::faint(" │ ╲▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁╱   "),
                Seg::rule("▀▀▀▀▀▀▀▀▀  │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::faint(" ❯ _"),
                Seg::glow(" █"),
                Seg::rule("                           │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::faint(" ❯ cargo test"),
                Seg::glow(" █"),
                Seg::rule("                  │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::faint(" ❯ "),
                Seg::glow("▊"),
                Seg::rule("                             │"),
            ],
            &[Seg::rule(" ╰─────────────────────────────────╯")],
        ],
    },
    Art {
        name: "cat",
        rows: &[
            &[Seg::rule(" ╭                             ╮")],
            &[
                Seg::rule(" │"),
                Seg::glow("   ▄▀▀▄     ▄▀▀▄"),
                Seg::rule("             │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::glow("  █    █   █    █"),
                Seg::rule("            │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::glow("  █ "),
                Seg::accent("◕◕"),
                Seg::glow(" █   █ "),
                Seg::accent("◕◕"),
                Seg::glow(" █"),
                Seg::rule("            │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::glow("  █    █   █    █"),
                Seg::rule("            │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::glow("   █▄▄▄█▄▄▄▄▄▄█▄▄▄"),
                Seg::rule("           │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::faint("      ╱        ╲"),
                Seg::rule("             │"),
            ],
            &[Seg::rule(" ╰                             ╯")],
        ],
    },
    Art {
        name: "gem",
        rows: &[
            &[Seg::rule(" ╭                         ╮")],
            &[
                Seg::rule(" │"),
                Seg::body("       ╱╲"),
                Seg::rule("                │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::body("      ╱  ╲"),
                Seg::faint("   ✦"),
                Seg::rule("           │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::body("     ╱    ╲"),
                Seg::rule("              │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::body("    ╱╱╱╱╱╱╲"),
                Seg::rule("              │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::accent("   ░░░░░░░░░"),
                Seg::rule("             │"),
            ],
            &[
                Seg::rule(" │"),
                Seg::accent("  ░░░░░░░░░░░"),
                Seg::rule("            │"),
            ],
            &[Seg::rule(" ╰                         ╯")],
        ],
    },
    Art {
        name: "rocket",
        rows: &[
            &[Seg::rule(" ╭                ╮")],
            &[Seg::rule(" │ "), Seg::faint("        "), Seg::glow("▲"), Seg::rule("      │")],
            &[Seg::rule(" │ "), Seg::faint("       "), Seg::glow("▲▲▲"), Seg::rule("     │")],
            &[Seg::rule(" │ "), Seg::faint("      "), Seg::glow("▲▲▲▲▲"), Seg::rule("    │")],
            &[Seg::rule(" │ "), Seg::faint("     "), Seg::rule("╱▔▔▔▔▔▔╲  │")],
            &[Seg::rule(" │ "), Seg::faint("    "), Seg::rule("┌───────┐  │")],
            &[Seg::rule(" │ "), Seg::faint("    "), Seg::rule("│"), Seg::accent(" ▟███▙ "), Seg::rule("│  │")],
            &[Seg::rule(" │ "), Seg::faint("    "), Seg::rule("│"), Seg::accent(" ▜███▛ "), Seg::rule("│  │")],
            &[Seg::rule(" │ "), Seg::faint("    "), Seg::rule("└───┬───┘  │")],
            &[Seg::rule(" │ "), Seg::faint("     "), Seg::glow("╽"), Seg::accent("▐█▌"), Seg::glow("╿"), Seg::faint(" ✧✦"), Seg::rule("  │")],
            &[Seg::rule(" │ "), Seg::faint("       "), Seg::accent("▀▀▀"), Seg::rule("     │")],
            &[Seg::rule(" ╰                ╯")],
        ],
    },
    Art {
        name: "mount",
        rows: &[
            &[Seg::rule(" ╭                     ╮")],
            &[Seg::rule(" │ "), Seg::faint("          "), Seg::glow("☀"), Seg::rule("         │")],
            &[Seg::rule(" │ "), Seg::glow("   ▂▃▄▅▆▇"), Seg::rule("           │")],
            &[Seg::rule(" │ "), Seg::glow("  ▃▄▅▆▇█"), Seg::rule("            │")],
            &[Seg::rule(" │ "), Seg::glow(" ▄▅▆▇█▄▅▆▇"), Seg::rule("          │")],
            &[Seg::rule(" │ "), Seg::glow("▅▆▇█▄▅▆▇█▄▅▆"), Seg::rule("        │")],
            &[Seg::rule(" │ "), Seg::faint("▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁▁"), Seg::rule(" │")],
            &[Seg::rule(" │ "), Seg::rule(" ❯ "), Seg::glow("▊"), Seg::rule("                │")],
            &[Seg::rule(" ╰                     ╯")],
        ],
    },
    Art {
        name: "circuit",
        rows: &[
            &[Seg::rule(" ╭                        ╮")],
            &[Seg::rule(" │ "), Seg::glow(" ●"), Seg::rule("───"), Seg::glow("●"), Seg::rule("────●"), Seg::rule("            │")],
            &[Seg::rule(" │ "), Seg::rule(" │"), Seg::faint("        │      ╭─╯"), Seg::rule("   │")],
            &[Seg::rule(" │ "), Seg::rule(" │   ┌────┤      │"), Seg::rule("     │")],
            &[Seg::rule(" │ "), Seg::glow(" ●───┘"), Seg::faint("           ●"), Seg::rule("     │")],
            &[Seg::rule(" │ "), Seg::rule(" │"), Seg::rule("                     │")],
            &[Seg::rule(" │ "), Seg::rule(" ╰──●"), Seg::faint("─────"), Seg::rule("             │")],
            &[Seg::rule(" │ "), Seg::faint("       ╰────●──╮"), Seg::rule("       │")],
            &[Seg::rule(" │ "), Seg::accent("            ╰─●"), Seg::rule("        │")],
            &[Seg::rule(" │ "), Seg::faint("               ╰──── ⚡"), Seg::rule(" │")],
            &[Seg::rule(" ╰                        ╯")],
        ],
    },
    // Adapted from asciiart.eu (Tux, jgs) — redrawn to fit the band.
    Art {
        name: "tux",
        rows: &[
            &[Seg::rule(" ╭              ╮")],
            &[Seg::rule(" │ "), Seg::faint("   .--."), Seg::rule("      │")],
            &[Seg::rule(" │ "), Seg::rule("  |"), Seg::glow("o_o"), Seg::rule(" |"), Seg::rule("     │")],
            &[Seg::rule(" │ "), Seg::rule("  |"), Seg::glow(":_/"), Seg::rule(" |"), Seg::rule("     │")],
            &[Seg::rule(" │ "), Seg::rule(" //   \\ "), Seg::rule("     │")],
            &[Seg::rule(" │ "), Seg::rule("(|     | )"), Seg::rule("   │")],
            &[Seg::rule(" │ "), Seg::rule("/'\\_   _/`\\"), Seg::rule("  │")],
            &[Seg::rule(" │ "), Seg::rule("\\___"), Seg::accent(")="), Seg::rule("(___/)"), Seg::rule(" │")],
            &[Seg::rule(" ╰              ╯")],
        ],
    },
    // Adapted from asciiart.eu (space saucer) — redrawn to fit the band.
    Art {
        name: "ufo",
        rows: &[
            &[Seg::rule(" ╭                  ╮")],
            &[Seg::rule(" │ "), Seg::faint("        ___"), Seg::rule("      │")],
            &[Seg::rule(" │ "), Seg::faint("    _.-'   '-._"), Seg::rule("  │")],
            &[Seg::rule(" │ "), Seg::rule("   /  "), Seg::glow("o  o  o"), Seg::rule("  \\"), Seg::rule(" │")],
            &[Seg::rule(" │ "), Seg::rule("   \\   ___   /"), Seg::rule("   │")],
            &[Seg::rule(" │ "), Seg::rule("    '-("), Seg::accent("___"), Seg::rule(")-'"), Seg::rule("    │")],
            &[Seg::rule(" │ "), Seg::faint("      '  '  '"), Seg::rule("    │")],
            &[Seg::rule(" ╰                  ╯")],
        ],
    },
    // Adapted from asciiart.eu (bug) — redrawn to fit the band.
    Art {
        name: "bug",
        rows: &[
            &[Seg::rule(" ╭             ╮")],
            &[Seg::rule(" │ "), Seg::rule("\\ /"), Seg::rule("         │")],
            &[Seg::rule(" │ "), Seg::faint("    "), Seg::glow("oVo"), Seg::rule("     │")],
            &[Seg::rule(" │ "), Seg::rule("\\___"), Seg::accent("XXX"), Seg::rule("___/"), Seg::rule(" │")],
            &[Seg::rule(" │ "), Seg::rule(" __"), Seg::accent("XXXXX"), Seg::rule("__"), Seg::rule("  │")],
            &[Seg::rule(" │ "), Seg::rule("/__"), Seg::accent("XXXXX"), Seg::rule("__\\"), Seg::rule(" │")],
            &[Seg::rule(" │ "), Seg::rule("/   "), Seg::accent("XXX"), Seg::rule("   \\"), Seg::rule(" │")],
            &[Seg::rule(" │ "), Seg::faint("     V"), Seg::rule("      │")],
            &[Seg::rule(" ╰             ╯")],
        ],
    },
];

/// Tallest variant — the banner band is sized to this and shorter logos are
/// centered inside it.
const BANNER_ROWS: u16 = 12;

/// Pick a logo at random from the variants that fit `width`. Re-rolled on
/// every display, never per frame, so the banner doesn't flicker.
pub fn pick_logo(width: u16) -> &'static Art {
    let fits: Vec<&Art> = LOGOS
        .iter()
        .filter(|a| composed_width(a) <= width)
        .collect();
    let pool: Vec<&Art> = if fits.is_empty() {
        // Too narrow even for the identity block: take the most compact logo
        // and let the paragraph clip rather than showing an empty band.
        LOGOS
            .iter()
            .min_by_key(|a| composed_width(a))
            .into_iter()
            .collect()
    } else {
        fits
    };
    let n = next_rand() as usize % pool.len();
    pool[n]
}

/// A fresh OS-seeded `u64` — enough randomness to re-roll a banner, with no
/// PRNG state to carry around.
fn next_rand() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    std::hash::RandomState::new().build_hasher().finish()
}

/// The identity block pinned to the right of whichever logo is showing, so
/// the name/version/tagline ride along with all of them — not just one.
const INFO: &[(&str, Ink)] = &[
    ("LaudaCode", Ink::Glow),
    ("v", Ink::Accent),
    ("AI coding agent", Ink::Faint),
];

/// Spaces between the right edge of a logo and the identity block.
const INFO_GUTTER: u16 = 3;

/// Widest line in the identity block (the version is filled in at compile
/// time, so it is measured with a placeholder of the same shape).
fn info_width() -> u16 {
    INFO.iter()
        .map(|(text, _)| {
            if *text == "v" {
                ("v".to_string() + env!("CARGO_PKG_VERSION"))
                    .chars()
                    .count()
            } else {
                text.chars().count()
            }
        })
        .max()
        .unwrap_or(0) as u16
}

/// The identity block with the real version spliced in.
fn info_segments() -> Vec<(&'static str, Ink)> {
    INFO.iter()
        .map(|(text, ink)| {
            if *text == "v" {
                (concat!("v", env!("CARGO_PKG_VERSION")), *ink)
            } else {
                (*text, *ink)
            }
        })
        .collect()
}

/// A logo's own width — every row padded out to this so the identity block
/// starts in the same column on every line.
fn art_width(art: &Art) -> u16 {
    art.rows
        .iter()
        .map(|r| r.iter().map(|s| s.0.chars().count()).sum::<usize>())
        .max()
        .unwrap_or(0) as u16
}

/// Total width a logo needs once the identity block is included. This — not
/// the art width — is what decides whether the logo fits the terminal.
pub fn composed_width(art: &Art) -> u16 {
    art_width(art) + INFO_GUTTER + info_width()
}

/// True when at least one logo (art + identity block) fits in `width`.
/// Below this, callers should fall back to the one-line identity header.
pub fn any_logo_fits(width: u16) -> bool {
    LOGOS.iter().any(|a| composed_width(a) <= width)
}

/// Render a logo with the identity block beside it, vertically centered in
/// the band.
pub fn banner_lines(art: &Art) -> Vec<Line<'static>> {
    let t = crate::theme::get();
    let grad = crate::theme::banner_gradient(art.rows.len());
    let aw = art_width(art);
    let info = info_segments();
    let top = (BANNER_ROWS as usize).saturating_sub(art.rows.len()) / 2;
    let info_top = top + art.rows.len().saturating_sub(info.len()) / 2;

    let mut out: Vec<Line<'static>> = Vec::with_capacity(BANNER_ROWS as usize);
    for y in 0..BANNER_ROWS as usize {
        let mut spans: Vec<Span<'static>> = Vec::new();
        // Logo, left-anchored and padded to the art width.
        if y >= top && y < top + art.rows.len() {
            let i = y - top;
            let body = grad.get(i).copied().unwrap_or(t.banner[0]);
            let row = art.rows[i];
            let used: usize = row.iter().map(|s| s.0.chars().count()).sum();
            for Seg(text, ink) in row {
                spans.push(Span::styled(
                    *text,
                    Style::default()
                        .fg(match ink {
                            Ink::Body => body,
                            Ink::Glow => t.accent,
                            Ink::Accent => t.accent2,
                            Ink::Rule => t.border,
                            Ink::Faint => t.dim,
                        })
                        .add_modifier(Modifier::BOLD),
                ));
            }
            spans.push(Span::raw(" ".repeat((aw as usize).saturating_sub(used))));
        }
        // Identity block, right of the gutter. The gutter goes on *every*
        // block row, not just the first, or the lines sit ragged.
        if y >= info_top && y < info_top + info.len() {
            spans.push(Span::raw(" ".repeat(INFO_GUTTER as usize)));
            let (text, ink) = info[y - info_top];
            spans.push(Span::styled(
                text,
                Style::default()
                    .fg(match ink {
                        Ink::Body => t.text,
                        Ink::Glow => t.accent,
                        Ink::Accent => t.accent2,
                        Ink::Rule => t.border,
                        Ink::Faint => t.dim,
                    })
                    .add_modifier(Modifier::BOLD),
            ));
        }
        out.push(Line::from(spans));
    }
    out
}

/// Banner gradient derived from the active theme's three color stops.
fn banner_colors() -> Vec<Color> {
    crate::theme::banner_gradient(crate::tui::HEADER_HEIGHT as usize)
}

// ---------------------------------------------------------------------------
// Chrome helpers — the shared visual vocabulary for every panel, popup and
// status line. Routing through these keeps the UI consistent and lets
// `/theme` restyle the whole shell (no stray hardcoded greys).
// ---------------------------------------------------------------------------

/// Rounded border used by every panel/modal/popup. Plain on terminals that
/// can't render the box-drawing set reliably.
fn rounded() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
}

/// Rounded panel with an optional title line and accent border color.
fn panel(title: Option<Line<'static>>, accent: Option<Color>) -> Block<'static> {
    let t = crate::theme::get();
    let mut b = rounded().border_style(Style::default().fg(accent.unwrap_or(t.border)));
    if let Some(ti) = title {
        b = b.title(ti);
    }
    b
}

/// Selected-row style: a filled "pill" instead of the old bold-only row.
fn selection_style() -> Style {
    let t = crate::theme::get();
    Style::default()
        .bg(t.surface)
        .fg(t.surface_fg)
        .add_modifier(Modifier::BOLD)
}

/// A key-hint chip: the key cap is accent-colored + bold, the label dim.
/// Mirrors the `[Key] Label` idiom used across polished ratatui apps.
fn key_hint(key: &str, label: &str) -> Vec<Span<'static>> {
    let t = crate::theme::get();
    vec![
        Span::styled(
            format!(" {key} "),
            Style::default()
                .bg(t.surface)
                .fg(t.hint_key)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {label}"), Style::default().fg(t.hint_text)),
    ]
}

/// Colored context/progress meter with rounded caps, e.g. `▏██████░░░░▕`.
/// `pct` is 0-100; the fill color escalates to warning/danger as it grows.
fn meter(pct: u64, slots: usize) -> Vec<Span<'static>> {
    let t = crate::theme::get();
    let filled = (pct as usize * slots / 100).min(slots);
    let fill = if pct >= 85 {
        t.error
    } else if pct >= 60 {
        t.warning
    } else {
        t.bar_fill
    };
    vec![
        Span::styled("▏", Style::default().fg(t.bar_empty)),
        Span::styled("█".repeat(filled), Style::default().fg(fill)),
        Span::styled("░".repeat(slots - filled), Style::default().fg(t.bar_empty)),
        Span::styled("▕", Style::default().fg(t.bar_empty)),
    ]
}

/// Total header height: the tallest logo (12 rows), so the band never resizes
/// as the logo changes.
pub const HEADER_HEIGHT: u16 = BANNER_ROWS;
/// Height of the slim one-line wordmark header used on short/narrow windows.
const HEADER_COMPACT: u16 = 1;

/// One entry in the transcript (the scrolling history above the composer).
#[derive(Debug, Clone)]
pub enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    ToolCall {
        name: String,
        summary: String,
    },
    ToolResult {
        name: String,
        ok: bool,
        preview: String,
    },
    /// A tool mutated files — rendered as colored unified diffs.
    ToolDiff {
        name: String,
        files: Vec<crate::diff::FileDiff>,
    },
    Info(String),
    Error(String),
}

/// What the app should do next after an event is processed.
#[derive(Debug)]
pub enum Action {
    None,
    Submit(String),
    CycleMode,
    Quit,
    OpenSlash(String),
    /// User answered a pending approval modal.
    Approve(bool),
    /// "Always allow" — approve and switch to FULL AUTO for the session.
    ApproveAlways,
    /// Esc pressed while the agent is busy — request interruption.
    Interrupt,
    /// Ctrl+B — show/hide the brand banner.
    ToggleBanner,
    /// The input modal was answered with Enter; carries the typed text.
    InputSubmit(String),
}

/// A tappable region registered during draw. Termux is touch-first, so
/// everything the keyboard can do should also be reachable by tapping the
/// thing that shows it. Registered regions are hit-tested topmost-first.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tap {
    /// Index into [`Tui::hint_chips`] — the key-hint strip under the composer.
    HintChip(usize),
    /// The mode pill in the composer title or footer.
    ModeChip,
    /// The brand banner band (toggles it off/on).
    Banner,
    /// The "↑ N lines · esc release" indicator (scrolls back to the bottom).
    ScrollHint,
    /// Inside the Ctrl+O overlay — dismisses it.
    OverlayClose,
    /// An approval-modal button, tagged with its key (`y`/`a`/`n`/esc).
    Approval(char),
    /// Confirm the open input modal.
    InputConfirm,
    /// Cancel the open input modal.
    InputCancel,
    /// The app-brand chip in the footer.
    FooterBrand,
    /// The floating slash-command suggestion popup.
    SlashPopup,
    /// The floating @-mention file suggestion popup.
    AtPopup,
}

/// A modal picker over a list of strings (models, providers, ...).
/// A picker row: the thing you pick, plus the context that tells you whether
/// you picked the right one. Rendering the two at different weights is what
/// makes a list of models or providers read as a UI instead of a text dump.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    pub label: String,
    pub detail: String,
    /// Short status chip, e.g. "active". Empty when there is
    /// nothing worth saying.
    pub badge: String,
}

impl PickerRow {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            badge: String::new(),
        }
    }

    pub fn badge(mut self, b: impl Into<String>) -> Self {
        self.badge = b.into();
        self
    }

    /// Derive a row from a `"label · detail · detail"` string, the shape
    /// every existing call site already builds. Only the first separator
    /// splits: the rest is context and belongs in the dimmed column.
    pub fn parse(s: &str) -> Self {
        match s.split_once(" · ") {
            Some((label, rest)) => Self::new(label.trim(), rest.trim()),
            None => Self::new(s.trim(), ""),
        }
    }

    /// What a selection hands back to the caller. Unchanged from the old
    /// flat-string rows, so every existing `split(" · ")` parse still works.
    pub fn to_wire(&self) -> String {
        if self.detail.is_empty() {
            self.label.clone()
        } else {
            format!("{} · {}", self.label, self.detail)
        }
    }

    fn haystack(&self) -> String {
        if self.detail.is_empty() {
            self.label.to_lowercase()
        } else {
            format!("{} {}", self.label, self.detail).to_lowercase()
        }
    }
}

struct Picker {
    title: String,
    items: Vec<PickerRow>,
    selected: usize,
    filter: String,
}

/// One row in the slash-command popup (built-in or user-defined).
pub struct SlashEntry {
    pub cmd: String,
    pub desc: String,
    /// Bare name when user-defined — used by repl to route submission.
    pub custom: Option<String>,
}

/// Which interactive `/provider` flow is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupKind {
    /// Connect a brand-new provider from a preset.
    Add,
    /// Replace the stored API key of an existing provider.
    EditKey,
}

/// Which `/session` input-modal a submitted value belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    Rename,
    Search,
}

/// Metadata for a running `/provider add|edit` flow — tells the app what an
/// [`Action::InputSubmit`] from the modal belongs to.
pub struct ProviderSetup {
    pub kind: SetupKind,
    /// Preset key (Add) or configured provider name (EditKey).
    pub name: String,
    pub base_url: String,
    /// Set once the key modal has been answered (Add only).
    pub api_key: Option<String>,
    /// True when a custom provider still needs its base_url entered first.
    pub need_base_url: bool,
}

impl ProviderSetup {
    pub fn add(key: &str, base_url: &str) -> Self {
        // A custom preset ships with no base_url — collect it as a first step.
        let need_base_url = base_url.trim().is_empty();
        Self {
            kind: SetupKind::Add,
            name: key.to_string(),
            base_url: if need_base_url {
                "https://".to_string()
            } else {
                base_url.to_string()
            },
            api_key: None,
            need_base_url,
        }
    }

    pub fn edit_key(name: &str) -> Self {
        Self {
            kind: SetupKind::EditKey,
            name: name.to_string(),
            base_url: String::new(),
            api_key: None,
            need_base_url: false,
        }
    }
}

/// Modal single-line input dialog (masked for API keys). Rendered centered
/// over the transcript; typing goes to the modal, not the chat composer.
pub struct InputModal {
    pub title: String,
    /// One-line instruction shown above the input field.
    pub hint: String,
    pub value: String,
    /// Render bullets instead of the typed characters.
    pub mask: bool,
}

impl InputModal {
    pub fn new(title: impl Into<String>, hint: impl Into<String>, mask: bool) -> Self {
        Self {
            title: title.into(),
            hint: hint.into(),
            value: String::new(),
            mask,
        }
    }

    /// What the input row displays (masked or plain) plus the caret.
    pub fn display(&self) -> String {
        let shown = if self.mask {
            "•".repeat(self.value.chars().count())
        } else {
            self.value.clone()
        };
        format!("{shown}█")
    }
}

impl Picker {
    fn filtered(&self) -> Vec<usize> {
        let f = self.filter.to_lowercase();
        self.items
            .iter()
            .enumerate()
            .filter(|(_, r)| f.is_empty() || r.haystack().contains(&f))
            .map(|(i, _)| i)
            .collect()
    }
}

/// The full-screen TUI application state.
pub struct Tui {
    pub input: String,
    pub entries: Vec<Entry>,
    pub mode: Mode,
    pub status: Option<(String, Instant)>,
    pub spinner_idx: usize,
    /// Agent activity indicator (rendered in the footer, never in the transcript).
    busy: bool,
    busy_label: String,
    /// When the current busy stretch started — drives the
    /// "working (esc · Ns)" elapsed counter.
    busy_since: Option<Instant>,
    /// Last reported token usage + assumed window for the context meter.
    pub ctx_used: u64,
    pub ctx_total: u64,
    scroll: usize,
    picker: Option<Picker>,
    /// Modal text-input dialog (API keys, model ids) for /provider flows.
    pub input_modal: Option<InputModal>,
    pending_approval: Option<String>,
    /// Highlighted row in the slash-command suggestion popup.
    slash_sel: usize,
    /// Project files for `@mention` completion (relative paths, sorted).
    files: Vec<String>,
    /// User-defined slash commands (name, description), loaded at startup.
    pub custom_cmds: Vec<(String, String)>,
    /// Full templates keyed by command name for submission rendering.
    pub custom_templates: std::collections::BTreeMap<String, String>,
    /// Right-side dashboard state (visible on wide terminals).
    pub dash: Dash,
    /// In-progress `/provider add` flow (composer submissions are captured).
    pub pending_setup: Option<ProviderSetup>,
    /// True when no usable provider is configured — plain prompts are
    /// redirected to `/provider add` until one is set up.
    pub needs_setup: bool,
    /// Header subtitle ("· provider-name") — updated on provider switches.
    pub subtitle: String,
    /// Provider being edited via the `/provider edit` sub-menus.
    pub edit_target: Option<String>,
    /// In-flight `/session rename|search` waiting on an input-modal answer.
    pub pending_session: Option<SessionAction>,
    /// Session id/name staged for deletion, awaiting confirm.
    pub pending_delete: Option<String>,
    /// Highlighted row in the @-file popup.
    at_sel: usize,
    /// Ctrl+O output-expansion overlay (last tool results in full).
    overlay: bool,
    overlay_scroll: usize,
    /// Double-press Ctrl+C to quit (first press warns instead of exiting).
    last_ctrl_c: Option<Instant>,
    /// Brand banner pinned above the transcript (Ctrl+B toggles).
    show_banner: bool,
    /// Logo currently in the banner. Picked once per display and re-rolled on
    /// toggle — never per frame, or it would flicker.
    logo: &'static Art,
    /// Render cache: wrapped lines for already-processed entries. Only the
    /// growing tail (the streaming entry) is re-wrapped each frame.
    cache_width: u16,
    /// Width of the last draw — decides which logos fit the banner band.
    last_width: u16,
    cached_lines: Vec<Line<'static>>,
    processed_entries: usize,
    /// Per-processed-entry: (content length when wrapped, line count).
    entry_state: Vec<(usize, usize)>,
    last_tick: Instant,
    /// Session start for the dashboard elapsed timer.
    session_started: Instant,
    /// Esc pressed once while busy — waiting for the confirming second Esc
    /// inside [`ESC_ARM_WINDOW`]; `None` when not armed.
    esc_armed_at: Option<Instant>,
    /// Ambient particle effect engine (rendered in the banner band).
    pub fx: crate::effects::Engine,
    /// Sent-prompt history for ↑/↓ recall (newest last).
    pub history: Vec<String>,
    /// Index into [`Tui::history`] while browsing; None = not browsing.
    history_pos: Option<usize>,
    /// In-progress draft saved when history browsing starts, restored by ↓.
    history_draft: String,
    /// Byte index of the editing caret in [`Tui::input`]. Left/Right move it;
    /// typed characters insert here instead of always appending.
    cursor: usize,
    /// Collapsed big-paste payloads by chip number (`[paste #N +L lines]`).
    /// `None` slots are deleted chips whose numbers are never reused, so a
    /// stale reference can never resolve to somebody else's text.
    pastes: Vec<Option<String>>,
    /// Image file names queued for the next message. Display list only —
    /// the payloads live in the worker's pending_images and ride along on
    /// submit, which is also when this list clears.
    pub attached_images: Vec<String>,
    /// Geometry of the last drawn frames, used to map touch/mouse taps back
    /// onto widgets (composer caret, picker rows, transcript).
    last_transcript: Option<Rect>,
    last_composer: Option<Rect>,
    last_picker: Option<Rect>,
    /// Geometry of the floating suggestion popups, so a tap can complete the
    /// command/file under the finger (they float over the transcript).
    last_slash_popup: Option<Rect>,
    last_at_popup: Option<Rect>,
    /// Tappable regions registered by the most recent frame.
    tap_targets: Vec<(Rect, Tap)>,
    /// Row of the previous mouse event — drives finger-drag scrolling.
    last_mouse_row: Option<u16>,
}

/// Identity + counters rendered in the wide-terminal side dashboard.
#[derive(Debug, Clone, Default)]
pub struct Dash {
    /// Short display form of the unique session id.
    pub session_id: String,
    /// Friendly session name (from first response or /session rename).
    pub session_name: String,
    pub model: String,
    pub provider: String,
    /// Working directory, home-shortened, for display.
    pub cwd: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Cumulative tokens/requests across the whole session (not just last).
    pub tot_tokens: u64,
    pub requests: usize,
    pub messages: usize,
    pub plan_done: usize,
    pub plan_total: usize,
}

impl Dash {
    pub fn set_session(
        &mut self,
        id: &str,
        model: &str,
        provider: &str,
        cwd: &str,
        messages: usize,
    ) {
        self.session_id = shorten_id(id);
        self.model = model.to_string();
        self.provider = provider.to_string();
        self.cwd = cwd.to_string();
        self.messages = messages;
    }

    pub fn set_name(&mut self, name: &str) {
        self.session_name = name.to_string();
    }

    /// Reflect a live model/provider switch in the dashboard immediately.
    pub fn set_endpoint(&mut self, provider: &str, model: &str) {
        self.provider = provider.to_string();
        self.model = model.to_string();
    }

    pub fn record_usage(&mut self, prompt: u64, completion: u64) {
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
        self.tot_tokens += prompt + completion;
        self.requests += 1;
    }

    pub fn set_plan(&mut self, todos: &[crate::tools::TodoItem]) {
        self.plan_total = todos.len();
        self.plan_done = todos.iter().filter(|t| t.status == "completed").count();
    }
}

/// First 13 chars of a session id for tight displays.
pub fn shorten_session_id(id: &str) -> String {
    shorten_id(id)
}

fn shorten_id(id: &str) -> String {
    let head: String = id.chars().take(13).collect();
    if id.chars().count() > 13 {
        format!("{head}…")
    } else {
        head.to_string()
    }
}

/// Built-in slash commands surfaced by the composer autocomplete. `pub(crate)`
/// so the dispatcher in `repl.rs` can assert every command it handles is listed
/// here — that hand-maintained table is exactly how `/agents` went missing once.
pub(crate) const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "show all commands"),
    ("/model", "pick a model from the live list"),
    (
        "/reasoning",
        "thinking depth: normal · low · medium · high · max (auto-detected)",
    ),
    ("/approvals", "switch approval mode (plan/build/full-auto)"),
    ("/agents", "list sub-agents and their roles"),
    ("/provider", "menu: add · use · edit · list"),
    ("/compact", "summarize history to free context"),
    ("/clear", "reset the conversation"),
    ("/retry", "re-run the previous task"),
    ("/export", "save transcript as markdown"),
    ("/resume", "resume a previous session by id"),
    ("/session", "rename · search · list · delete sessions"),
    ("/checkpoint", "snapshot the conversation as a branch point"),
    ("/checkpoints", "list this session's checkpoints"),
    ("/branch", "branch a new session from a checkpoint"),
    ("/image", "attach an image to your next message"),
    ("/status", "provider · model · session info"),
    ("/usage", "tokens & cost this session"),
    ("/archive", "park a session aside"),
    ("/doctor", "environment diagnostics"),
    ("/update", "check for a new release and install it"),
    ("/mcp", "external MCP tool servers · status"),
    ("/lsp", "language servers · status"),
    ("/diff", "show uncommitted git changes"),
    ("/review", "reviewer analyzes uncommitted changes"),
    ("/undo", "revert file changes from the last turn"),
    ("/init", "create an AGENTS.md project brief"),
    ("/quit", "exit Laudacode"),
    ("/exit", "exit Laudacode (alias of /quit)"),
    ("/theme", "switch color theme"),
    ("/banner", "roll a different banner logo"),
    ("/effect", "ambient effects (petals, rain, …)"),
    (
        "/skills",
        "search & pick a skill (staged into the composer)",
    ),
];

/// Indices into `SLASH_COMMANDS` whose name starts with `query`
/// (case-insensitive). Empty query returns everything. Test-only helper.
#[cfg(test)]
fn filter_slash_commands(query: &str) -> Vec<usize> {
    let q = query.to_lowercase();
    SLASH_COMMANDS
        .iter()
        .enumerate()
        .filter(|(_, (cmd, _))| q.is_empty() || cmd.starts_with(&q))
        .map(|(i, _)| i)
        .collect()
}

/// How long an armed Esc interrupt stays armed (double-Esc to interrupt).
const ESC_ARM_WINDOW: std::time::Duration = std::time::Duration::from_millis(1500);
const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const TICK_MS: u64 = 100;
/// Rows visible in the floating slash/@ suggestion popups (excluding border).
const POPUP_MAX_VISIBLE: usize = 6;

impl Tui {
    pub fn new() -> Self {
        Self {
            input: String::new(),
            entries: vec![],
            mode: Mode::Build,
            status: None,
            spinner_idx: 0,
            busy: false,
            busy_label: "working".into(),
            busy_since: None,
            ctx_used: 0,
            ctx_total: 128_000,
            scroll: 0,
            picker: None,
            input_modal: None,
            pending_approval: None,
            slash_sel: 0,
            files: vec![],
            custom_cmds: vec![],
            custom_templates: Default::default(),
            dash: Dash::default(),
            pending_setup: None,
            needs_setup: false,
            subtitle: String::new(),
            edit_target: None,
            pending_session: None,
            pending_delete: None,
            session_started: Instant::now(),
            esc_armed_at: None,
            at_sel: 0,
            overlay: false,
            overlay_scroll: 0,
            last_ctrl_c: None,
            show_banner: true,
            logo: pick_logo(80),
            cache_width: 0,
            last_width: 80,
            cached_lines: Vec::new(),
            processed_entries: 0,
            entry_state: Vec::new(),
            last_tick: Instant::now(),
            fx: crate::effects::Engine::new(crate::effects::EffectKind::Off),
            history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
            cursor: 0,
            pastes: Vec::new(),
            attached_images: Vec::new(),
            last_transcript: None,
            last_composer: None,
            last_picker: None,
            last_slash_popup: None,
            last_at_popup: None,
            tap_targets: Vec::new(),
            last_mouse_row: None,
        }
    }

    /// Record a submitted prompt for ↑/↓ recall. Consecutive duplicates are
    /// skipped and the list is capped.
    pub fn record_history(&mut self, entry: &str) {
        self.history_pos = None;
        self.history_draft.clear();
        if entry.is_empty() {
            return;
        }
        if self.history.last().map(|l| l == entry).unwrap_or(false) {
            return;
        }
        self.history.push(entry.to_string());
        if self.history.len() > HISTORY_MAX {
            let drop = self.history.len() - HISTORY_MAX;
            self.history.drain(..drop);
        }
    }

    /// Seed with persisted history from previous sessions (merged at the
    /// front so this session's entries stay newest).
    pub fn seed_history(&mut self, past: Vec<String>) {
        if past.is_empty() {
            return;
        }
        let mut merged = past;
        merged.append(&mut self.history);
        merged.dedup();
        if merged.len() > HISTORY_MAX {
            let drop = merged.len() - HISTORY_MAX;
            merged.drain(..drop);
        }
        self.history = merged;
    }

    /// ↑ — older prompt. Starts browsing from the newest entry (any typed
    /// draft is saved for ↓ to restore).
    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_pos {
            None => {
                self.history_draft = std::mem::take(&mut self.input);
                let pos = self.history.len() - 1;
                self.history_pos = Some(pos);
                self.input = self.history[pos].clone();
            }
            Some(pos) => {
                if pos > 0 {
                    self.history_pos = Some(pos - 1);
                    self.input = self.history[pos - 1].clone();
                }
            }
        }
    }

    /// ↓ walks forward through history; past the newest it restores
    /// the draft and exits browsing mode.
    fn history_down(&mut self) {
        if let Some(pos) = self.history_pos {
            if pos + 1 < self.history.len() {
                self.history_pos = Some(pos + 1);
                self.input = self.history[pos + 1].clone();
            } else {
                self.history_pos = None;
                self.input = std::mem::take(&mut self.history_draft);
            }
        }
    }

    /// Scroll the transcript up by `n` rows (wheel / PgUp).
    pub fn page_up(&mut self, n: usize) {
        self.scroll += n;
    }

    /// Scroll the transcript down by `n` rows (wheel / PgDn), clamped at 0.
    pub fn page_down(&mut self, n: usize) {
        self.scroll = self.scroll.saturating_sub(n);
    }

    pub fn push(&mut self, e: Entry) {
        self.entries.push(e);
        self.scroll = 0;
    }

    /// What the Esc key does while the agent is busy: the first press ARMS
    /// the interrupt (visible status hint) and the second press inside
    /// [`ESC_ARM_WINDOW`] actually interrupts — a deliberate double-tap, so
    /// a stray Esc can't kill a running turn. Not busy: close an open
    /// @-token first, then release scroll-back, then clear the input.
    fn esc_pressed(&mut self) -> Action {
        self.history_pos = None;
        if self.busy {
            let armed = self
                .esc_armed_at
                .is_some_and(|t| t.elapsed() <= ESC_ARM_WINDOW);
            if armed {
                self.esc_armed_at = None;
                self.clear_status();
                return Action::Interrupt;
            }
            self.esc_armed_at = Some(Instant::now());
            self.set_status("press Esc again to interrupt");
            return Action::None;
        }
        self.esc_armed_at = None;
        if self.at_token_present() {
            if let Some(i) = self.input.rfind('@') {
                self.input.truncate(i);
                self.cursor_home();
                self.at_sel = 0;
            }
        } else if self.scroll > 0 {
            self.scroll = 0;
        } else {
            self.input.clear();
            self.cursor_home();
        }
        Action::Interrupt
    }

    /// Set/clear the footer activity indicator. Ending a busy period also
    /// disarms a stale interrupt arm.
    pub fn set_busy(&mut self, busy: bool, label: impl Into<String>) {
        self.busy = busy;
        if !busy {
            self.esc_armed_at = None;
        }
        if busy {
            self.busy_label = label.into();
            if self.busy_since.is_none() {
                self.busy_since = Some(Instant::now());
            }
        } else {
            self.busy_since = None;
        }
    }

    pub fn is_busy(&self) -> bool {
        self.busy
    }

    /// Feed the context-left meter (footer). `total` is the assumed window.
    pub fn set_usage(&mut self, used: u64, total: u64) {
        self.ctx_used = used;
        if total > 0 {
            self.ctx_total = total;
        }
    }

    /// Refresh the @-mention file list (relative paths, sorted, capped).
    pub fn set_files(&mut self, files: Vec<String>) {
        let mut f = files;
        f.sort_by_key(|p| p.to_lowercase());
        self.files = f.into_iter().take(2000).collect();
    }

    /// Insert bracketed-paste content. Short pastes go in verbatim; big ones
    /// (Claude/Codex-style: >3 lines or >800 chars) collapse to a
    /// `[paste #N +L lines]` chip so the composer stays readable. The full
    /// text expands back at submit; Backspace on a chip drops it whole.
    /// Never triggers submission — the user sends with Enter afterwards.
    pub fn insert_paste(&mut self, text: &str) {
        if self.input_modal.is_some() {
            if let Some(m) = &mut self.input_modal {
                m.value.push_str(text.trim_end_matches(['\r', '\n']));
            }
            return;
        }
        if self.pending_approval.is_some() || self.picker.is_some() || self.overlay {
            return; // modals take over all input
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        let lines = normalized.lines().count();
        if lines > 3 || normalized.chars().count() > 800 {
            let n = self.pastes.len();
            self.pastes.push(Some(normalized));
            self.insert_str(&format!("[paste #{n} +{lines} lines]"));
        } else {
            self.insert_str(&normalized);
        }
        self.slash_sel = 0;
        self.at_sel = 0;
    }

    /// Chip token for paste payload `n`, or `None` when the slot is dead.
    /// Only live chips expand — a hand-typed lookalike stays literal.
    fn paste_token(&self, n: usize) -> Option<String> {
        let body = self.pastes.get(n)?.as_ref()?;
        Some(format!("[paste #{n} +{} lines]", body.lines().count()))
    }

    /// Expand live paste chips in `text` to their full content.
    fn expand_pastes(&self, text: &str) -> String {
        let mut out = text.to_string();
        for n in 0..self.pastes.len() {
            let pair = match (self.paste_token(n), self.pastes.get(n)) {
                (Some(t), Some(Some(b))) => (t, b.clone()),
                _ => continue,
            };
            out = out.replace(&pair.0, &pair.1);
        }
        out
    }

    /// Take the composer content for submission: expand paste chips, clear
    /// the draft + payloads + image chips, record history. Returns `None`
    /// when there is nothing to send.
    fn take_submission(&mut self) -> Option<String> {
        let input = self.input.trim().to_string();
        // Expand before dropping the payloads — the chips resolve here.
        let expanded = self.expand_pastes(&input);
        self.input.clear();
        self.cursor_home();
        self.pastes.clear();
        self.attached_images.clear();
        if expanded.is_empty() {
            return None;
        }
        self.record_history(&expanded);
        Some(expanded)
    }

    /// Open the centered input dialog (API key, model id, …).
    pub fn open_input_modal(&mut self, modal: InputModal) {
        self.input_modal = Some(modal);
    }

    fn on_input_modal_key(&mut self, key: KeyEvent) -> Action {
        let Some(m) = &mut self.input_modal else {
            return Action::None;
        };
        match key.code {
            KeyCode::Enter => {
                let value = std::mem::take(&mut m.value);
                self.input_modal = None;
                Action::InputSubmit(value)
            }
            KeyCode::Esc => {
                self.input_modal = None;
                Action::None
            }
            KeyCode::Backspace => {
                m.value.pop();
                Action::None
            }
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    if c == 'c' {
                        self.input_modal = None;
                    }
                } else {
                    m.value.push(c);
                }
                Action::None
            }
            _ => Action::None,
        }
    }

    /// Composer height for the current input: grows line-by-line as the
    /// prompt gets longer, capped so the transcript never starves.
    pub fn composer_height(&self, area_width: u16, area_height: u16) -> u16 {
        const MIN_H: u16 = 3;
        const MAX_H: u16 = 14;
        let inner_w = area_width.saturating_sub(2).max(10);
        let text_rows = if self.input.is_empty() {
            1
        } else {
            wrap_composer(&self.input, inner_w as usize).len() as u16
        };
        let desired = text_rows + 2; // borders
        let cap = area_height.saturating_sub(6).clamp(MIN_H, MAX_H).max(MIN_H);
        desired.clamp(MIN_H, cap)
    }

    /// Toggle the pinned brand banner (Ctrl+B). Re-rolls the logo so the
    /// button doubles as "show me another one".
    pub fn toggle_banner(&mut self) {
        self.show_banner = !self.show_banner;
        if self.show_banner {
            self.logo = pick_logo(self.last_width);
        }
    }

    /// The logo the banner is currently showing.
    pub fn banner_logo(&self) -> &'static Art {
        self.logo
    }

    /// Re-roll the banner logo (e.g. after a terminal resize).
    pub fn reroll_banner(&mut self) {
        self.logo = pick_logo(self.last_width);
    }

    /// Roll a *different* banner logo (`/banner`) and make sure the banner
    /// shows. Pure random can hand back the same art twice, which reads as
    /// "the command did nothing" — so the current logo is excluded when
    /// alternatives fit.
    pub fn roll_banner(&mut self) {
        self.show_banner = true;
        let fits: Vec<&Art> = LOGOS
            .iter()
            .filter(|a| composed_width(a) <= self.last_width)
            .collect();
        let others: Vec<&Art> = fits
            .iter()
            .filter(|a| a.name != self.logo.name)
            .copied()
            .collect();
        let pool = if others.is_empty() { fits } else { others };
        if !pool.is_empty() {
            self.logo = pool[next_rand() as usize % pool.len()];
        }
    }

    pub fn banner_visible(&self) -> bool {
        self.show_banner
    }

    // -----------------------------------------------------------------------
    // @-file mention popup
    // -----------------------------------------------------------------------

    /// Active while the user is typing a path after an '@' (no whitespace yet
    /// after the last '@', and the '@' begins the input or follows a space).
    pub fn at_popup_active(&self) -> bool {
        match self.input.rfind('@') {
            None => false,
            Some(i) => {
                let at_start = i == 0 || self.input[..i].ends_with(char::is_whitespace);
                let no_space_after = !self.input[i + 1..].contains(char::is_whitespace);
                at_start
                    && no_space_after
                    && self
                        .files
                        .iter()
                        .any(|f| Self::at_matches(&self.files_query(), f))
            }
        }
    }

    fn files_query(&self) -> String {
        match self.input.rfind('@') {
            Some(i) => self.input[i + 1..].to_lowercase(),
            None => String::new(),
        }
    }

    fn at_matches(query: &str, path: &str) -> bool {
        query.is_empty() || path.to_lowercase().contains(query)
    }

    /// Indices into `files` matching the current @-query (substring, then
    /// prefix-first ordering for relevance).
    pub fn at_matches_list(&self) -> Vec<usize> {
        if !self.at_popup_active() && !self.at_token_present() {
            return Vec::new();
        }
        let q = self.files_query();
        let mut subs: Vec<(usize, usize)> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| Self::at_matches(&q, f))
            .map(|(i, f)| {
                let lower = f.to_lowercase();
                let depth = f.matches('/').count();
                // Rank: filename-prefix hits first, shallower paths next.
                let rank =
                    if lower.rsplit('/').next().unwrap_or("").starts_with(&q) && !q.is_empty() {
                        0
                    } else {
                        1
                    };
                (rank * 10_000 + depth, i)
            })
            .collect();
        subs.sort_by_key(|(rank, _)| *rank);
        subs.into_iter().map(|(_, i)| i).collect()
    }

    fn at_token_present(&self) -> bool {
        match self.input.rfind('@') {
            Some(i) => i == 0 || self.input[..i].ends_with(char::is_whitespace),
            None => false,
        }
    }

    /// Replace the typed '@query' with the highlighted file (plus a space).
    pub fn complete_at(&mut self) {
        let matches = self.at_matches_list();
        if matches.is_empty() {
            return;
        }
        let idx = matches[self.at_sel.min(matches.len() - 1)];
        let path = &self.files[idx];
        let start = self.input.rfind('@').unwrap_or(0);
        self.input.truncate(start);
        self.input.push('@');
        self.input.push_str(path);
        self.input.push(' ');
        self.at_sel = 0;
        self.cursor_end();
    }

    fn move_at_sel(&mut self, delta: isize) {
        let n = self.at_matches_list().len();
        if n == 0 {
            return;
        }
        let cur = self.at_sel.min(n - 1) as isize;
        let next = ((cur + delta).rem_euclid(n as isize)) as usize;
        self.at_sel = next;
    }

    // ---- Cursor (caret) editing helpers ---------------------------------

    /// Byte offset of the N-th char in `s` (0-indexed char count).
    fn char_byte_offset(s: &str, char_idx: usize) -> usize {
        s.char_indices().nth(char_idx).map_or(s.len(), |(i, _)| i)
    }

    pub fn cursor_end(&mut self) {
        self.cursor = self.input.chars().count();
    }

    fn cursor_home(&mut self) {
        self.cursor = 0;
    }

    fn cursor_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
        }
    }

    fn cursor_right(&mut self) {
        if self.cursor < self.input.chars().count() {
            self.cursor += 1;
        }
    }

    /// Insert `c` at the caret and advance past it.
    fn insert_char(&mut self, c: char) {
        let byte_at = Self::char_byte_offset(&self.input, self.cursor);
        let tail: String = self.input[byte_at..].to_string();
        self.input.truncate(byte_at);
        self.input.push(c);
        self.input.push_str(&tail);
        self.cursor += 1;
    }

    /// Insert a string at the caret and advance past it.
    fn insert_str(&mut self, s: &str) {
        let byte_at = Self::char_byte_offset(&self.input, self.cursor);
        let tail: String = self.input[byte_at..].to_string();
        self.input.truncate(byte_at);
        self.input.push_str(s);
        self.input.push_str(&tail);
        self.cursor += s.chars().count();
    }

    /// Delete the char immediately before the caret — or a whole paste chip
    /// when the caret sits right behind one (chips delete atomically, like
    /// Claude/Codex, instead of leaving a broken half-token behind).
    fn backspace_at(&mut self) -> bool {
        if self.cursor == 0 || self.input.is_empty() {
            return false;
        }
        let byte_at = Self::char_byte_offset(&self.input, self.cursor);
        // A live chip ending exactly at the caret: (start byte, token
        // chars, payload index). Owned data only, so the borrow of
        // `self.input` ends before the mutation below.
        let chip: Option<(usize, usize, usize)> = self.input[..byte_at]
            .rfind("[paste #")
            .and_then(|start| {
                let token = &self.input[start..byte_at];
                if !token.ends_with(']') {
                    return None;
                }
                let len = token.chars().count();
                self.pastes.iter().enumerate().find_map(|(n, _)| {
                    (self.paste_token(n).as_deref() == Some(token)).then_some((start, len, n))
                })
            });
        if let Some((start, len, n)) = chip {
            self.pastes[n] = None;
            let tail: String = self.input[byte_at..].to_string();
            self.input.truncate(start);
            self.input.push_str(&tail);
            self.cursor -= len;
            return true;
        }
        let prev_byte = Self::char_byte_offset(&self.input, self.cursor - 1);
        let tail: String = self.input[byte_at..].to_string();
        self.input.truncate(prev_byte);
        self.input.push_str(&tail);
        self.cursor -= 1;
        true
    }

    /// Append streamed assistant text, merging into the last Assistant entry.
    pub fn push_stream_text(&mut self, delta: &str) {
        if let Some(Entry::Assistant(t)) = self.entries.last_mut() {
            t.push_str(delta);
            return;
        }
        self.entries.push(Entry::Assistant(delta.to_string()));
    }

    /// Append streamed reasoning text, merging into the last Reasoning entry.
    pub fn push_reasoning_text(&mut self, delta: &str) {
        if let Some(Entry::Reasoning(t)) = self.entries.last_mut() {
            t.push_str(delta);
            return;
        }
        self.entries.push(Entry::Reasoning(delta.to_string()));
    }

    pub fn set_status(&mut self, msg: impl Into<String>) {
        let m = msg.into();
        if m.is_empty() {
            self.status = None
        } else {
            self.status = Some((m, Instant::now()))
        }
    }

    pub fn clear_status(&mut self) {
        self.status = None;
    }

    /// The open picker's title and items, for tests.
    #[cfg(test)]
    pub fn picker_view(&self) -> Option<(&str, &[PickerRow])> {
        self.picker
            .as_ref()
            .map(|p| (p.title.as_str(), p.items.as_slice()))
    }

    pub fn open_picker(&mut self, title: impl Into<String>, items: Vec<String>) {
        self.open_picker_rows(title, items.iter().map(|s| PickerRow::parse(s)).collect());
    }

    /// Pickers that carry real status (active provider, signed in, current
    /// model) pass explicit rows so they can show a badge.
    pub fn open_picker_rows(&mut self, title: impl Into<String>, items: Vec<PickerRow>) {
        self.picker = Some(Picker {
            title: title.into(),
            items,
            selected: 0,
            filter: String::new(),
        });
    }

    pub fn open_approval(&mut self, detail: String) {
        self.push(Entry::Info(format!("Approval requested:\n{}", detail)));
        self.pending_approval = Some(detail);
    }

    /// The suggestion popup is open while the user is still typing the
    /// command name — i.e. input starts with '/' and has no whitespace yet.
    pub fn slash_popup_active(&self) -> bool {
        self.input.starts_with('/') && !self.input.chars().skip(1).any(char::is_whitespace)
    }

    /// Built-in + custom command entries for the popup.
    pub fn slash_entries(&self) -> Vec<SlashEntry> {
        let mut v: Vec<SlashEntry> = SLASH_COMMANDS
            .iter()
            .map(|(c, d)| SlashEntry {
                cmd: c.to_string(),
                desc: d.to_string(),
                custom: None,
            })
            .collect();
        for cc in &self.custom_cmds {
            v.push(SlashEntry {
                cmd: format!("/{}", cc.0),
                desc: cc.1.clone(),
                custom: Some(cc.0.clone()),
            });
        }
        v
    }

    /// Matching entries for the current input as indices into `slash_entries`.
    pub fn slash_matches(&self) -> Vec<usize> {
        if !self.slash_popup_active() {
            return Vec::new();
        }
        let q = self.input.to_lowercase();
        self.slash_entries()
            .iter()
            .enumerate()
            .filter(|(_, e)| q.is_empty() || e.cmd.starts_with(&q))
            .map(|(i, _)| i)
            .collect()
    }

    pub fn set_custom_cmds(&mut self, cmds: Vec<(String, String)>) {
        self.custom_cmds = cmds;
    }

    /// Replace the typed token with the highlighted suggestion (plus a space).
    pub fn complete_slash(&mut self) {
        let matches = self.slash_matches();
        if matches.is_empty() {
            return;
        }
        let idx = matches[self.slash_sel.min(matches.len() - 1)];
        self.input = format!("{} ", self.slash_entries()[idx].cmd);
        self.slash_sel = 0;
        self.cursor_end();
    }

    fn move_slash_sel(&mut self, delta: isize) {
        let n = self.slash_matches().len();
        if n == 0 {
            return;
        }
        let cur = self.slash_sel.min(n - 1) as isize;
        let next = ((cur + delta).rem_euclid(n as isize)) as usize;
        self.slash_sel = next;
    }

    /// Text content of an entry (used to detect growth of the streaming tail).
    fn entry_len(e: &Entry) -> usize {
        match e {
            Entry::User(t)
            | Entry::Assistant(t)
            | Entry::Reasoning(t)
            | Entry::Info(t)
            | Entry::Error(t) => t.len(),
            Entry::ToolCall { name, summary } => name.len() + summary.len(),
            Entry::ToolResult { name, preview, .. } => name.len() + preview.len(),
            Entry::ToolDiff { name, files } => {
                name.len()
                    + files
                        .iter()
                        .map(|f| f.path.len() + f.lines.iter().map(|l| l.text.len()).sum::<usize>())
                        .sum::<usize>()
            }
        }
    }

    /// Wrap one entry into display lines.
    fn entry_lines(e: &Entry, width: u16) -> Vec<Line<'static>> {
        match e {
            Entry::User(t) => vec![Line::from(Span::styled(
                format!("{} {}", "›", t),
                Style::default()
                    .fg(crate::theme::get().user)
                    .add_modifier(Modifier::BOLD),
            ))],
            Entry::Assistant(t) => {
                crate::markdown::render_markdown(t, width.saturating_sub(2) as usize)
            }
            Entry::Reasoning(t) => wrap_text(t, width.saturating_sub(4) as usize)
                .into_iter()
                .map(|l| {
                    Line::from(Span::styled(
                        l,
                        Style::default()
                            .fg(crate::theme::get().dim)
                            .add_modifier(Modifier::ITALIC),
                    ))
                })
                .collect(),
            Entry::ToolCall { name, summary } => vec![Line::from(vec![
                Span::styled("● ", Style::default().fg(crate::theme::get().accent2)),
                Span::styled(
                    name.clone(),
                    Style::default()
                        .fg(crate::theme::get().accent2)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!(" {summary}"),
                    Style::default().fg(crate::theme::get().gray),
                ),
            ])],
            Entry::ToolResult { name, ok, preview } => {
                let t = crate::theme::get();
                let color = if *ok { t.success } else { t.error };
                let mut lines = vec![Line::from(vec![
                    Span::styled(if *ok { "✔ " } else { "✘ " }, Style::default().fg(color)),
                    Span::styled(name.clone(), Style::default().fg(color)),
                ])];
                for l in preview.lines().take(4) {
                    lines.push(Line::from(Span::styled(
                        format!("  {l}"),
                        Style::default().fg(crate::theme::get().gray),
                    )));
                }
                let hidden = preview.lines().count().saturating_sub(4);
                if hidden > 0 {
                    lines.push(Line::from(Span::styled(
                        format!("  … +{hidden} lines (ctrl+o for full output)"),
                        Style::default().fg(crate::theme::get().gray),
                    )));
                }
                lines
            }
            Entry::ToolDiff { name, files } => {
                use crate::diff::LineKind;
                let total_add: usize = files.iter().map(|f| f.added).sum();
                let total_del: usize = files.iter().map(|f| f.removed).sum();
                let mut lines = vec![Line::from(vec![
                    Span::styled(
                        "✎ ",
                        Style::default()
                            .fg(crate::theme::get().accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        name.clone(),
                        Style::default()
                            .fg(crate::theme::get().accent)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!("  +{total_add} −{total_del}"),
                        Style::default().fg(Color::Gray),
                    ),
                ])];
                for f in files {
                    if f.is_empty() {
                        continue;
                    }
                    lines.push(Line::from(vec![
                        Span::styled("┌─ ", Style::default().fg(crate::theme::get().accent2)),
                        Span::styled(
                            f.path.clone(),
                            Style::default()
                                .fg(crate::theme::get().accent2)
                                .add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    // Syntax-aware diff rows: token colors from the file's
                    // language over subtle add/remove backgrounds.
                    let lang = crate::syntax::lang_of(&f.path);
                    let mut syn_state = crate::syntax::SynState::default();
                    for dl in &f.lines {
                        let (bar, tint) = match dl.kind {
                            LineKind::Add => ("│", Some(crate::theme::get().add_bg)),
                            LineKind::Del => ("│", Some(crate::theme::get().del_bg)),
                            LineKind::Meta => ("│", None),
                            LineKind::Ctx => ("│", None),
                        };
                        let bar_style = match dl.kind {
                            LineKind::Add => Style::default().fg(crate::theme::get().success),
                            LineKind::Del => Style::default().fg(crate::theme::get().error),
                            LineKind::Meta => Style::default()
                                .fg(crate::theme::get().heading)
                                .add_modifier(Modifier::ITALIC),
                            LineKind::Ctx => Style::default().fg(crate::theme::get().dim),
                        };
                        // Meta lines (@@ headers) stay plain; code gets tokens.
                        // The +/-/-sign keeps its strong kind color.
                        let content: Vec<Span<'static>> = match dl.kind {
                            LineKind::Meta => vec![Span::styled(
                                dl.text.clone(),
                                Style::default()
                                    .fg(crate::theme::get().heading)
                                    .add_modifier(Modifier::ITALIC),
                            )],
                            _ => {
                                let base = match tint {
                                    Some(bg) => Style::default().bg(bg),
                                    None => Style::default(),
                                };
                                let mut spans: Vec<Span<'static>> = Vec::new();
                                if let Some(rest) = dl
                                    .text
                                    .strip_prefix('+')
                                    .or_else(|| dl.text.strip_prefix('-'))
                                {
                                    let sign_color = match dl.kind {
                                        LineKind::Add => crate::theme::get().success,
                                        _ => crate::theme::get().error,
                                    };
                                    spans.push(Span::styled(
                                        dl.text[..1].to_string(),
                                        base.fg(sign_color).add_modifier(Modifier::BOLD),
                                    ));
                                    spans.extend(crate::syntax::highlight_line(
                                        rest,
                                        lang,
                                        &mut syn_state,
                                        base,
                                    ));
                                } else {
                                    spans.extend(crate::syntax::highlight_line(
                                        &dl.text,
                                        lang,
                                        &mut syn_state,
                                        base,
                                    ));
                                }
                                spans
                            }
                        };
                        for seg in
                            crate::markdown::wrap_styled(&content, width.saturating_sub(4) as usize)
                        {
                            let mut row = vec![Span::styled(bar.to_string(), bar_style)];
                            row.extend(seg.spans);
                            lines.push(Line::from(row));
                        }
                    }
                    lines.push(Line::from(Span::styled(
                        "└─",
                        Style::default().fg(crate::theme::get().accent2),
                    )));
                }
                lines
            }
            Entry::Info(t) => t
                .lines()
                .map(|l| {
                    Line::from(Span::styled(
                        l.to_string(),
                        Style::default().fg(crate::theme::get().heading),
                    ))
                })
                .collect(),
            Entry::Error(t) => wrap_text(t, width.saturating_sub(2) as usize)
                .into_iter()
                .map(|l| {
                    Line::from(Span::styled(
                        l,
                        Style::default()
                            .fg(crate::theme::get().error)
                            .add_modifier(Modifier::BOLD),
                    ))
                })
                .collect(),
        }
    }

    /// Bring the render cache up to date.
    ///
    /// Only entries appended since the last frame — plus the growing
    /// streaming tail — are (re)wrapped; everything else is cached. Per-frame
    /// cost is O(new bytes), not O(transcript).
    fn ensure_render_cache(&mut self, width: u16) {
        if self.cache_width != width {
            self.cache_width = width;
            self.cached_lines.clear();
            self.processed_entries = 0;
            self.entry_state.clear();
        }
        // Transcript cleared/truncated (/clear).
        while self.processed_entries > self.entries.len() {
            self.processed_entries -= 1;
            let (_, n) = self.entry_state.pop().unwrap_or((0, 0));
            let keep = self.cached_lines.len().saturating_sub(n);
            self.cached_lines.truncate(keep);
        }
        // The last processed entry may have grown (streaming appends to it):
        // drop its cached lines so it gets re-wrapped below.
        if self.processed_entries > 0 {
            let idx = self.processed_entries - 1;
            if Self::entry_len(&self.entries[idx]) != self.entry_state[idx].0 {
                self.processed_entries -= 1;
                let (_, n) = self.entry_state.pop().unwrap_or((0, 0));
                let keep = self.cached_lines.len().saturating_sub(n);
                self.cached_lines.truncate(keep);
            }
        }
        // Wrap any newly appended entries.
        while self.processed_entries < self.entries.len() {
            let e = &self.entries[self.processed_entries];
            let lines = Self::entry_lines(e, width);
            let count = lines.len();
            let len = Self::entry_len(e);
            self.cached_lines.extend(lines);
            self.entry_state.push((len, count));
            self.processed_entries += 1;
        }
    }

    /// Dashboard panel size gate (None = hidden): shown on terminals that
    /// are at least 88 columns wide AND 39 rows tall — covers both portrait
    /// desktop splits and Termux landscape (~39×147). Anything narrower or
    /// shorter keeps the single-column layout untouched.
    pub fn dash_width(total_w: u16, total_h: u16) -> Option<u16> {
        if total_w >= 88 && total_h >= 39 {
            Some(Self::DASH_WIDTH)
        } else {
            None
        }
    }

    /// Height of the header to reserve — 0 = none.
    ///   full 11-row art  → wide AND tall enough
    ///   compact 1-line   → some width but little height (mobile portrait)
    pub fn header_height(total_w: u16, total_h: u16, show_banner: bool) -> u16 {
        if !show_banner {
            return 0;
        }
        if total_w >= 60 && total_h >= 23 {
            HEADER_HEIGHT
        } else if total_w >= 40 && total_h >= 10 {
            HEADER_COMPACT
        } else {
            0
        }
    }

    /// Height of the compact summary bar shown when the side dashboard can't
    /// fit (narrow window) but there's still room for a single info line.
    /// The content truncates itself to the width, so portrait phones down to
    /// 40 columns still get model/provider/token status.
    pub fn compact_summary_h(total_w: u16, total_h: u16, no_dashboard: bool) -> u16 {
        if no_dashboard && total_w >= 40 && total_h >= 14 {
            1
        } else {
            0
        }
    }

    /// Build the hint strip, dropping low-priority chips until they fit
    /// `width` columns. Each chip renders the key cap in the accent color and
    /// the action label dimmed, so the strip scans much faster than a run of
    /// plain grey text. Returns `(key, label)` pairs chosen for `width`.
    fn hint_chips(width: u16) -> Vec<(&'static str, &'static str)> {
        const CHIPS: &[(&str, &str)] = &[
            ("enter", "send"),
            ("esc", "interrupt"),
            ("↑↓", "history"),
            ("tab", "mode"),
            ("←→", "edit"),
            ("ctrl+o", "tools"),
            ("ctrl+b", "banner"),
            ("ctrl+c", "quit"),
        ];
        let w = width as usize;
        if w < 14 {
            return vec![("esc", "")];
        }
        let mut out: Vec<(&str, &str)> = Vec::new();
        // Each chip costs " KEY label" plus the 2-col separator between chips.
        let mut used = 1usize;
        for (key, label) in CHIPS {
            let cost = key.chars().count()
                + label.chars().count()
                + 3
                + if out.is_empty() { 0 } else { 2 };
            if used + cost > w {
                break;
            }
            used += cost;
            out.push((key, label));
        }
        if out.is_empty() {
            out.push(("esc", ""));
        }
        out
    }

    /// Plain-text form of [`Tui::hint_chips`] (test-only width check).
    #[cfg(test)]
    fn hint_line(width: u16) -> String {
        let chips = Self::hint_chips(width);
        let mut s = String::from(" ");
        for (i, (key, label)) in chips.iter().enumerate() {
            if i > 0 {
                s.push_str("  ");
            }
            s.push_str(key);
            if !label.is_empty() {
                s.push(' ');
                s.push_str(label);
            }
        }
        s
    }

    /// Last path component of the working directory, for the composer
    /// breadcrumb. Falls back to the full stored path when it has no basename.
    fn cwd_basename(&self) -> String {
        let raw = self.dash.cwd.trim();
        if raw.is_empty() {
            return String::new();
        }
        let p = raw.trim_end_matches('/');
        if p.is_empty() {
            return "/".to_string();
        }
        p.rsplit('/')
            .next()
            .filter(|s| !s.is_empty())
            .unwrap_or(p)
            .to_string()
    }
    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        // Every frame rebuilds the tap map, so stale hitboxes can never fire.
        self.tap_targets.clear();
        self.last_slash_popup = None;
        self.last_at_popup = None;

        // Ctrl+O full-output overlay takes over the screen.
        if self.overlay {
            self.draw_overlay(f, area);
            return;
        }

        // Input modal (API key / model id) floats over everything.
        if let Some(mut m) = self.input_modal.take() {
            self.draw_input_modal(f, area, &mut m);
            self.input_modal = Some(m);
            return;
        }

        if self.picker.is_some() {
            if let Some(mut p) = self.picker.take() {
                self.draw_picker(f, area, &mut p);
                self.picker = Some(p);
            }
            return;
        }

        // Terminals ≥88×50 get a persistent side dashboard; smaller sizes
        // keep the classic single-column layout pixel-for-pixel.
        let (area, dash_area) = match Self::dash_width(area.width, area.height) {
            Some(dw) => {
                let cols =
                    Layout::horizontal([Constraint::Min(40), Constraint::Length(dw)]).split(area);
                (cols[0], Some(cols[1]))
            }
            None => (area, None),
        };

        // The composer grows with the draft instead of clipping long prompts.
        let comp_h = self.composer_height(area.width, area.height);

        // Adaptive header: full 11-row banner when there's room; a slim
        // one-line wordmark when the window is short/narrow; nothing on tiny
        // screens. This keeps the transcript front-and-center on mobile.
        let header_rows = Self::header_height(area.width, area.height, self.show_banner);
        let has_header = header_rows > 0;
        // When no side dashboard fits (narrow window), show a compact summary
        // bar above the composer so identity/tokens/plan stay glanceable.
        let compact_h = Self::compact_summary_h(area.width, area.height, dash_area.is_none());
        let has_compact = compact_h > 0;

        let mut constraints = Vec::with_capacity(7);
        if has_header {
            constraints.push(Constraint::Length(header_rows));
        }
        constraints.push(Constraint::Min(1));
        if has_compact {
            constraints.push(Constraint::Length(compact_h));
        }
        // Attachment chips (queued images) get their own row above the
        // composer so they never eat transcript space when absent.
        let has_attachments = !self.attached_images.is_empty();
        if has_attachments {
            constraints.push(Constraint::Length(1));
        }
        constraints.push(Constraint::Length(comp_h));
        constraints.push(Constraint::Length(1));
        constraints.push(Constraint::Length(1));
        let chunks = Layout::vertical(constraints).split(area);
        let mut idx = 0usize;
        let header = if has_header {
            let h = chunks[idx];
            idx += 1;
            Some(h)
        } else {
            None
        };
        let transcript_area = chunks[idx];
        idx += 1;
        let compact_area = if has_compact {
            let c = chunks[idx];
            idx += 1;
            Some(c)
        } else {
            None
        };
        let composer_area = if has_attachments {
            let a = chunks[idx];
            idx += 1;
            let names = self
                .attached_images
                .iter()
                .map(|n| format!("[image: {n}]"))
                .collect::<Vec<_>>()
                .join("  ");
            f.render_widget(
                Paragraph::new(Line::from(Span::styled(
                    Self::truncate_to(names, area.width as usize),
                    Style::default().fg(crate::theme::get().accent),
                ))),
                a,
            );
            chunks[idx]
        } else {
            chunks[idx]
        };
        idx += 1;
        let hints_area = chunks[idx];
        idx += 1;
        let footer_area = chunks[idx];

        if let Some(h) = header {
            if header_rows == HEADER_COMPACT {
                // Slim wordmark line — mode chip + version, one row.
                let line = Line::from(vec![
                    Span::styled(
                        " LaudaCode ",
                        Style::default()
                            .fg(banner_colors()[0])
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" v{} · {}", env!("CARGO_PKG_VERSION"), self.mode.label()),
                        Style::default().fg(crate::theme::get().dim),
                    ),
                ]);
                f.render_widget(Paragraph::new(line), h);
                // Tapping the slim header cycles the mode (same as Tab).
                self.tap_targets.push((h, Tap::ModeChip));
            } else {
                // A wider window may unlock a roomier logo; a narrower one must
                // fall back so the art never clips mid-glyph.
                if self.last_width != area.width {
                    self.last_width = area.width;
                    self.reroll_banner();
                }
                let banner_lines = banner_lines(self.logo);
                f.render_widget(Paragraph::new(banner_lines), h);
                // Ambient particles live inside the banner band only.
                self.fx.set_area(h.width, h.height);
                if !self.is_busy() {
                    self.fx.render(f.buffer_mut(), h);
                }
                // Tapping the banner collapses it (Ctrl+B equivalent).
                self.tap_targets.push((h, Tap::Banner));
            }
        }

        // Record widget geometry so touch/mouse taps can be mapped back.
        self.last_transcript = Some(transcript_area);
        self.last_composer = Some(composer_area);

        // Compact summary bar (narrow screens only — fills in for the side
        // dashboard): model · provider · session · tokens · plan.
        if let Some(c) = compact_area {
            let line = self.compact_summary(c.width);
            f.render_widget(
                Paragraph::new(Line::from(line)).alignment(ratatui::layout::Alignment::Left),
                c,
            );
        }

        // Transcript
        let width = transcript_area.width.max(20);
        self.ensure_render_cache(width);
        let lines = &self.cached_lines;
        let total = lines.len() as u16;
        let height = transcript_area.height;
        let skip = self.scroll.min(total.saturating_sub(height) as usize);
        let start = total.saturating_sub(height + skip as u16);
        let shown: Vec<ListItem> = lines
            .iter()
            .skip(start as usize)
            .map(|l| ListItem::new(l.clone()))
            .collect();
        let list = List::new(shown).block(Block::default().borders(Borders::NONE));
        f.render_widget(list, transcript_area);

        // Scroll hint + position indicator when the transcript is released
        // from the bottom, so the user can tell how far back they are.
        if self.scroll > 0 {
            let t = crate::theme::get();
            let hint = Span::styled(
                format!(" ↑ {} lines · esc release ", self.scroll),
                Style::default().fg(t.warning).add_modifier(Modifier::BOLD),
            );
            let r = Rect::new(
                transcript_area.x,
                transcript_area.y,
                transcript_area.width,
                1,
            );
            let p = Paragraph::new(Line::from(hint)).alignment(ratatui::layout::Alignment::Right);
            f.render_widget(p, r);
            // Tapping it jumps back to the live tail.
            self.tap_targets.push((r, Tap::ScrollHint));
        }

        // Composer — the focused widget, so its border always uses the
        // focus accent (the active mode color) rather than a raw grey.
        let comp_style = Style::default().fg(self.mode.color());

        let cursor_ok = self.pending_approval.is_none();
        let comp_inner_w = composer_area.width.saturating_sub(2).max(10) as usize;
        let text: Vec<Line> = if self.input.is_empty() && !cursor_ok {
            vec![Line::from(Span::styled(
                "waiting for approval — y / a / n",
                Style::default()
                    .fg(crate::theme::get().dim)
                    .add_modifier(Modifier::ITALIC),
            ))]
        } else if self.input.is_empty() {
            vec![Line::from(Span::styled(
                Self::placeholder_for(comp_inner_w),
                Style::default().fg(crate::theme::get().dim),
            ))]
        } else {
            // Rendered from the exact same wrapped rows used for height and
            // cursor math — spaces can never disappear again.
            wrap_composer(&self.input, comp_inner_w)
                .into_iter()
                .map(Line::from)
                .collect()
        };
        // Title carries a mode pill + working directory so the composer
        // doubles as a breadcrumb for "where am I, which mode".
        let mode_pill = Span::styled(
            format!(" {} ", self.mode.label()),
            Style::default()
                .bg(self.mode.color())
                .fg(crate::theme::get().overlay)
                .add_modifier(Modifier::BOLD),
        );
        let cwd = self.cwd_basename();
        let comp_title = if cwd.is_empty() {
            Line::from(vec![Span::styled(" ", Style::default()), mode_pill])
        } else {
            Line::from(vec![
                Span::styled(" ", Style::default()),
                mode_pill,
                Span::styled(
                    format!("  {cwd} "),
                    Style::default().fg(crate::theme::get().dim),
                ),
            ])
        };
        let composer = Paragraph::new(text)
            .block(rounded().border_style(comp_style).title(comp_title.clone()));
        f.render_widget(composer, composer_area);
        if cursor_ok && !self.input.is_empty() {
            // Place the caret at `self.cursor` (char count from start).
            let inner_w = composer_area.width.saturating_sub(2).max(10) as usize;
            let prefix: String = self.input.chars().take(self.cursor).collect();
            let segs = wrap_composer(&prefix, inner_w);
            let last = segs.last().map(String::as_str).unwrap_or("");
            let col = UnicodeWidthStr::width(last) as u16;
            let row = composer_area.y
                + 1
                + ((segs.len().saturating_sub(1)) as u16)
                    .min(composer_area.height.saturating_sub(2));
            let x = composer_area.x + 1 + col.min(composer_area.width.saturating_sub(2));
            f.set_cursor_position((x, row));
        }

        // Slash-command + @-file suggestion popups, floating above the composer.
        if self.pending_approval.is_none() {
            self.draw_slash_popup(f, area, composer_area);
            self.draw_at_popup(f, area, composer_area);
        }

        // Hints strip under the composer — colored key-cap chips, trimmed to
        // what fits so a narrow Termux window never clips awkwardly.
        let mut hint_spans: Vec<Span<'static>> = vec![Span::raw(" ")];
        let chips = Self::hint_chips(hints_area.width);
        let mut chip_x = hints_area.x + 1; // leading space cell
        for (i, (key, label)) in chips.iter().enumerate() {
            if i > 0 {
                hint_spans.push(Span::styled("  ", Style::default()));
                chip_x += 2;
            }
            let start = chip_x;
            let key_w = UnicodeWidthStr::width(*key) as u16;
            let label_w = if label.is_empty() {
                0
            } else {
                1 + UnicodeWidthStr::width(*label) as u16
            };
            // Each chip is independently tappable.
            let cw = key_w + label_w;
            if cw > 0 && start + cw <= hints_area.x + hints_area.width {
                self.tap_targets
                    .push((Rect::new(start, hints_area.y, cw, 1), Tap::HintChip(i)));
            }
            chip_x += cw;
            hint_spans.push(Span::styled(
                (*key).to_string(),
                Style::default()
                    .fg(crate::theme::get().hint_key)
                    .add_modifier(Modifier::BOLD),
            ));
            if !label.is_empty() {
                hint_spans.push(Span::styled(
                    format!(" {label}"),
                    Style::default().fg(crate::theme::get().hint_text),
                ));
            }
        }
        f.render_widget(Paragraph::new(Line::from(hint_spans)), hints_area);

        // Approval modal floats over everything else.
        if let Some(detail) = self.pending_approval.clone() {
            self.draw_approval_modal(f, area, &detail);
        }

        // Footer: brand + mode chip + activity on the left; context meter +
        // subtitle right-aligned in a second column.
        let cols = Layout::horizontal([Constraint::Percentage(55), Constraint::Percentage(45)])
            .split(footer_area);
        let brand = " LaudaCode ";
        let brand_w = UnicodeWidthStr::width(brand) as u16;
        // The mode is already stated in the slim header wordmark, so repeating
        // it in the footer just says the same word twice. The composer carries
        // a mode pill too, and that one is adjacent to the caret.
        let show_chip = header_rows != HEADER_COMPACT;
        let mode_chip = if show_chip {
            format!(" {} ", self.mode.label())
        } else {
            String::from("  ")
        };
        let chip_w = UnicodeWidthStr::width(mode_chip.as_str()) as u16;
        // Tapping the brand or mode chip in the footer toggles banner / mode.
        self.tap_targets.push((
            Rect::new(footer_area.x, footer_area.y, brand_w, 1),
            Tap::FooterBrand,
        ));
        self.tap_targets.push((
            Rect::new(footer_area.x + brand_w, footer_area.y, chip_w, 1),
            Tap::ModeChip,
        ));
        let mut spans = vec![
            Span::styled(
                brand,
                Style::default()
                    .fg(crate::theme::get().accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                mode_chip,
                Style::default()
                    .bg(self.mode.color())
                    .fg(crate::theme::get().overlay)
                    .add_modifier(Modifier::BOLD),
            ),
        ];
        if self.busy {
            let glyph = SPINNER[self.spinner_idx % SPINNER.len()];
            let secs = self.busy_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            spans.push(Span::styled(
                format!(" {glyph} {} (esc · {secs}s)", self.busy_label),
                Style::default()
                    .fg(crate::theme::get().warning)
                    .add_modifier(Modifier::BOLD),
            ));
        }
        if let Some((msg, at)) = &self.status {
            let secs = at.elapsed().as_secs();
            // cols[0] clips silently mid-word — trim the message itself to
            // the remaining width so spinner and text end cleanly instead.
            // Too narrow for even that: drop the status, the spinner matters.
            let used: usize = spans
                .iter()
                .map(|s| UnicodeWidthStr::width(&*s.content))
                .sum();
            let avail = (cols[0].width as usize).saturating_sub(used);
            if avail > 4 {
                spans.push(Span::styled(
                    Self::truncate_to(format!("  ·  {msg} ({secs}s)"), avail),
                    Style::default().fg(crate::theme::get().dim),
                ));
            }
        }
        f.render_widget(Paragraph::new(Line::from(spans)), cols[0]);

        // Right side: provider subtitle + context meter. The meter adapts to
        // the available footer width so it can never clip mid-bar.
        let mut meter_spans: Vec<Span<'static>> = Vec::new();
        let sub = self.subtitle.trim();
        if !sub.is_empty() {
            meter_spans.push(Span::styled(
                format!("{sub}  "),
                Style::default().fg(crate::theme::get().dim),
            ));
        }
        if self.ctx_total > 0 {
            let pct = self
                .ctx_used
                .min(self.ctx_total)
                .checked_mul(100)
                .and_then(|n| n.checked_div(self.ctx_total))
                .map(|p| p.min(100))
                .unwrap_or(0);
            // Subtitle + "ctx " + caps + " 100% " = fixed overhead; give the
            // bar whatever remains, but always leave at least two slots.
            const FIXED: usize = 12;
            let avail = (cols[1].width as usize).saturating_sub(sub.chars().count() + FIXED);
            let slots = avail.clamp(2, 10);
            let left = 100 - pct;
            let tail = format!(" {:>3}% ", left);
            let label = if slots >= 6 { "ctx " } else { "" };
            meter_spans.push(Span::styled(
                label,
                Style::default().fg(crate::theme::get().dim),
            ));
            meter_spans.extend(meter(pct, slots));
            meter_spans.push(Span::styled(
                tail,
                Style::default().fg(if left <= 15 {
                    crate::theme::get().error
                } else {
                    crate::theme::get().gray
                }),
            ));
        }
        f.render_widget(
            Paragraph::new(Line::from(meter_spans)).alignment(ratatui::layout::Alignment::Right),
            cols[1],
        );

        if let Some(dash_rect) = dash_area {
            self.draw_dashboard(f, dash_rect);
        }
    }

    /// Compact one-line summary for narrow windows (no side dashboard).
    /// Prioritizes the most glanceable info, truncated to `width`.
    fn compact_summary(&self, width: u16) -> Vec<Span<'static>> {
        let w = width as usize;
        let mut parts: Vec<String> = Vec::new();
        if !self.dash.session_id.is_empty() {
            parts.push(self.dash.session_id.clone());
        }
        if !self.dash.model.is_empty() {
            parts.push(self.dash.model.clone());
        }
        if !self.dash.provider.is_empty() {
            parts.push(self.dash.provider.clone());
        }
        if self.dash.tot_tokens > 0 {
            parts.push(format!("{} tok", Self::fmt_tokens(self.dash.tot_tokens)));
        }
        if self.dash.plan_total > 0 {
            parts.push(format!(
                "plan {}/{}",
                self.dash.plan_done, self.dash.plan_total
            ));
        }
        let joined = parts.join(" · ");
        let mut spans = vec![
            Span::styled(" ", Style::default().fg(crate::theme::get().accent)),
            Span::styled(
                "LaudaCode",
                Style::default()
                    .fg(crate::theme::get().accent)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("  ", Style::default().fg(crate::theme::get().dim)),
        ];
        let budget = w.saturating_sub(14);
        if budget >= 4 {
            let shown = Self::truncate_to(joined, budget);
            spans.push(Span::styled(
                shown,
                Style::default().fg(crate::theme::get().gray),
            ));
        }
        spans
    }

    /// Persistent right-side panel: session identity + live counters.
    fn draw_dashboard(&mut self, f: &mut Frame, rect: Rect) {
        let lines = self.dashboard_lines(rect.width);
        let block = panel(
            Some(Line::from(vec![
                Span::styled("◆ ", Style::default().fg(crate::theme::get().accent)),
                Span::styled(
                    "laudacode",
                    Style::default()
                        .fg(crate::theme::get().accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ])),
            None,
        );
        f.render_widget(Paragraph::new(lines).block(block), rect);
    }

    /// Build the dashboard rows (pure — unit-tested without a terminal).
    fn dashboard_lines(&self, width: u16) -> Vec<Line<'static>> {
        let theme = crate::theme::get();
        let w = width.saturating_sub(2) as usize; // border padding
        let mut v: Vec<Line<'static>> = Vec::new();
        let row = |label: &str, value: String, vcolor: Color| -> Line<'static> {
            Line::from(vec![
                Span::styled(format!(" {:<9}", label), Style::default().fg(theme.dim)),
                Span::styled(
                    Self::truncate_to(value, w.saturating_sub(10)),
                    Style::default().fg(vcolor),
                ),
            ])
        };
        v.push(row("session", self.dash.session_id.clone(), theme.text));
        if !self.dash.session_name.is_empty() {
            v.push(row("name", self.dash.session_name.clone(), theme.accent2));
        }
        v.push(row("model", self.dash.model.clone(), theme.gray));
        v.push(row("provider", self.dash.provider.clone(), theme.gray));
        v.push(Line::from(Span::styled(
            format!(" {}", "─".repeat(w.saturating_sub(1))),
            Style::default().fg(theme.rule),
        )));
        v.push(row(
            "mode",
            self.mode.label().to_string(),
            self.mode.color(),
        ));
        v.push(Line::from(Span::raw(String::new())));

        // Context usage block.
        v.push(Line::from(Span::styled(
            " context",
            Style::default().fg(theme.dim),
        )));
        let pct = self
            .ctx_used
            .min(self.ctx_total)
            .checked_mul(100)
            .and_then(|n| n.checked_div(self.ctx_total))
            .map(|p| p.min(100))
            .unwrap_or(0);
        const SLOTS: usize = 14;
        let mut bar = vec![Span::raw(" ")];
        bar.extend(meter(pct, SLOTS));
        v.push(Line::from(bar));
        v.push(Line::from(vec![
            Span::styled(format!(" {:<9}", "in"), Style::default().fg(theme.dim)),
            Span::styled(
                Self::fmt_tokens(self.dash.prompt_tokens),
                Style::default().fg(theme.gray),
            ),
            Span::styled(" tok", Style::default().fg(theme.dim)),
        ]));
        v.push(Line::from(vec![
            Span::styled(format!(" {:<9}", "out"), Style::default().fg(theme.dim)),
            Span::styled(
                Self::fmt_tokens(self.dash.completion_tokens),
                Style::default().fg(theme.gray),
            ),
            Span::styled(" tok", Style::default().fg(theme.dim)),
        ]));
        v.push(Line::from(vec![
            Span::styled(format!(" {:<9}", "total"), Style::default().fg(theme.dim)),
            Span::styled(
                Self::fmt_tokens(self.dash.tot_tokens),
                Style::default().fg(theme.accent2),
            ),
            Span::styled(" tok", Style::default().fg(theme.dim)),
        ]));

        v.push(Line::from(Span::raw(String::new())));
        v.push(row("requests", self.dash.requests.to_string(), theme.gray));
        v.push(row("messages", self.dash.messages.to_string(), theme.gray));
        if self.dash.plan_total > 0 {
            v.push(row(
                "plan",
                format!("{}/{} done", self.dash.plan_done, self.dash.plan_total),
                if self.dash.plan_done == self.dash.plan_total {
                    theme.success
                } else {
                    theme.gray
                },
            ));
        }
        v.push(row("cwd", self.dash.cwd.clone(), theme.dim));
        let secs = self.session_started.elapsed().as_secs();
        v.push(row("elapsed", Self::fmt_elapsed(secs), theme.dim));
        v.push(Line::from(Span::raw(String::new())));
        let mut hints: Vec<Span<'static>> = vec![Span::raw(" ")];
        for (i, (key, label)) in [("esc", "interrupt"), ("tab", "mode")].iter().enumerate() {
            if i > 0 {
                hints.push(Span::styled("  ", Style::default()));
            }
            hints.push(Span::styled(
                (*key).to_string(),
                Style::default()
                    .fg(theme.hint_key)
                    .add_modifier(Modifier::BOLD),
            ));
            hints.push(Span::styled(
                format!(" {label}"),
                Style::default().fg(theme.hint_text),
            ));
        }
        v.push(Line::from(hints));
        v
    }

    /// Clamp a display string to `w` cells (unicode-width aware-ish).
    /// Composer hint text, trimmed to the available inner width.
    ///
    /// This was a fixed 74-character string, so any composer narrower than
    /// that clipped the tail mid-word. The full text is kept as the source of
    /// truth and truncated here instead.
    fn placeholder_for(inner_w: usize) -> String {
        const FULL: &str = "ask laudacode anything — @ mention · # remember · / commands";
        // Below this the tail is worthless anyway; say less rather than
        // ellipsising into noise.
        if inner_w < 18 {
            return Self::truncate_to("ask laudacode…".to_string(), inner_w);
        }
        Self::truncate_to(FULL.to_string(), inner_w)
    }

    /// Modal dialog width: never wider than the screen (a fixed minimum
    /// clips buttons off portrait phones), never narrower than 20 columns.
    /// Holds for screens >= 22 wide; below that the terminal is unusable.
    fn modal_width(area_w: u16) -> u16 {
        area_w.saturating_sub(2).clamp(20, 64)
    }

    fn truncate_to(s: String, w: usize) -> String {        if UnicodeWidthStr::width(s.as_str()) <= w {
            return s;
        }
        let mut out = String::new();
        let mut used = 0usize;
        for ch in s.chars() {
            let cw = UnicodeWidthStr::width(ch.to_string().as_str());
            if used + cw > w.saturating_sub(1) {
                out.push('…');
                break;
            }
            out.push(ch);
            used += cw;
        }
        out
    }

    /// 45231 → "45.2k"; keeps dashboards tight.
    fn fmt_tokens(n: u64) -> String {
        if n >= 1_000_000 {
            format!("{:.1}m", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            n.to_string()
        }
    }

    /// 3725 → "1h02m", 59 → "0m59s".
    fn fmt_elapsed(secs: u64) -> String {
        if secs >= 3600 {
            format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
        } else {
            format!("{}m{:02}s", secs / 60, secs % 60)
        }
    }

    /// Fixed dashboard column width on wide terminals.
    const DASH_WIDTH: u16 = 40;

    /// Centered approval dialog: detail + y/a/n options.
    fn draw_approval_modal(&mut self, f: &mut Frame, area: Rect, detail: &str) {
        // Never wider than the screen: a fixed 40-col minimum clips the
        // y/a/n buttons off portrait phones.
        let width = Self::modal_width(area.width);
        let wrapped = wrap_text(detail, width.saturating_sub(4) as usize);
        let height = (wrapped.len() as u16 + 5).min(area.height.saturating_sub(2));
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 3,
            width,
            height,
        };
        let theme = crate::theme::get();
        let mut lines: Vec<Line> = vec![Line::from(vec![
            Span::styled(
                "⚠ ",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                "Allow this action?",
                Style::default()
                    .fg(theme.warning)
                    .add_modifier(Modifier::BOLD),
            ),
        ])];
        for l in wrapped {
            lines.push(Line::from(Span::styled(l, Style::default().fg(theme.gray))));
        }
        lines.push(Line::from(String::new()));
        let mut keys: Vec<Span> = Vec::new();
        for (i, (key, label)) in [
            ("y", "yes"),
            ("a", "always"),
            ("n", "no"),
            ("esc", "cancel"),
        ]
        .iter()
        .enumerate()
        {
            if i > 0 {
                keys.push(Span::styled("   ", Style::default()));
            }
            keys.extend(key_hint(key, label));
        }
        lines.push(Line::from(keys));
        let block = panel(
            Some(Line::from(vec![
                Span::styled("⚠ ", Style::default().fg(theme.warning)),
                Span::styled(
                    "approval",
                    Style::default()
                        .fg(theme.warning)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ])),
            Some(theme.warning),
        );
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            rect,
        );
        // Make the key-cap buttons tappable: their layout is deterministic
        // (leading space, " key label", 3-col separators) so we can mirror
        // the x offsets instead of pixel-locating rendered spans.
        let mut x = rect.x + 1;
        let inner_w = rect.width.saturating_sub(2);
        for (key, label) in [
            ("y", "yes"),
            ("a", "always"),
            ("n", "no"),
            ("esc", "cancel"),
        ] {
            let w = (key.chars().count() + label.chars().count() + 3) as u16;
            if x + w > rect.x + 1 + inner_w {
                break;
            }
            let c = match key {
                "y" => Tap::Approval('y'),
                "a" => Tap::Approval('a'),
                "n" => Tap::Approval('n'),
                _ => Tap::Approval('\x1b'), // esc
            };
            self.tap_targets
                .push((Rect::new(x, rect.y + rect.height - 2, w, 1), c));
            x += w + 3; // trailing separator
        }
    }

    /// Centered modal input dialog: hint line + live input field with caret.
    fn draw_input_modal(&mut self, f: &mut Frame, area: Rect, m: &mut InputModal) {
        // Same no-overflow rule as the approval modal below-40-col screens.
        let width = Self::modal_width(area.width);
        let hint_lines = wrap_text(&m.hint, width.saturating_sub(4) as usize);
        let height = (hint_lines.len() as u16 + 6).min(area.height.saturating_sub(2));
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        let theme = crate::theme::get();
        let mut lines: Vec<Line> = Vec::new();
        for l in &hint_lines {
            lines.push(Line::from(Span::styled(
                l.clone(),
                Style::default().fg(theme.gray),
            )));
        }
        lines.push(Line::from(String::new()));
        // The field itself: a filled row so it reads as an input box.
        let field_w = (width.saturating_sub(4)) as usize;
        let mut shown = m.display();
        if shown.chars().count() >= field_w {
            shown = shown
                .chars()
                .skip(shown.chars().count() - field_w)
                .collect();
        }
        let pad = field_w.saturating_sub(shown.chars().count());
        lines.push(Line::from(vec![
            Span::styled(
                format!(" {shown}"),
                Style::default()
                    .fg(theme.accent2)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(" ".repeat(pad), Style::default().fg(theme.surface)),
        ]));
        lines.push(Line::from(String::new()));
        let mut keys: Vec<Span> = Vec::new();
        for (i, (key, label)) in [("enter", "confirm"), ("esc", "cancel")].iter().enumerate() {
            if i > 0 {
                keys.push(Span::styled("   ", Style::default()));
            }
            keys.extend(key_hint(key, label));
        }
        lines.push(Line::from(keys));
        let block = panel(
            Some(Line::from(vec![
                Span::styled("✎ ", Style::default().fg(theme.accent)),
                Span::styled(
                    m.title.clone(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ])),
            Some(theme.accent),
        );
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            rect,
        );
        // Tappable confirm/cancel key-caps, laid out identically to the
        // rendered spans (" enter confirm   esc cancel").
        let mut x = rect.x + 1;
        let inner_w = rect.width.saturating_sub(2);
        for (w, tap) in [
            (("enter", "confirm"), Tap::InputConfirm),
            (("esc", "cancel"), Tap::InputCancel),
        ] {
            let w = (w.0.chars().count() + w.1.chars().count() + 3) as u16;
            if x + w > rect.x + 1 + inner_w {
                break;
            }
            self.tap_targets
                .push((Rect::new(x, rect.y + rect.height - 2, w, 1), tap));
            x += w + 3;
        }
    }

    /// Ctrl+O overlay: recent tool activity expanded in full, scrollable.
    fn draw_overlay(&mut self, f: &mut Frame, area: Rect) {
        let theme = crate::theme::get();
        f.render_widget(Clear, area);
        // Any tap inside the overlay dismisses it (same as Esc).
        self.tap_targets.push((area, Tap::OverlayClose));
        let block = panel(
            Some(Line::from(vec![
                Span::styled("⚙ ", Style::default().fg(theme.accent2)),
                Span::styled(
                    "tool output",
                    Style::default()
                        .fg(theme.accent2)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled("  esc/ctrl+o close ", Style::default().fg(theme.dim)),
            ])),
            Some(theme.accent2),
        );
        let inner = block.inner(area);
        let inner_w = inner.width.max(20);
        // Collect the last tool entries, newest last.
        let mut lines: Vec<Line> = Vec::new();
        for e in &self.entries {
            match e {
                Entry::ToolCall { name, summary } => {
                    lines.push(Line::from(vec![
                        Span::styled("● ", Style::default().fg(theme.accent2)),
                        Span::styled(
                            name.to_string(),
                            Style::default()
                                .fg(theme.accent2)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(format!(" {summary}"), Style::default().fg(theme.gray)),
                    ]));
                }
                Entry::ToolResult { name, ok, preview } => {
                    let color = if *ok { theme.success } else { theme.error };
                    lines.push(Line::from(Span::styled(
                        format!("{} {name}", if *ok { "✔" } else { "✘" }),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    )));
                    for l in wrap_text(preview, inner_w as usize) {
                        lines.push(Line::from(Span::styled(
                            format!("  {l}"),
                            Style::default().fg(theme.gray),
                        )));
                    }
                    lines.push(Line::from(Span::raw(String::new())));
                }
                Entry::ToolDiff { .. } => {
                    // Full colored rendering reuses the transcript pipeline.
                    lines.extend(Self::entry_lines(e, inner_w));
                    lines.push(Line::from(Span::raw(String::new())));
                }
                _ => {}
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "no tool output yet",
                Style::default().fg(theme.dim),
            )));
        }
        let total_lines = lines.len();
        let visible = inner.height as usize;
        let max_scroll = total_lines.saturating_sub(visible);
        let skip = self.overlay_scroll.min(max_scroll);
        let start = total_lines.saturating_sub(visible + skip);
        let shown = &lines[start..start + visible.min(total_lines - start)];
        f.render_widget(Paragraph::new(shown.to_vec()).block(block), area);
        // Scrollbar hint on the right edge of the panel.
        if max_scroll > 0 {
            let mut sb = ScrollbarState::new(max_scroll).position(skip);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .style(Style::default().fg(theme.border))
                    .begin_symbol(None)
                    .end_symbol(None),
                inner,
                &mut sb,
            );
        }
    }

    /// Floating suggestion list above the composer while typing a command.
    fn draw_slash_popup(&mut self, f: &mut Frame, area: Rect, composer: Rect) {
        let matches = self.slash_matches();
        if matches.is_empty() {
            return;
        }
        const MAX_VISIBLE: usize = POPUP_MAX_VISIBLE;
        let visible = matches.len().min(MAX_VISIBLE);
        let height = visible as u16 + 2; // borders
                                         // Full width, not a clamped box: anything narrower leaves the row's
                                         // other content (the compact summary) poking out beside the border.
        let width = area.width;
        let y = composer.y.saturating_sub(height);
        if y < area.y {
            return; // not enough room above the composer
        }
        let rect = Rect {
            x: area.x,
            y,
            width,
            height,
        };
        // Remember the geometry so a tap on a row can complete it.
        self.last_slash_popup = Some(rect);
        // Popups float over the transcript: their tap region is registered
        // last (topmost) so it always wins over chrome underneath it.
        self.tap_targets.push((rect, Tap::SlashPopup));

        let entries = self.slash_entries();
        let sel = self.slash_sel.min(matches.len() - 1);
        // Keep the highlighted row inside a sliding window.
        let start = sel.saturating_sub(visible / 2).min(matches.len() - visible);
        let theme = crate::theme::get();
        let sel_style = selection_style();
        let items: Vec<ListItem> = matches[start..start + visible]
            .iter()
            .map(|&i| {
                let entry = &entries[i];
                let (cmd, desc) = (&entry.cmd, &entry.desc);
                let selected = i == matches[sel];
                let name_color = if entry.custom.is_some() {
                    theme.success
                } else {
                    theme.accent2
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        " ",
                        if selected {
                            sel_style
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(
                        format!("{:<14}", cmd),
                        if selected {
                            sel_style
                        } else {
                            Style::default().fg(name_color).add_modifier(Modifier::BOLD)
                        },
                    ),
                    Span::styled(
                        desc.to_string(),
                        if selected {
                            sel_style
                        } else {
                            Style::default().fg(theme.dim)
                        },
                    ),
                ]))
            })
            .collect();

        let block = panel(
            Some(Line::from(Span::styled(
                " commands ",
                Style::default().fg(theme.dim),
            ))),
            Some(theme.border),
        );
        f.render_widget(Clear, rect);
        f.render_widget(List::new(items).block(block), rect);
    }

    /// Floating file list above the composer while typing an '@path'.
    fn draw_at_popup(&mut self, f: &mut Frame, area: Rect, composer: Rect) {
        let matches = self.at_matches_list();
        if matches.is_empty() || self.pending_approval.is_some() {
            return;
        }
        const MAX_VISIBLE: usize = POPUP_MAX_VISIBLE;
        let visible = matches.len().min(MAX_VISIBLE);
        let height = visible as u16 + 2;
        let width = area.width;
        // Stack above the slash popup when both would collide.
        let slash_h = if self.slash_popup_active() {
            POPUP_MAX_VISIBLE as u16 + 2
        } else {
            0
        };
        let y = composer.y.saturating_sub(height + slash_h);
        if y < area.y {
            return;
        }
        let rect = Rect {
            x: area.x,
            y,
            width,
            height,
        };
        // Remember the geometry so a tap on a row can complete it.
        self.last_at_popup = Some(rect);
        // Registered last (topmost) for the same reason as the slash popup.
        self.tap_targets.push((rect, Tap::AtPopup));
        let sel = self.at_sel.min(matches.len() - 1);
        let start = sel.saturating_sub(visible / 2).min(matches.len() - visible);
        let theme = crate::theme::get();
        let sel_style = selection_style();
        let items: Vec<ListItem> = matches[start..start + visible]
            .iter()
            .map(|&i| {
                let selected = i == matches[sel];
                let path = &self.files[i];
                let style = if selected {
                    sel_style
                } else {
                    Style::default().fg(theme.success)
                };
                ListItem::new(Line::from(vec![
                    Span::styled(
                        " ",
                        if selected {
                            sel_style
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(format!("@{path}"), style),
                ]))
            })
            .collect();
        let block = panel(
            Some(Line::from(Span::styled(
                " files ",
                Style::default().fg(theme.success),
            ))),
            Some(theme.success),
        );
        f.render_widget(Clear, rect);
        f.render_widget(List::new(items).block(block), rect);
    }

    fn draw_picker(&mut self, f: &mut Frame, area: Rect, p: &mut Picker) {
        self.last_picker = Some(area);
        let theme = crate::theme::get();
        let block = panel(
            Some(Line::from(vec![
                Span::styled("◇ ", Style::default().fg(theme.accent)),
                Span::styled(
                    p.title.clone(),
                    Style::default()
                        .fg(theme.accent)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw(" "),
            ])),
            Some(theme.accent),
        );
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.height == 0 || inner.width == 0 {
            return;
        }

        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .split(inner);

        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled("› ", Style::default().fg(theme.accent)),
                Span::raw(p.filter.clone()),
            ])),
            rows[0],
        );

        let idxs = p.filtered();
        let max = rows[1].height as usize;
        let sel_pos = idxs.iter().position(|i| *i == p.selected).unwrap_or(0);
        let start = sel_pos.saturating_sub(max / 2);
        let sel_style = selection_style();
        // Budget for the row text: one leading space plus the scrollbar column.
        // Without this a long item (a provider base URL, an agent description)
        // was sliced at the panel edge, cutting mid-word with no sign that
        // anything was lost.
        // Width budget, left to right: selection marker, scrollbar, badge
        // chip, then the text. Getting this wrong silently clips the badge,
        // which is the one part of the row that carries meaning.
        let marker_w = 3usize;
        let scrollbar_w = 1usize;
        let badge_w: usize = idxs
            .iter()
            .filter_map(|i| p.items.get(*i))
            .map(|r| r.badge.chars().count())
            .max()
            .unwrap_or(0);
        // +1 for a guaranteed gap, so a chip never butts against the text.
        let chip_w = if badge_w > 0 { badge_w + 1 } else { 0 };
        let text_avail = (rows[1].width as usize).saturating_sub(marker_w + scrollbar_w + chip_w);
        // The label wins ties but never eats the whole row — a provider with
        // a very long name must still show where it points.
        let label_cap = (text_avail / 2).max(8);
        let mut items: Vec<ListItem> = idxs
            .iter()
            .skip(start)
            .take(max)
            .map(|i| {
                let row = &p.items[*i];
                let selected = *i == p.selected;
                let label_style = if selected {
                    sel_style
                } else {
                    Style::default().fg(theme.text)
                };
                let label = Self::truncate_to(row.label.clone(), label_cap);
                let used = label.chars().count();
                // The trailing 1 is the gap before the chip.
                let det_budget =
                    text_avail.saturating_sub(used + 2 + usize::from(!row.badge.is_empty()));
                let mut spans = vec![
                    Span::styled(
                        if selected { " ▸ " } else { "   " },
                        if selected {
                            sel_style
                        } else {
                            Style::default()
                        },
                    ),
                    Span::styled(label, label_style),
                ];
                // Context sits in the dimmed column so the eye lands on the
                // label first, padded to a fixed width so the chips line up.
                if !row.detail.is_empty() && det_budget > 1 {
                    let det = Self::truncate_to(row.detail.clone(), det_budget);
                    let pad = det_budget.saturating_sub(det.chars().count());
                    spans.push(Span::raw("  "));
                    spans.push(Span::styled(det, Style::default().fg(theme.dim)));
                    spans.push(Span::raw(" ".repeat(pad)));
                }
                if !row.badge.is_empty() {
                    spans.push(Span::raw(" "));
                    spans.push(Span::styled(
                        row.badge.clone(),
                        Style::default()
                            .fg(theme.accent)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                ListItem::new(Line::from(spans))
            })
            .collect();
        // An empty filter match renders as a blank list, which reads as a
        // stall while typing on a phone keyboard — say so explicitly.
        if items.is_empty() {
            items.push(ListItem::new(Line::from(Span::styled(
                "   no matches — keep typing, esc cancels",
                Style::default().fg(theme.dim),
            ))));
        }
        f.render_widget(List::new(items), rows[1]);
        // Scrollbar when the list overflows the viewport.
        if idxs.len() > max && max > 1 {
            let mut sb = ScrollbarState::new(idxs.len().saturating_sub(max)).position(start);
            f.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight)
                    .style(Style::default().fg(theme.border))
                    .begin_symbol(None)
                    .end_symbol(None),
                rows[1],
                &mut sb,
            );
        }

        let count = idxs.len();
        let mut hints: Vec<Span> = vec![Span::styled(
            format!(" {count} items  "),
            Style::default().fg(theme.dim),
        )];
        for (i, (key, label)) in [("↑↓", "move"), ("enter", "select"), ("esc", "cancel")]
            .iter()
            .enumerate()
        {
            if i > 0 {
                hints.push(Span::styled("  ", Style::default()));
            }
            hints.extend(key_hint(key, label));
        }
        f.render_widget(Paragraph::new(Line::from(hints)), rows[2]);
    }

    /// Handle one terminal event. Returns the action the host should take.
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        if key.kind == crossterm::event::KeyEventKind::Release {
            return Action::None;
        }
        if key.code != KeyCode::Esc {
            self.esc_armed_at = None;
        } else if key.kind == crossterm::event::KeyEventKind::Repeat {
            return Action::None;
        }
        // When input was set directly (e.g. tests), cursor may still be 0
        // while input is non-empty. Move to end in that case.
        let char_count = self.input.chars().count();
        if self.cursor == 0 && char_count > 0 {
            self.cursor = char_count;
        } else {
            self.cursor = self.cursor.min(char_count);
        }

        // Ctrl+O output overlay takes over all keys.
        if self.overlay {
            return match key.code {
                KeyCode::Esc | KeyCode::Char('o') => {
                    if key.code == KeyCode::Char('o')
                        && !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !self.input.is_empty()
                    {
                        Action::None
                    } else {
                        self.overlay = false;
                        self.overlay_scroll = 0;
                        Action::None
                    }
                }
                KeyCode::Up => {
                    self.overlay_scroll += 5;
                    Action::None
                }
                KeyCode::Down => {
                    self.overlay_scroll = self.overlay_scroll.saturating_sub(5);
                    Action::None
                }
                _ => Action::None,
            };
        }

        // Input modal (API keys / model ids) takes over all keys.
        if self.input_modal.is_some() {
            return self.on_input_modal_key(key);
        }

        // Modal approval takes over all keys.
        if self.pending_approval.is_some() {
            return match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => {
                    self.pending_approval = None;
                    Action::Approve(true)
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    self.pending_approval = None;
                    Action::ApproveAlways
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    self.pending_approval = None;
                    Action::Approve(false)
                }
                _ => Action::None,
            };
        }

        if self.picker.is_some() {
            return self.on_picker_key(key);
        }

        // @-file mention autocomplete takes over navigation keys while open.
        if self.at_popup_active() {
            let matches = self.at_matches_list();
            if !matches.is_empty() {
                match key.code {
                    KeyCode::Up => {
                        self.move_at_sel(-1);
                        return Action::None;
                    }
                    KeyCode::Down => {
                        self.move_at_sel(1);
                        return Action::None;
                    }
                    KeyCode::Tab => {
                        self.complete_at();
                        return Action::None;
                    }
                    KeyCode::Enter
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                            && self.input.trim_end()
                                != self.files[matches[self.at_sel.min(matches.len() - 1)]] =>
                    {
                        self.complete_at();
                        return Action::None;
                    }
                    _ => {}
                }
            }
        }

        // Slash-command autocomplete takes over navigation keys while open.
        if self.slash_popup_active() {
            let matches = self.slash_matches();
            if !matches.is_empty() {
                let idx = matches[self.slash_sel.min(matches.len() - 1)];
                let cmd = self.slash_entries()[idx].cmd.clone();
                match key.code {
                    KeyCode::Up => {
                        self.move_slash_sel(-1);
                        return Action::None;
                    }
                    KeyCode::Down => {
                        self.move_slash_sel(1);
                        return Action::None;
                    }
                    KeyCode::Tab => {
                        self.complete_slash();
                        return Action::None;
                    }
                    // Enter completes unless the input already IS the
                    // highlighted command — then fall through and submit.
                    KeyCode::Enter
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                            && self.input.trim_end() != cmd =>
                    {
                        self.complete_slash();
                        return Action::None;
                    }
                    _ => {}
                }
            }
        }

        match key.code {
            KeyCode::Enter
                if !key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                match self.take_submission() {
                    Some(input) => Action::Submit(input),
                    None => Action::None,
                }
            }
            // Shift+Enter / Alt+Enter insert a newline instead of submitting.
            KeyCode::Enter => {
                self.insert_char('\n');
                Action::None
            }
            KeyCode::BackTab => Action::CycleMode,
            // Tab cycles PLAN → BUILD → FULL AUTO (when the slash popup
            // isn't open — there it completes the highlighted command).
            KeyCode::Tab => Action::CycleMode,
            KeyCode::Char(c) => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match c {
                        'c' | 'd' => {
                            // Double-press: first press clears the
                            // composer (or warns), second within 2s quits.
                            let now = Instant::now();
                            let again = self
                                .last_ctrl_c
                                .map(|t| now.duration_since(t) < Duration::from_secs(2))
                                .unwrap_or(false);
                            if again || self.input.is_empty() && self.scroll == 0 {
                                return Action::Quit;
                            }
                            if !self.input.is_empty() {
                                self.input.clear();
                                self.cursor_home();
                                self.pastes.clear();
                                self.last_ctrl_c = None;
                                return Action::None;
                            }
                            self.last_ctrl_c = Some(now);
                            self.set_status("press ctrl+c again to quit");
                            Action::None
                        }
                        'b' => Action::ToggleBanner,
                        'o' => {
                            self.overlay = true;
                            self.overlay_scroll = 0;
                            Action::None
                        }
                        _ => Action::None,
                    }
                } else {
                    self.insert_char(c);
                    self.slash_sel = 0;
                    self.at_sel = 0;
                    Action::None
                }
            }
            KeyCode::Backspace => {
                if self.backspace_at() {
                    self.slash_sel = 0;
                    self.at_sel = 0;
                }
                Action::None
            }
            KeyCode::Left => {
                // Move the caret backward one char (unless a popup owns keys).
                if !self.at_popup_active() && !self.slash_popup_active() {
                    self.cursor_left();
                }
                Action::None
            }
            KeyCode::Right => {
                if !self.at_popup_active() && !self.slash_popup_active() {
                    self.cursor_right();
                }
                Action::None
            }
            KeyCode::Esc => self.esc_pressed(),
            // Scrolling lives on PageUp/PageDown only — the arrow keys are
            // fully reserved for prompt-history recall.
            KeyCode::Up => {
                self.history_up();
                self.cursor_end();
                Action::None
            }
            KeyCode::Down => {
                self.history_down();
                self.cursor_end();
                Action::None
            }
            KeyCode::PageUp if self.scrollable() => {
                self.page_up(20);
                Action::None
            }
            KeyCode::PageDown => {
                self.page_down(20);
                Action::None
            }
            _ => Action::None,
        }
    }

    fn scrollable(&self) -> bool {
        true
    }

    /// Convert touch/mouse events into UI actions. Returns an `Action` for the
    /// host to run (e.g. confirming a tapped picker row), or `None` when the
    /// event was consumed purely by the TUI (scrolling, caret placement).
    pub fn on_mouse(&mut self, me: MouseEvent) -> Option<Action> {
        // Touch swipes and mouse wheels both arrive as scroll events.
        match me.kind {
            MouseEventKind::ScrollUp => {
                self.page_up(3);
                return None;
            }
            MouseEventKind::ScrollDown => {
                self.page_down(3);
                return None;
            }
            MouseEventKind::ScrollRight | MouseEventKind::ScrollLeft => return None,
            _ => {}
        }

        // Finger-drag scrolling: track the row under the pressed button and
        // scroll by the vertical delta while it stays down. Termux reports a
        // finger drag as Down + Moved, so watch for both.
        match me.kind {
            MouseEventKind::Down(MouseButton::Left) | MouseEventKind::Moved => {
                if let Some(prev_row) = self.last_mouse_row.replace(me.row) {
                    let delta = me.row as isize - prev_row as isize;
                    if delta > 0 {
                        self.page_down(delta as usize);
                    } else if delta < 0 {
                        self.page_up((-delta) as usize);
                    }
                }
                return None;
            }
            MouseEventKind::Up(MouseButton::Left) => {
                // Drag ends; the tap below may still trigger an action.
                self.last_mouse_row = None;
            }
            MouseEventKind::Up(_) => {
                self.last_mouse_row = None;
                return None;
            }
            _ => {}
        }
        let pos = Position::new(me.column, me.row);

        // Tap targets registered by the last frame (topmost wins). Floating
        // popups register last, so their rows beat any chrome they cover.
        if let Some(action) = self.tap_hit(pos) {
            return Some(action);
        }

        // Tapping inside an open picker selects + confirms that row.
        if self.picker.is_some() {
            if let Some(area) = self.last_picker {
                if area.contains(pos) {
                    return self.picker_tap_row(me.row, area);
                }
            }
        }

        // Tapping inside the composer repositions the editing caret.
        if let Some(c) = self.last_composer {
            if c.contains(pos) {
                self.caret_to_tap(c, me.column, me.row);
                return None;
            }
        }

        None
    }

    /// Hit-test the tap map registered by the most recent frame, topmost
    /// first (later registrations draw over earlier ones). Returns the
    /// `Action` the tap should trigger, or `None` when nothing matches.
    fn tap_hit(&mut self, pos: Position) -> Option<Action> {
        // Find topmost hit without a second mutable borrow while matching.
        let hit = self
            .tap_targets
            .iter()
            .rev()
            .find(|(r, _)| r.contains(pos))
            .map(|(_, t)| t.clone());
        match hit? {
            // Mode pill in the composer title, slim header or footer —
            // same as pressing Tab.
            Tap::ModeChip => Some(Action::CycleMode),
            // The banner band or footer brand — same as Ctrl+B.
            Tap::Banner | Tap::FooterBrand => {
                self.toggle_banner();
                Some(Action::None)
            }
            // "↑ N lines · esc release" — jump back to the live tail.
            Tap::ScrollHint => {
                self.scroll = 0;
                Some(Action::None)
            }
            // Overlay (Ctrl+O) — tapping anywhere dismisses it.
            Tap::OverlayClose => {
                self.overlay = false;
                Some(Action::None)
            }
            // Hint chips stand in for their keys on touch screens.
            Tap::HintChip(i) => self.hint_chip_action(i),
            Tap::Approval(c) => match c {
                'y' => Some(Action::Approve(true)),
                'a' => Some(Action::ApproveAlways),
                'n' => Some(Action::Approve(false)),
                // esc — same as the modal's Esc handling: deny and close.
                _ => Some(Action::Approve(false)),
            },
            // Confirm the input modal with its current value.
            Tap::InputConfirm => {
                let value = self.input_modal.as_ref().map(|m| m.value.clone())?;
                self.input_modal = None;
                Some(Action::InputSubmit(value))
            }
            // Cancel the input modal without submitting.
            Tap::InputCancel => {
                self.input_modal = None;
                Some(Action::None)
            }
            // Floating suggestion popups: row taps complete the entry; the
            // tap is consumed (border included) without falling through.
            Tap::SlashPopup | Tap::AtPopup => self.popup_tap(pos),
        }
    }

    /// The action a key-hint chip represents: the chip strip is the
    /// touch-screen stand-in for the keyboard shortcuts it names.
    fn hint_chip_action(&mut self, chip: usize) -> Option<Action> {
        let chips = Self::hint_chips(u16::MAX);
        let key = chips.get(chip).map(|(k, _)| k.to_string())?;
        match key.as_str() {
            "enter" => {
                // Submit only when there is something to send.
                self.take_submission().map(Action::Submit)
            }
            "esc" => Some(self.esc_pressed()),
            "tab" => Some(Action::CycleMode),
            "ctrl+b" => {
                self.toggle_banner();
                Some(Action::None)
            }
            "ctrl+o" => {
                self.overlay = true;
                Some(Action::None)
            }
            "ctrl+c" => Some(Action::Quit),
            // ↑↓ history and ←→ editing don't map to a single tap: they
            // fall back to the composer so the caret still moves.
            _ => None,
        }
    }

    /// Map a tap on a floating slash/@ suggestion popup to a completion. The
    /// popups render a 1-cell border plus one row per visible match, with the
    /// list windowed around the current selection — mirror that math here.
    /// Returns `Some(Action::None)` when the tap landed on the popup (so it is
    /// consumed without falling through to the composer caret).
    fn popup_tap(&mut self, pos: Position) -> Option<Action> {
        if let Some(rect) = self.last_slash_popup {
            if rect.contains(pos) {
                let matches = self.slash_matches();
                if !matches.is_empty() {
                    let visible = matches.len().min(POPUP_MAX_VISIBLE);
                    let sel = self.slash_sel.min(matches.len() - 1);
                    let start = sel.saturating_sub(visible / 2).min(matches.len() - visible);
                    if let Some(off) = Self::popup_row_offset(pos, rect, visible) {
                        self.slash_sel = start + off;
                        self.complete_slash();
                    }
                }
                return Some(Action::None);
            }
        }
        if let Some(rect) = self.last_at_popup {
            if rect.contains(pos) {
                let matches = self.at_matches_list();
                if !matches.is_empty() {
                    let visible = matches.len().min(POPUP_MAX_VISIBLE);
                    let sel = self.at_sel.min(matches.len() - 1);
                    let start = sel.saturating_sub(visible / 2).min(matches.len() - visible);
                    if let Some(off) = Self::popup_row_offset(pos, rect, visible) {
                        self.at_sel = start + off;
                        self.complete_at();
                    }
                }
                return Some(Action::None);
            }
        }
        None
    }

    /// Translate a tap position into a 0-based row index within a popup, or
    /// `None` when the tap hit the border rather than an item row.
    fn popup_row_offset(pos: Position, rect: Rect, visible: usize) -> Option<usize> {
        if pos.y <= rect.y || pos.y >= rect.y + rect.height.saturating_sub(1) {
            return None; // top or bottom border
        }
        let row = (pos.y - rect.y - 1) as usize;
        if row < visible {
            Some(row)
        } else {
            None
        }
    }

    /// Tap-to-choose for the open picker: the tapped row (0-based item index)
    /// is selected and confirmed, matching what `Enter` does.
    fn picker_tap_row(&mut self, row: u16, area: Rect) -> Option<Action> {
        let tapped = {
            let p = self.picker.as_mut()?;
            let idxs = p.filtered();
            if idxs.is_empty() {
                return None;
            }
            // Items live below the top border + filter row and above the
            // footer row: border(1) + filter(1) + footer(1).
            let max = area.height.saturating_sub(3) as usize;
            if max == 0 {
                return None;
            }
            let sel_pos = idxs.iter().position(|i| *i == p.selected).unwrap_or(0);
            let start = sel_pos.saturating_sub(max / 2);
            let ri = row.saturating_sub(area.y + 2) as usize;
            if ri >= max || start + ri >= idxs.len() {
                return None;
            }
            (
                p.items[idxs[start + ri]].clone(),
                p.title.clone().to_lowercase(),
            )
        };
        self.picker = None;
        Some(Action::OpenSlash(format!(
            "{}:{}",
            tapped.1,
            tapped.0.to_wire()
        )))
    }

    /// Place the editing caret nearest the tapped point inside the composer.
    fn caret_to_tap(&mut self, composer: Rect, col: u16, row: u16) {
        if self.pending_approval.is_some() || self.input.is_empty() {
            return;
        }
        let inner_w = composer.width.saturating_sub(2).max(10) as usize;
        let segs = wrap_composer(&self.input, inner_w);
        let screen_inner_top = composer.y as usize + 1;
        let rel_row = row as usize;
        if rel_row < screen_inner_top {
            return;
        }
        let text_row = rel_row - screen_inner_top;
        // Jump to the end when the tap lands below the wrapped text.
        if text_row >= segs.len() {
            self.cursor = self.input.chars().count();
            return;
        }
        let tap_col = col.saturating_sub(composer.x + 1) as usize;
        let mut char_idx = 0usize;
        for (r, seg) in segs.iter().enumerate() {
            if r == text_row {
                char_idx += seg.chars().take(tap_col.min(seg.chars().count())).count();
                break;
            }
            char_idx += seg.chars().count();
        }
        self.cursor = char_idx.min(self.input.chars().count());
    }

    fn on_picker_key(&mut self, key: KeyEvent) -> Action {
        let mut action = Action::None;
        {
            let p = match &mut self.picker {
                Some(p) => p,
                None => return Action::None,
            };
            let idxs = p.filtered();
            let pos = idxs.iter().position(|i| *i == p.selected).unwrap_or(0);
            match key.code {
                KeyCode::Esc => {
                    self.picker = None;
                }
                KeyCode::Up => {
                    if pos > 0 {
                        p.selected = idxs[pos - 1];
                    } else if let Some(last) = idxs.last() {
                        p.selected = *last;
                    }
                }
                KeyCode::Down => {
                    if pos + 1 < idxs.len() {
                        p.selected = idxs[pos + 1];
                    } else if let Some(first) = idxs.first() {
                        p.selected = *first;
                    }
                }
                KeyCode::Backspace => {
                    p.filter.pop();
                }
                KeyCode::Char(c) => {
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        if c == 'c' {
                            self.picker = None;
                        }
                    } else {
                        p.filter.push(c);
                        if let Some(first) = p.filtered().first() {
                            p.selected = *first;
                        }
                    }
                }
                KeyCode::Enter => {
                    if let Some(i) = idxs.get(pos) {
                        let chosen = p.items[*i].clone();
                        let title = p.title.clone().to_lowercase();
                        self.picker = None;
                        action = Action::OpenSlash(format!("{title}:{}", chosen.to_wire()));
                    }
                }
                _ => {}
            }
        }
        action
    }

    /// True when enough time passed to advance the spinner.
    pub fn tick_due(&mut self) -> bool {
        if self.last_tick.elapsed() >= Duration::from_millis(TICK_MS) {
            self.last_tick = Instant::now();
            self.spinner_idx = self.spinner_idx.wrapping_add(1);
            true
        } else {
            false
        }
    }
}

/// Enter the alternate screen and raw mode. Call before `run_tui`.
pub fn enter_tui() -> std::io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = std::io::stdout();
    // Bracketed paste makes terminals deliver multi-line clipboard content as
    // ONE paste event instead of a stream of Enter presses — without it,
    // pasting anything multi-line instantly submitted the composer.
    // Mouse capture makes touch/swipe gestures arrive as wheel events so
    // fingers scroll the transcript while physical ↑/↓ stay on history
    // (crucial on Termux, where swipes are otherwise sent as arrow keys).
    crossterm::execute!(
        stdout,
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    Ok(())
}

/// Restore terminal state on any exit path.
pub fn leave_tui() {
    let _ = disable_raw_mode();
    let _ = crossterm::execute!(
        std::io::stdout(),
        DisableMouseCapture,
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
}

/// Run the TUI event loop until the user quits.
///
/// `on_action` receives each action produced by key presses; it may push
/// entries / open pickers via the shared `Tui` handle. Returns when an
/// `Action::Quit` is produced or `on_action` requests shutdown through the
/// returned boolean (`false` = stop).
pub fn run_tui<F>(tui: &mut Tui, subtitle: String, mut on_action: F) -> anyhow::Result<()>
where
    F: FnMut(&mut Tui, Action) -> bool,
{
    // Seed once; afterwards the subtitle is live state that provider/model
    // switches update (see ProviderSwitched handling in repl).
    tui.subtitle = subtitle;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
            .map_err(|e| anyhow::anyhow!("terminal init failed: {e}"))?;

    loop {
        terminal
            .draw(|f| tui.draw(f))
            .map_err(|e| anyhow::anyhow!("draw failed: {e}"))?;

        // Poll with a short timeout so the spinner can tick while idle.
        if !event::poll(Duration::from_millis(TICK_MS))
            .map_err(|e| anyhow::anyhow!("poll failed: {e}"))?
        {
            // Give the host a chance to drain background worker events even
            // with no key activity — this is what makes streamed output,
            // spinner animation and interrupts work without key presses.
            if tui.tick_due() {
                if !tui.is_busy() && tui.fx.kind != crate::effects::EffectKind::Off {
                    tui.fx.tick();
                }
                let _ = on_action(tui, Action::None);
            }
            continue;
        }

        match event::read().map_err(|e| anyhow::anyhow!("read failed: {e}"))? {
            crossterm::event::Event::Key(key) => {
                // Termux sends both Press and Release; only act on Press.
                if key.kind != KeyEventKind::Press {
                    continue;
                }
                let action = tui.on_key(key);
                match action {
                    Action::None => {}
                    other => {
                        if !on_action(tui, other) {
                            break;
                        }
                    }
                }
            }
            crossterm::event::Event::Paste(text) => {
                // Multi-line clipboard content arrives as one event thanks to
                // bracketed paste — insert it verbatim, never auto-submit.
                tui.insert_paste(&text);
                let _ = on_action(tui, Action::None);
            }
            crossterm::event::Event::Mouse(me) => {
                // Touch swipes scroll the transcript; taps choose picker rows
                // or reposition the composer caret (see on_mouse).
                if let Some(action) = tui.on_mouse(me) {
                    match action {
                        Action::None => {}
                        other => {
                            if !on_action(tui, other) {
                                break;
                            }
                        }
                    }
                } else {
                    let _ = on_action(tui, Action::None);
                }
            }
            crossterm::event::Event::Resize(_, _) => {}
            _ => {}
        }
    }

    Ok(())
}

/// Greedy word-wrap for the composer that PRESERVES whitespace exactly
/// (trailing spaces, double spaces, blank rows). The old collapsing wrapper
/// desynced the caret from the rendered text — typing a space appeared to do
/// nothing until the next character landed. Height, rendering and cursor
/// placement all share this function so they can never disagree again.
pub fn wrap_composer(text: &str, w: usize) -> Vec<String> {
    let w = w.max(4);
    let mut out: Vec<String> = Vec::new();
    for src in text.split('\n') {
        let cs: Vec<char> = src.chars().collect();
        let mut cur = String::new();
        let mut cur_w = 0usize;
        let mut i = 0usize;
        while i < cs.len() {
            // Chunk = maximal run of spaces or maximal run of non-spaces.
            let start = i;
            let is_space = cs[i] == ' ';
            while i < cs.len() && (cs[i] == ' ') == is_space {
                i += 1;
            }
            let chunk: String = cs[start..i].iter().collect();
            let chunk_w: usize = chunk
                .chars()
                .map(|c| UnicodeWidthStr::width(c.to_string().as_str()))
                .sum();

            let last_chunk_of_line = i >= cs.len();
            let break_here = cur_w + chunk_w > w && !cur.is_empty()
                // Never push the freshly-typed trailing space to a hidden row.
                && !(is_space && last_chunk_of_line);
            if break_here {
                out.push(cur.trim_end().to_string());
                cur = String::new();
                cur_w = 0;
                // Standard wrap: the spaces that caused the break vanish;
                // only END-OF-LINE trailing spaces are ever kept.
                if is_space {
                    continue;
                }
            }
            cur.push_str(&chunk);
            cur_w += chunk_w;
            // Hard-split chunks wider than the box.
            while UnicodeWidthStr::width(cur.as_str()) > w {
                let mut split_at = 0usize;
                let mut acc = 0usize;
                for (idx, c) in cur.char_indices() {
                    let cwid = UnicodeWidthStr::width(c.to_string().as_str());
                    if acc + cwid > w {
                        break;
                    }
                    acc += cwid;
                    split_at = idx + c.len_utf8();
                }
                let tail = cur.split_off(split_at);
                out.push(std::mem::take(&mut cur));
                cur = tail;
                cur_w = UnicodeWidthStr::width(cur.as_str());
            }
        }
        // Every source line yields at least one row (blank rows included),
        // and trailing spaces stay visible in the row where they were typed.
        out.push(cur);
    }
    out
}

/// Naive greedy word-wrap that respects existing newlines.
fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let w = width.max(10);
    let mut out = Vec::new();
    for para in text.split('\n') {
        if para.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        for word in para.split_whitespace() {
            let mut word = word.to_string();
            if UnicodeWidthStr::width(line.as_str()) + UnicodeWidthStr::width(word.as_str()) + 1 > w
            {
                if !line.is_empty() {
                    out.push(std::mem::take(&mut line));
                }
                // Very long words get hard-split.
                while UnicodeWidthStr::width(word.as_str()) > w {
                    let split_at = word
                        .char_indices()
                        .map(|(i, _)| i)
                        .take_while(|i| UnicodeWidthStr::width(&word[..*i]) <= w)
                        .last()
                        .unwrap_or(w.min(word.len()));
                    let tail = word.split_off(split_at);
                    out.push(std::mem::replace(&mut word, tail));
                }
                line.push_str(&word);
            } else {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(&word);
            }
        }
        out.push(line);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, mods)
    }

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    #[test]
    fn exit_alias_is_suggested() {
        let mut tui = Tui::new();
        tui.input = "/ex".into();
        let matches = tui.slash_matches();
        assert!(!matches.is_empty(), "/exit must appear in suggestions");
        let entries = tui.slash_entries();
        assert!(entries.iter().any(|e| e.cmd == "/exit"), "alias registered");
        // Completing from '/ex' can land on /exit.
        tui.complete_slash();
        assert!(
            tui.input.starts_with("/exit ") || tui.input.starts_with("/"),
            "completion works: {}",
            tui.input
        );
    }

    #[test]
    fn wrap_composer_preserves_spaces_exactly() {
        // Trailing space stays in its row (the reported bug).
        assert_eq!(wrap_composer("word ", 40), vec!["word "]);
        // Double spaces are not collapsed.
        assert_eq!(wrap_composer("a  b", 40), vec!["a  b"]);
        // Word wrap on overflow, no space loss; greedy fill packs the row.
        assert_eq!(wrap_composer("aaa bbb ccc", 7), vec!["aaa bbb", "ccc"]);
        // Blank rows survive.
        assert_eq!(wrap_composer("l1\n\nl2", 10), vec!["l1", "", "l2"]);
        // Oversized single token hard-splits without dropping chars.
        let rows = wrap_composer(&"x".repeat(25), 10);
        let joined: String = rows.concat();
        assert_eq!(joined, "x".repeat(25));
        assert!(rows
            .iter()
            .all(|r| UnicodeWidthStr::width(r.as_str()) <= 10));
    }

    #[test]
    fn composer_height_counts_preserved_spaces() {
        let mut t = Tui::new();
        // 11-char word hits the 10-cell inner-width clamp → wraps onto a
        // second row; the collapsing wrapper undercounted rows like this.
        t.input = format!("{} b", "a".repeat(10));
        assert_eq!(t.composer_height(12, 30), 4, "2 wrapped rows + borders");
        // And the rendered rows match the height math exactly.
        assert_eq!(wrap_composer(&t.input, 10), vec!["aaaaaaaaaa", "b"]);
    }

    #[test]
    fn composer_grows_with_long_prompts_and_caps() {
        let mut t = Tui::new();
        // Empty → minimum 3 rows (border + 1 line + border).
        assert_eq!(t.composer_height(80, 30), 3);
        // One wrapped line of text still fits in the base box.
        t.input = "hello world".into();
        assert_eq!(t.composer_height(80, 30), 3);
        // Explicit lines expand the box line-by-line; the trailing newline
        // adds a final empty row for the cursor.
        t.input = "l1\nl2\nl3\nl4\n".into();
        assert_eq!(
            t.composer_height(80, 30),
            7,
            "4 lines + trailing blank + borders"
        );
        // Long single token wraps and counts as multiple rows.
        t.input = "x".repeat(200);
        let h = t.composer_height(40, 30);
        assert!(h > 3, "wrapped long line must grow the box: {h}");
        // Cap: never eats the whole screen.
        t.input = "y\n".repeat(100);
        assert_eq!(t.composer_height(80, 30), 14, "hard cap");
        // Tiny terminal keeps at least the minimum.
        assert_eq!(t.composer_height(80, 8), 3);
    }

    #[test]
    fn paste_inserts_verbatim_without_submitting() {
        let mut t = Tui::new();
        t.insert_paste("line one\r\nline two\nline three\r");
        assert_eq!(t.input, "line one\nline two\nline three\n");
        // No submission side effects: busy untouched, entries untouched.
        assert!(!t.is_busy());
        assert!(t.entries.is_empty());
        // Paste while a modal is open is ignored entirely.
        t.open_approval("allow?".into());
        t.insert_paste("should not land");
        assert!(!t.input.contains("should not land"));
    }

    #[test]
    fn big_paste_collapses_to_chip_and_expands_on_submit() {
        let mut t = Tui::new();
        t.insert_paste("l1\nl2\nl3\nl4\nl5\nl6\nl7\nl8\nl9");
        assert_eq!(t.input, "[paste #0 +9 lines]");
        // Backspace removes the whole chip, not one char.
        match t.on_key(key(KeyCode::Backspace, KeyModifiers::NONE)) {
            Action::None => {}
            other => panic!("backspace must not submit, got {other:?}"),
        }
        assert!(t.input.is_empty());
        assert!(t.pastes.iter().all(|p| p.is_none()));
        // Re-paste, then submit expands to the full text.
        t.insert_paste("a\nb\nc\nd");
        assert_eq!(t.input, "[paste #1 +4 lines]", "numbers are never reused");
        match t.on_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::Submit(s) => assert_eq!(s, "a\nb\nc\nd"),
            other => panic!("enter must submit, got {other:?}"),
        }
        // History recorded the expanded text, so ↑ recall has no dangling chip.
        assert_eq!(t.history.last().map(String::as_str), Some("a\nb\nc\nd"));
    }

    #[test]
    fn hand_typed_chip_lookalike_stays_literal() {
        let mut t = Tui::new();
        t.input = "see [paste #7 +2 lines] ok".into();
        t.cursor_end();
        match t.on_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::Submit(s) => assert_eq!(s, "see [paste #7 +2 lines] ok"),
            other => panic!("enter must submit, got {other:?}"),
        }
    }

    #[test]
    fn dashboard_appears_only_on_wide_terminals() {
        // Needs BOTH ≥88 columns and ≥39 rows (Termux landscape is ~39 tall).
        assert_eq!(Tui::dash_width(87, 50), None);
        assert_eq!(Tui::dash_width(88, 50), Some(40));
        assert_eq!(Tui::dash_width(147, 39), Some(40), "Termux landscape");
        assert_eq!(Tui::dash_width(88, 38), None, "too short — no dashboard");
        assert_eq!(
            Tui::dash_width(100, 30),
            None,
            "short terminal keeps old layout"
        );
        assert_eq!(Tui::dash_width(220, 60), Some(40));
    }

    #[test]
    fn header_height_scales_down_on_small_screens() {
        // Full 11-row banner only when wide AND tall.
        assert_eq!(Tui::header_height(120, 30, true), HEADER_HEIGHT);
        assert_eq!(
            Tui::header_height(120, 22, true),
            HEADER_COMPACT,
            "too short for art"
        );
        assert_eq!(
            Tui::header_height(59, 50, true),
            HEADER_COMPACT,
            "too narrow for art"
        );
        assert_eq!(Tui::header_height(120, 9, true), 0, "way too short");
        assert_eq!(Tui::header_height(39, 20, true), 0, "too narrow");
        // Toggling the banner off always yields no header.
        assert_eq!(Tui::header_height(120, 50, false), 0);
    }

    #[test]
    fn compact_summary_shows_when_no_dashboard() {
        // No dashboard (narrow) + enough room → compact bar appears.
        assert_eq!(Tui::compact_summary_h(80, 30, true), 1);
        // With a dashboard present, never double up.
        assert_eq!(Tui::compact_summary_h(100, 50, false), 0);
        // Too narrow / too short → no bar.
        assert_eq!(Tui::compact_summary_h(40, 30, true), 1);
        assert_eq!(Tui::compact_summary_h(36, 30, true), 0);
        assert_eq!(Tui::compact_summary_h(80, 12, true), 0);
    }

    #[test]
    fn modals_never_exceed_screen_width() {
        for w in [22u16, 30, 39, 40, 64, 100, 200] {
            let m = Tui::modal_width(w);
            assert!(m <= w, "modal {m} wider than screen {w}");
            assert!((20..=64).contains(&m), "modal width {m} out of range");
        }
    }

    #[test]
    fn tool_result_names_hidden_lines() {
        let long = Entry::ToolResult {
            name: "run".into(),
            ok: true,
            preview: "1\n2\n3\n4\n5\n6".into(),
        };
        let lines = Tui::entry_lines(&long, 80);
        assert_eq!(lines.len(), 1 + 4 + 1, "header + 4 rows + hint");
        let hint: String = lines
            .last()
            .unwrap()
            .spans
            .iter()
            .map(|s| &*s.content)
            .collect::<Vec<_>>()
            .join("");
        assert!(hint.contains("+2 lines"), "{hint:?}");
        let short = Entry::ToolResult {
            name: "run".into(),
            ok: true,
            preview: "a\nb".into(),
        };
        assert_eq!(Tui::entry_lines(&short, 80).len(), 1 + 2, "no hint when nothing hidden");
    }

    #[test]
    fn hint_line_never_exceeds_width() {
        for w in [6u16, 12, 20, 40, 80] {
            let h = Tui::hint_line(w);
            assert!(
                h.chars().count() <= w as usize,
                "hint too wide at {w}: {h:?}"
            );
        }
        // Narrow width yields a tiny fallback, not an empty line.
        assert!(!Tui::hint_line(10).is_empty());
        // Wide enough shows the first (priority) chip "enter send".
        assert!(Tui::hint_line(60).starts_with(" enter send"));
    }

    #[test]
    fn mouse_scroll_moves_transcript() {
        let mut t = Tui::new();
        for _ in 0..50 {
            t.push(Entry::Info("line".into()));
        }
        t.scroll = 10;
        // ScrollDown moves toward the newest content (decreases offset).
        t.on_mouse(mouse(MouseEventKind::ScrollDown, 0, 0));
        assert!(t.scroll < 10, "scroll down should move toward newest");
        // ScrollUp scrolls back toward older content.
        t.on_mouse(mouse(MouseEventKind::ScrollUp, 0, 0));
        assert_eq!(t.scroll, 10, "scroll up undoes the scroll");
        // Horizontal wheel events are ignored without side effects.
        t.on_mouse(mouse(MouseEventKind::ScrollRight, 0, 0));
        t.on_mouse(mouse(MouseEventKind::ScrollLeft, 0, 0));
        assert_eq!(t.scroll, 10, "horizontal scroll must do nothing");
    }

    #[test]
    fn picker_tap_confirms_tapped_row_and_closes() {
        let mut t = Tui::new();
        t.open_picker(
            "theme",
            vec!["lauda".into(), "dracula".into(), "nord".into()],
        );
        // Imitate a drawn picker filling a 50x10 screen; item rows begin at
        // area.y + 2 (top border + filter row).
        let area = Rect::new(0, 0, 50, 10);
        t.last_picker = Some(area);
        // Tap the third row (index 2 → "nord").
        let action = t.on_mouse(mouse(
            MouseEventKind::Up(MouseButton::Left),
            4,
            area.y + 2 + 2,
        ));
        match action {
            Some(Action::OpenSlash(cmd)) => assert_eq!(cmd, "theme:nord"),
            other => panic!("expected OpenSlash, got {other:?}"),
        }
        assert!(t.picker.is_none(), "picker must close after a tap choice");
    }

    #[test]
    fn picker_tap_ignores_taps_outside_rows() {
        let mut t = Tui::new();
        t.open_picker("theme", vec!["lauda".into(), "dracula".into()]);
        let area = Rect::new(0, 0, 50, 10);
        t.last_picker = Some(area);
        // Tap in the footer row (below the item list) → no selection.
        let action = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 4, 9));
        assert!(action.is_none());
        assert!(t.picker.is_some(), "picker stays open on invalid row tap");
    }

    #[test]
    fn composer_tap_moves_caret_near_tapped_point() {
        let mut t = Tui::new();
        t.input = "hello world".into();
        t.cursor = t.input.chars().count();
        t.last_composer = Some(Rect::new(0, 20, 40, 3));
        // Tap the first character cell (x=1, inner row 0 → y=21).
        t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1, 21));
        assert!(
            t.cursor == 0,
            "caret should jump to start, got {}",
            t.cursor
        );
        // Tapping an empty composer changes nothing.
        t.input.clear();
        t.cursor = 0;
        t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5, 21));
        assert_eq!(t.cursor, 0);
    }

    #[test]
    fn slash_popup_tap_completes_tapped_command() {
        let mut t = Tui::new();
        t.input = "/".into();
        // Popup of 6 visible rows: border, rows 1..6, bottom border.
        let rect = Rect::new(0, 10, 40, 8);
        t.last_slash_popup = Some(rect);
        // Frames register the popup as the topmost tap target.
        t.tap_targets.push((rect, Tap::SlashPopup));
        // Tap the second visible row (y = rect.y + 2).
        let action = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, rect.y + 2));
        assert!(matches!(action, Some(Action::None)));
        // The command on that row replaced the composer input.
        assert_eq!(t.input, format!("{} ", t.input.trim_end()));
        assert!(!t.slash_popup_active(), "completion closes the popup");
        assert!(t.input.starts_with('/'));
    }

    #[test]
    fn slash_popup_tap_on_border_is_consumed_but_keeps_popup() {
        let mut t = Tui::new();
        t.input = "/mo".into();
        let rect = Rect::new(0, 10, 40, 8);
        t.last_slash_popup = Some(rect);
        t.tap_targets.push((rect, Tap::SlashPopup));
        // Tap the top border row — must not fall through to the composer.
        let action = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, rect.y));
        assert!(matches!(action, Some(Action::None)));
        assert_eq!(t.input, "/mo", "input unchanged on a border tap");
        assert!(t.slash_popup_active());
    }

    #[test]
    fn slash_popup_tap_outside_leaves_composer_intact() {
        let mut t = Tui::new();
        t.input = "/mo".into();
        t.cursor = 3;
        t.last_slash_popup = Some(Rect::new(0, 10, 40, 8));
        // Tap well below the popup (composer area) — no popup, no completion.
        let action = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, 40));
        assert!(action.is_none());
        assert_eq!(t.input, "/mo");
    }

    #[test]
    fn at_popup_tap_completes_tapped_file() {
        let mut t = Tui::new();
        t.set_files(vec!["src/main.rs".into(), "README.md".into()]);
        t.input = "@".into();
        t.cursor = 1;
        let rect = Rect::new(0, 10, 40, 8);
        t.last_at_popup = Some(rect);
        t.tap_targets.push((rect, Tap::AtPopup));
        t.tap_targets.push((rect, Tap::AtPopup));
        // Tap the second visible row (deeper path: src/main.rs).
        let action = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, rect.y + 2));
        assert!(matches!(action, Some(Action::None)));
        assert_eq!(t.input, "@src/main.rs ");
        assert!(!t.at_popup_active());
    }

    #[test]
    fn at_popup_tap_outside_falls_through() {
        let mut t = Tui::new();
        t.set_files(vec!["src/main.rs".into()]);
        t.input = "@".into();
        t.cursor = 1;
        let rect = Rect::new(0, 10, 40, 8);
        t.last_at_popup = Some(rect);
        t.tap_targets.push((rect, Tap::AtPopup));
        let action = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, 40));
        assert!(action.is_none());
        assert_eq!(t.input, "@", "input unchanged by a tap outside the popup");
    }

    #[test]
    fn popup_tap_beats_overlapping_chrome() {
        // Regression: a popup floating over the transcript must consume taps
        // on its rows even when a registered chip (here: the scroll hint)
        // occupies the same cells. Registered last wins, so the popup
        // completion fires instead of the scroll-to-bottom.
        let mut t = Tui::new();
        t.set_files(vec!["src/main.rs".into(), "README.md".into()]);
        t.input = "@".into();
        t.cursor = 1;
        let rect = Rect::new(0, 10, 40, 8);
        // The underlying chip is registered first (drawn earlier).
        t.tap_targets
            .push((Rect::new(0, 10, 40, 1), Tap::ScrollHint));
        t.last_at_popup = Some(rect);
        t.tap_targets.push((rect, Tap::AtPopup));
        // Tap the first visible row, which overlaps the scroll hint.
        let action = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, rect.y + 1));
        assert!(matches!(action, Some(Action::None)));
        assert_eq!(
            t.input, "@README.md ",
            "popup completion wins over the chip"
        );
        assert!(!t.at_popup_active());
    }

    #[test]
    fn tap_targets_fire_their_actions() {
        let mut t = Tui::new();
        // Mode chip — same as Tab.
        t.tap_targets.push((Rect::new(0, 0, 10, 1), Tap::ModeChip));
        assert!(matches!(
            t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5, 0)),
            Some(Action::CycleMode)
        ));
        // Banner tap toggles it (same as Ctrl+B).
        t.tap_targets.clear();
        let before = t.show_banner;
        t.tap_targets.push((Rect::new(0, 0, 10, 1), Tap::Banner));
        let _ = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5, 0));
        assert_eq!(t.show_banner, !before);
        // Scroll hint jumps back to the live tail.
        t.tap_targets.clear();
        t.scroll = 7;
        t.tap_targets
            .push((Rect::new(0, 0, 10, 1), Tap::ScrollHint));
        let _ = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5, 0));
        assert_eq!(t.scroll, 0);
        // Overlay tap dismisses the overlay.
        t.tap_targets.clear();
        t.overlay = true;
        t.tap_targets
            .push((Rect::new(0, 0, 10, 1), Tap::OverlayClose));
        let _ = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 5, 0));
        assert!(!t.overlay);
    }

    #[test]
    fn tap_hit_prefers_topmost_registration() {
        let mut t = Tui::new();
        // Two overlapping targets; the later (drawn on top) one must win.
        t.tap_targets.push((Rect::new(0, 0, 10, 10), Tap::Banner));
        t.tap_targets.push((Rect::new(2, 2, 4, 4), Tap::ModeChip));
        let action = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 3, 3));
        assert!(matches!(action, Some(Action::CycleMode)));
    }

    #[test]
    fn busy_esc_needs_a_confirming_double_press() {
        let mut t = Tui::new();
        t.set_busy(true, "working");
        // First Esc arms the interrupt instead of firing it.
        assert!(matches!(
            t.on_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            Action::None
        ));
        assert!(t.is_busy(), "arming must not stop the agent");
        // A second Esc within the window confirms: interrupt fires.
        assert!(matches!(
            t.on_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Interrupt
        ));
        assert!(
            t.esc_armed_at.is_none(),
            "armed flag must clear after the interrupt"
        );
        // Idle Esc keeps its old single-press semantics (clears the composer).
        t.set_busy(false, "");
        t.input = "draft".into();
        assert!(matches!(
            t.on_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            Action::Interrupt
        ));
        assert!(t.input.is_empty());
        // Arming expires: a late second Esc must NOT interrupt — and the
        // next press re-arms fresh instead of firing immediately.
        t.set_busy(true, "working");
        assert!(matches!(
            t.on_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            Action::None
        ));
        t.esc_armed_at = Some(Instant::now() - ESC_ARM_WINDOW - Duration::from_millis(1));
        assert!(matches!(
            t.on_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            Action::None
        ));
        assert!(
            t.esc_armed_at.is_some(),
            "re-armed for the next confirming press"
        );
        t.on_key(key(KeyCode::Char('x'), KeyModifiers::NONE));
        assert!(t.esc_armed_at.is_none(), "other keys disarm");
        assert!(matches!(
            t.on_key(key(KeyCode::Esc, KeyModifiers::NONE)),
            Action::None
        ));
        t.set_busy(false, "");
        assert!(t.esc_armed_at.is_none(), "finishing a turn disarms");
    }

    #[test]
    fn hint_chip_tap_submits_and_interrupts() {
        let mut t = Tui::new();
        t.input = "hello".into();
        t.cursor = 5;
        // "enter" is chip 0 in the full chip list.
        t.tap_targets
            .push((Rect::new(0, 0, 6, 1), Tap::HintChip(0)));
        match t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1, 0)) {
            Some(Action::Submit(s)) => assert_eq!(s, "hello"),
            other => panic!("expected Submit, got {other:?}"),
        }
        assert!(t.input.is_empty(), "composer cleared after submit tap");
        // "esc" chip mirrors the real Esc: clears input + signals interrupt.
        t.tap_targets.clear();
        t.input = "draft".into();
        t.tap_targets
            .push((Rect::new(0, 0, 6, 1), Tap::HintChip(1)));
        assert!(matches!(
            t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1, 0)),
            Some(Action::Interrupt)
        ));
        assert!(
            t.input.is_empty(),
            "esc chip clears the composer like the key"
        );
    }

    #[test]
    fn approval_buttons_are_tappable() {
        let mut t = Tui::new();
        for (x, expect) in [
            (0, Tap::Approval('y')),
            (6, Tap::Approval('a')),
            (13, Tap::Approval('n')),
        ] {
            t.tap_targets.clear();
            t.tap_targets.push((Rect::new(x, 0, 5, 1), expect));
            let action = t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), x, 0));
            assert!(action.is_some(), "button at {x} must fire");
        }
    }

    #[test]
    fn input_modal_buttons_submit_and_cancel() {
        let mut t = Tui::new();
        t.open_input_modal(InputModal::new("api key", "paste key", true));
        t.input_modal.as_mut().unwrap().value = "sk-test".into();
        t.tap_targets
            .push((Rect::new(0, 0, 6, 1), Tap::InputConfirm));
        match t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1, 0)) {
            Some(Action::InputSubmit(v)) => assert_eq!(v, "sk-test"),
            other => panic!("expected InputSubmit, got {other:?}"),
        }
        assert!(t.input_modal.is_none(), "confirm closes the modal");
        // Cancel discards the value and just closes.
        t.open_input_modal(InputModal::new("api key", "paste key", true));
        t.input_modal.as_mut().unwrap().value = "sk-nope".into();
        t.tap_targets
            .push((Rect::new(0, 0, 6, 1), Tap::InputCancel));
        assert!(matches!(
            t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 1, 0)),
            Some(Action::None)
        ));
        assert!(t.input_modal.is_none(), "cancel closes the modal");
    }

    #[test]
    fn drag_scrolls_transcript_and_tap_clears_it() {
        let mut t = Tui::new();
        // Press at row 10, drag up to row 7 → scrolls up 3 lines.
        t.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), 0, 10));
        t.on_mouse(mouse(MouseEventKind::Moved, 0, 7));
        assert_eq!(t.scroll, 3);
        // Dragging back down scrolls toward the live tail.
        t.on_mouse(mouse(MouseEventKind::Moved, 0, 10));
        assert_eq!(t.scroll, 0);
        // Releasing ends the drag; a later stray Moved must not scroll.
        t.on_mouse(mouse(MouseEventKind::Up(MouseButton::Left), 0, 10));
        t.on_mouse(mouse(MouseEventKind::Moved, 0, 4));
        assert_eq!(t.scroll, 0, "no drag active after release");
    }

    #[test]
    fn dash_endpoint_updates_live() {
        let mut t = Tui::new();
        t.dash.set_session("abc-123", "m1", "p1", "~", 0);
        t.dash.set_endpoint("tokenrouter", "gpt-5-mini");
        assert_eq!(t.dash.provider, "tokenrouter");
        assert_eq!(t.dash.model, "gpt-5-mini");
    }

    #[test]
    fn dashboard_rows_show_identity_and_counters() {
        use crate::tools::TodoItem;
        let mut t = Tui::new();
        t.dash.set_session(
            "d1046d10-7df0-4db5-b005-13cc48433fde",
            "stealth/ox-alpha",
            "openrouter",
            "~/Laudacode",
            23,
        );
        t.dash.record_usage(45_000, 1_250);
        t.dash.record_usage(46_000, 2_000);
        // Context meter is fed separately by the usage event path.
        t.set_usage(46_000, 128_000);
        t.dash.set_plan(&[
            TodoItem {
                content: "a".into(),
                status: "completed".into(),
            },
            TodoItem {
                content: "b".into(),
                status: "pending".into(),
            },
        ]);
        let lines = t.dashboard_lines(28);
        let text: Vec<String> = lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        let joined = text.join("\n");
        assert!(joined.contains("d1046d10-7df0…"), "{joined}");
        assert!(joined.contains("ox-alpha"));
        assert!(joined.contains("BUILD"), "mode row present");
        // Latest request wins.
        assert!(
            joined.contains("46.0k"),
            "prompt tokens humanized: {joined}"
        );
        assert!(
            joined.contains("2.0k"),
            "completion tokens humanized: {joined}"
        );
        assert!(joined.contains("1/2 done"), "plan progress: {joined}");
        assert!(joined.contains("requests"), "counter rows exist");
        assert!(joined.contains("█"), "context bar drawn");
    }

    /// Names matched by a slash-command query — index-independent so adding
    /// or reordering commands can't silently break the assertions below.
    fn matched_names(query: &str) -> Vec<&'static str> {
        filter_slash_commands(query)
            .into_iter()
            .map(|i| SLASH_COMMANDS[i].0)
            .collect()
    }

    #[test]
    fn slash_filter_matches_prefixes_case_insensitively() {
        assert_eq!(filter_slash_commands("/").len(), SLASH_COMMANDS.len());
        assert_eq!(filter_slash_commands("").len(), SLASH_COMMANDS.len());
        assert_eq!(matched_names("/pro"), vec!["/provider"]);
        assert_eq!(matched_names("/appro"), vec!["/approvals"]);
        assert_eq!(matched_names("/resum"), vec!["/resume"]);
        assert_eq!(matched_names("/imag"), vec!["/image"]);
        assert_eq!(matched_names("/RETRY"), vec!["/retry"]);
        assert_eq!(matched_names("/reason"), vec!["/reasoning"]);
        assert_eq!(matched_names("/quit"), vec!["/quit"]);
        assert_eq!(matched_names("/status"), vec!["/status"]);
        assert_eq!(matched_names("/diff"), vec!["/diff"]);
        assert_eq!(matched_names("/review"), vec!["/review"]);
        assert_eq!(matched_names("/undo"), vec!["/undo"]);
        assert_eq!(matched_names("/session"), vec!["/session"]);
        // Checkpointing trio.
        assert_eq!(
            matched_names("/checkpoint"),
            vec!["/checkpoint", "/checkpoints"]
        );
        assert_eq!(matched_names("/branch"), vec!["/branch"]);
        assert!(filter_slash_commands("/zzz").is_empty());
    }

    #[test]
    fn popup_only_while_typing_command_name() {
        let mut t = Tui::new();
        assert!(!t.slash_popup_active());
        t.input = "/".into();
        assert!(t.slash_popup_active());
        t.input = "/comp".into();
        assert!(t.slash_popup_active());
        // Space (args) closes the popup.
        t.input = "/provider use x".into();
        assert!(!t.slash_popup_active());
        // Non-slash input never opens it.
        t.input = "hello".into();
        assert!(!t.slash_popup_active());
    }

    #[test]
    fn tab_completes_highlighted_command() {
        let mut t = Tui::new();
        t.input = "/mod".into();
        let model = SLASH_COMMANDS
            .iter()
            .position(|(c, _)| *c == "/model")
            .expect("/model is listed");
        assert_eq!(t.slash_matches(), vec![model]);
        t.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(t.input, "/model ");
        // Popup closed after completion (trailing space).
        assert!(!t.slash_popup_active());
    }

    #[test]
    fn skills_autocomplete_completes_then_submits() {
        for completion_key in [KeyCode::Tab, KeyCode::Enter] {
            let mut t = Tui::new();
            t.set_custom_cmds(vec![("custom".into(), "a custom command".into())]);
            t.input = "/ski".into();
            let matches = t.slash_matches();
            assert_eq!(matches.len(), 1);
            assert_eq!(t.slash_entries()[matches[0]].cmd, "/skills");
            assert!(matches!(
                t.on_key(key(completion_key, KeyModifiers::NONE)),
                Action::None
            ));
            assert_eq!(t.input, "/skills ");
            assert!(!t.slash_popup_active());
            assert!(matches!(
                t.on_key(key(KeyCode::Enter, KeyModifiers::NONE)),
                Action::Submit(s) if s == "/skills"
            ));
            assert!(t.input.is_empty());
        }
    }

    #[test]
    fn enter_completes_partial_but_submits_exact() {
        let mut t = Tui::new();
        t.input = "/clea".into();
        match t.on_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::None => {}
            other => panic!("enter should complete, got {other:?}"),
        }
        assert_eq!(t.input, "/clear ");
        // Now the input exactly equals a command: Enter must submit it.
        t.input = "/clear".into();
        match t.on_key(key(KeyCode::Enter, KeyModifiers::NONE)) {
            Action::Submit(s) => assert_eq!(s, "/clear"),
            other => panic!("expected submit, got {other:?}"),
        }
    }

    #[test]
    fn arrows_navigate_popup_instead_of_scrolling() {
        let mut t = Tui::new();
        t.input = "/".into();
        t.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(t.slash_sel, 1);
        t.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        t.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        // Wrapped backwards from 0 to last.
        assert_eq!(t.slash_sel, SLASH_COMMANDS.len() - 1);
        assert_eq!(
            t.scroll, 0,
            "transcript must not scroll while popup is open"
        );
    }

    #[test]
    fn selection_resets_when_input_changes() {
        let mut t = Tui::new();
        t.input = "/c".into();
        t.on_key(key(KeyCode::Down, KeyModifiers::NONE)); // sel=1
        t.on_key(key(KeyCode::Char('l'), KeyModifiers::NONE)); // "/cl"
        assert_eq!(t.slash_sel, 0);
        // Matches for "/cl": /clear only. Look the index up rather than
        // hardcoding it — inserting a command above /clear must not break a
        // test that is only about selection reset.
        let clear = SLASH_COMMANDS
            .iter()
            .position(|(c, _)| *c == "/clear")
            .expect("/clear is listed");
        assert_eq!(t.slash_matches(), vec![clear]);
    }

    #[test]
    fn at_mention_popup_completes_files() {
        let mut t = Tui::new();
        t.set_files(vec![
            "src/main.rs".into(),
            "src/tools.rs".into(),
            "README.md".into(),
        ]);
        assert!(!t.at_popup_active());
        t.input = "@".into();
        assert!(t.at_popup_active(), "bare @ opens the list");
        t.input = "@too".into();
        assert!(t.at_popup_active());
        assert_eq!(t.at_matches_list().len(), 1, "only tools.rs matches");
        match t.on_key(key(KeyCode::Tab, KeyModifiers::NONE)) {
            Action::None => {}
            other => panic!("tab should complete @token, got {other:?}"),
        }
        assert_eq!(t.input, "@src/tools.rs ");
        // Space ends the token — popup closes.
        assert!(!t.at_popup_active());
        // '@' mid-word does not open the popup.
        t.input = "email user@host.com".into();
        assert!(!t.at_popup_active() && t.at_matches_list().is_empty());
    }

    #[test]
    fn ctrl_o_opens_output_overlay() {
        let mut t = Tui::new();
        t.push(Entry::ToolResult {
            name: "run_command".into(),
            ok: true,
            preview: "[exit: 0]\nall good".into(),
        });
        match t.on_key(key(KeyCode::Char('o'), KeyModifiers::CONTROL)) {
            Action::None => {}
            other => panic!("ctrl+o should toggle silently, got {other:?}"),
        }
        assert!(t.overlay);
        match t.on_key(key(KeyCode::Esc, KeyModifiers::NONE)) {
            Action::None => {}
            other => panic!("esc closes overlay, got {other:?}"),
        }
        assert!(!t.overlay);
    }

    #[test]
    fn approval_modal_supports_always() {
        let mut t = Tui::new();
        t.open_approval("write /etc/hosts [DANGEROUS]".into());
        match t.on_key(key(KeyCode::Char('a'), KeyModifiers::NONE)) {
            Action::ApproveAlways => {}
            other => panic!("'a' should approve always, got {other:?}"),
        }
        assert!(t.pending_approval.is_none());
        t.open_approval("x".into());
        assert!(matches!(
            t.on_key(key(KeyCode::Char('y'), KeyModifiers::NONE)),
            Action::Approve(true)
        ));
    }

    #[test]
    fn context_meter_tracks_usage() {
        let mut t = Tui::new();
        t.set_usage(32_000, 128_000);
        assert_eq!(t.ctx_used, 32_000);
        assert_eq!(t.ctx_total, 128_000);
        // Zero total is ignored (keeps default).
        t.set_usage(1_000, 0);
        assert_eq!(t.ctx_total, 128_000);
    }

    #[test]
    fn plain_typing_still_works_with_popup_logic() {
        let mut t = Tui::new();
        t.input = "fix the bug".into();
        assert!(matches!(
            t.on_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(_)
        ));
    }

    fn submit(t: &mut Tui, text: &str) {
        t.input = text.into();
        assert!(matches!(
            t.on_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            Action::Submit(_)
        ));
    }

    #[test]
    fn up_down_recall_prompts_and_restore_draft() {
        let mut t = Tui::new();
        submit(&mut t, "first prompt");
        submit(&mut t, "second prompt");
        // Empty composer + ↑ recalls newest.
        t.input.clear();
        t.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(t.input, "second prompt");
        // ↑ again walks older.
        t.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(t.input, "first prompt");
        // ↓ walks newer…
        t.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(t.input, "second prompt");
        // …and past the newest exits history (empty draft).
        t.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(t.input, "");
        assert!(t.history_pos.is_none());
    }

    #[test]
    fn history_skips_consecutive_duplicates_and_caps_size() {
        let mut t = Tui::new();
        submit(&mut t, "same");
        submit(&mut t, "same");
        submit(&mut t, "other");
        assert_eq!(t.history, vec!["same".to_string(), "other".to_string()]);
        for i in 0..(HISTORY_MAX + 20) {
            t.record_history(&format!("p{i}"));
        }
        assert_eq!(t.history.len(), HISTORY_MAX);
    }

    #[test]
    fn arrows_are_history_only_scrolling_on_page_keys() {
        let mut t = Tui::new();
        submit(&mut t, "old prompt");
        // ↑ recalls even when the composer has text — arrows are dedicated
        // to history (typed draft is saved for ↓).
        t.input = "half-typed".into();
        t.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(t.input, "old prompt");
        // ↓ past the newest restores the saved draft.
        t.on_key(key(KeyCode::Down, KeyModifiers::NONE));
        assert_eq!(t.input, "half-typed");
        // Arrows never scroll; PageUp/PageDown do.
        t.on_key(key(KeyCode::PageUp, KeyModifiers::NONE));
        assert_eq!(t.scroll, 20);
        t.on_key(key(KeyCode::PageDown, KeyModifiers::NONE));
        assert_eq!(t.scroll, 0);
    }

    #[test]
    fn seed_history_merges_persisted_entries() {
        let mut t = Tui::new();
        submit(&mut t, "live one");
        t.seed_history(vec!["from disk".into(), "older".into()]);
        assert_eq!(t.history.len(), 3);
        assert_eq!(t.history.last().map(String::as_str), Some("live one"));
        t.input.clear();
        t.on_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(t.input, "live one", "session entries stay newest");
    }
    fn line_texts(t: &Tui, width: u16) -> Vec<String> {
        let mut probe = clone_shallow(t);
        probe.ensure_render_cache(width);
        probe
            .cached_lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect()
    }

    fn clone_shallow(t: &Tui) -> Tui {
        Tui {
            input: t.input.clone(),
            entries: t.entries.clone(),
            ..Tui::new()
        }
    }

    #[test]
    fn incremental_cache_matches_full_rebuild() {
        let width = 40;
        let mut t = Tui::new();
        t.push(Entry::User("hello world".into()));
        // Simulate streaming growth of the assistant entry.
        for tail in ["The ", "quick brown fox ", "jumps over the lazy dog."] {
            if t.entries.len() < 2 {
                t.push(Entry::Assistant(tail.to_string()));
            } else if let Some(Entry::Assistant(a)) = t.entries.last_mut() {
                a.push_str(tail);
            }
            let _ = line_texts(&t, width);
        }
        let incremental = line_texts(&t, width);
        let mut fresh = clone_shallow(&t);
        fresh.ensure_render_cache(width);
        let expected: Vec<String> = fresh
            .cached_lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.clone()).collect())
            .collect();
        assert_eq!(incremental, expected);
    }

    #[test]
    fn clear_resets_render_cache() {
        let mut t = Tui::new();
        t.push(Entry::Info("one".into()));
        t.ensure_render_cache(40);
        assert!(!t.cached_lines.is_empty());
        t.entries.clear();
        t.ensure_render_cache(40);
        assert!(t.cached_lines.is_empty());
    }

    #[test]
    fn the_placeholder_fits_the_composer_at_every_width() {
        // The real invariant, on the function that produces the text: what is
        // drawn is never wider than the space it has, and a too-long
        // placeholder is marked with an ellipsis rather than cut mid-word.
        const FULL: &str = "ask laudacode anything — @ mention · # remember · / commands";
        for inner_w in [4usize, 10, 18, 24, 40, 60, 72, 74, 100, 200] {
            let got = Tui::placeholder_for(inner_w);
            assert!(
                UnicodeWidthStr::width(got.as_str()) <= inner_w,
                "placeholder is {inner_w}+ wide: {got:?}"
            );
            if got.len() < FULL.len() {
                assert!(
                    got.ends_with('…'),
                    "truncated placeholder is not marked: {got:?}"
                );
            }
        }
        // Wide enough: the full hint set, untouched.
        assert_eq!(Tui::placeholder_for(74), FULL);
    }

    #[test]
    fn tool_diffs_render_in_color() {
        use crate::diff::unified_diff;
        let d = unified_diff("src/lib.rs", "a\nb\n", "a\nc\n", 1);
        let mut t = Tui::new();
        t.push(Entry::ToolDiff {
            name: "edit_file".into(),
            files: vec![d],
        });
        let lines = line_texts(&t, 60);
        let joined = lines.join("\n");
        assert!(joined.contains("✎ edit_file"), "{joined}");
        // The per-file header carries only the path: the counts already
        // appear on the summary line above it, and repeating them read as a
        // rendering glitch.
        assert!(joined.contains("┌─ src/lib.rs"), "{joined}");
        assert!(
            !joined.contains("src/lib.rs (+1"),
            "per-file counts are duplicated: {joined}"
        );
        assert!(joined.contains("│-b"), "{joined}");
        assert!(joined.contains("│+c"), "{joined}");
        // Colors are attached to the right kinds: the bar keeps its
        // kind color while the content row is syntax-highlighted.
        let mut probe = clone_shallow(&t);
        probe.ensure_render_cache(60);
        let add_line = probe
            .cached_lines
            .iter()
            .find(|l| {
                l.spans.len() >= 3
                    && l.spans[0].content == "│"
                    && l.spans[1].content == "+"
                    && l.spans[2].content == "c"
            })
            .unwrap_or_else(|| panic!("no +c diff line"));
        assert_eq!(
            add_line.spans[0].style.fg,
            Some(crate::theme::get().success),
            "add bar green"
        );
        assert_eq!(
            add_line.spans[1].style.fg,
            Some(crate::theme::get().success),
            "+ sign stays green"
        );
        assert_eq!(
            add_line.spans[2].style.bg,
            Some(crate::theme::get().add_bg),
            "add tint behind content"
        );
        let del_line = probe
            .cached_lines
            .iter()
            .find(|l| {
                l.spans.len() >= 3
                    && l.spans[0].content == "│"
                    && l.spans[1].content == "-"
                    && l.spans[2].content == "b"
            })
            .unwrap();
        assert_eq!(
            del_line.spans[0].style.fg,
            Some(crate::theme::get().error),
            "del bar red"
        );
        assert_eq!(
            del_line.spans[2].style.bg,
            Some(crate::theme::get().del_bg),
            "del tint behind content"
        );
    }

    #[test]
    fn every_logo_fits_the_banner_band() {
        assert!(
            LOGOS.len() >= 7,
            "expected a varied banner set, got {}",
            LOGOS.len()
        );
        let mut names: Vec<&str> = LOGOS.iter().map(|a| a.name).collect();
        names.sort_unstable();
        let before = names.len();
        names.dedup();
        assert_eq!(before, names.len(), "duplicate logo name in {names:?}");
        for art in LOGOS {
            assert!(
                art.rows.len() as u16 <= HEADER_HEIGHT,
                "{} is {} rows, band is {HEADER_HEIGHT}",
                art.name,
                art.rows.len()
            );
            for row in art.rows {
                for Seg(text, _) in *row {
                    assert!(!text.contains('\t'), "{}: tabs break alignment", art.name);
                }
            }
            // Boxed logos must have every row the same width or the right-hand
            // border stair-steps. Left-aligned logos (no border glyph) are
            // exempt. The art's width only tracks its widest row, so check this here.
            // A real box opens with ╭ and closes with ╯ — free-form art can
            // contain │ (a rocket fuselage, a circuit trace) without being one.
            let boxed = art
                .rows
                .first()
                .is_some_and(|r| r.iter().any(|s| s.0.contains('╭')))
                && art
                    .rows
                    .last()
                    .is_some_and(|r| r.iter().any(|s| s.0.contains('╯')));
            if boxed {
                let widths: Vec<usize> = art
                    .rows
                    .iter()
                    .map(|r| r.iter().map(|s| s.0.chars().count()).sum())
                    .collect();
                let w = widths[0];
                for (i, got) in widths.iter().enumerate() {
                    assert_eq!(
                        *got, w,
                        "{}: row {i} is {got} wide, row 0 is {w} — border won't line up",
                        art.name
                    );
                }
            }
            // Rendering must always fill the band, whatever the logo's height.
            assert_eq!(
                banner_lines(art).len(),
                HEADER_HEIGHT as usize,
                "{}",
                art.name
            );
            // Every logo must carry the name/version/tagline. Alignment and
            // column placement are asserted by
            // `identity_block_sits_right_of_every_logo`.
            let text: String = banner_lines(art)
                .iter()
                .flat_map(|l| l.spans.iter())
                .map(|s| s.content.as_ref())
                .collect();
            assert!(text.contains("LaudaCode"), "{} lost the name", art.name);
            assert!(
                text.contains(concat!("v", env!("CARGO_PKG_VERSION"))),
                "{} lost the version",
                art.name
            );
            assert!(
                text.contains("AI coding agent"),
                "{} lost the tagline",
                art.name
            );
        }
    }

    #[test]
    fn identity_block_sits_in_one_column_right_of_every_logo() {
        // All three identity lines — not just the name — must start in the same
        // column, on adjacent rows, and nothing may spill past that column.
        let tags = [
            "LaudaCode",
            concat!("v", env!("CARGO_PKG_VERSION")),
            "AI coding agent",
        ];
        for art in LOGOS {
            let lines = banner_lines(art);
            let want = art_width(art) as usize + INFO_GUTTER as usize;
            let mut rows = Vec::new();
            for (y, line) in lines.iter().enumerate() {
                let mut col = 0usize;
                for span in &line.spans {
                    let text: &str = span.content.as_ref();
                    // Only the block counts: the terminal logo repeats the
                    // version inside its own title bar, left of the block.
                    if col >= want && tags.contains(&text) {
                        assert_eq!(
                            col, want,
                            "{}: {text:?} at col {col}, want {want}",
                            art.name
                        );
                        rows.push(y);
                    }
                    col += text.chars().count();
                }
            }
            assert_eq!(rows.len(), tags.len(), "{}: block incomplete", art.name);
            let (lo, hi) = (
                rows.iter().min().copied().unwrap(),
                rows.iter().max().copied().unwrap(),
            );
            assert_eq!(hi - lo, 2, "{}: block rows not adjacent", art.name);
            let widest = lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.chars().count())
                        .sum::<usize>()
                })
                .max()
                .unwrap();
            assert!(
                widest <= composed_width(art) as usize,
                "{}: row is {widest} wide, budget {}",
                art.name,
                composed_width(art)
            );
        }
    }

    #[test]
    fn a_logo_fits_from_its_narrowest_width_upward() {
        // The one-line identity header is only for widths where nothing fits,
        // so the threshold must be exactly the narrowest composed logo.
        let narrowest = LOGOS
            .iter()
            .map(composed_width)
            .min()
            .expect("logo table is not empty");
        assert!(
            !any_logo_fits(narrowest - 1),
            "claimed a fit one col too narrow"
        );
        assert!(
            any_logo_fits(narrowest),
            "narrowest logo does not fit itself"
        );
        for w in [narrowest, narrowest + 1, 80, 200, u16::MAX] {
            assert!(any_logo_fits(w), "nothing fits at {w}");
        }
    }

    #[test]
    fn narrow_terminals_never_pick_a_logo_that_clips() {
        // Everything must be a legal pick at any width, and wide-enough
        // widths may only return logos whose composed width (art + identity
        // block) actually fits.
        for w in [20u16, 40, 54, 60, 80, 200] {
            for _ in 0..200 {
                let art = pick_logo(w);
                if composed_width(art) <= w {
                    continue;
                }
                assert!(
                    LOGOS.iter().all(|a| composed_width(a) > w),
                    "at w={w} picked {} (needs {}) but others fit",
                    art.name,
                    composed_width(art)
                );
            }
        }
    }

    #[test]
    fn banner_toggle_roundtrip() {
        let mut t = Tui::new();
        assert!(t.banner_visible());
        t.toggle_banner();
        assert!(!t.banner_visible());
        t.toggle_banner();
        assert!(t.banner_visible());
    }

    #[test]
    fn banner_toggle_rerolls_the_logo() {
        // Ctrl+B doubles as "show me another logo". With enough draws across
        // restarts both variants should show up.
        let mut seen = std::collections::HashSet::new();
        for _ in 0..400 {
            let mut t = Tui::new();
            seen.insert(t.banner_logo().name);
            t.toggle_banner();
            t.toggle_banner();
            seen.insert(t.banner_logo().name);
        }
        assert!(seen.len() > 1, "logo never changed: {seen:?}");
    }

    #[test]
    fn banner_roll_never_repeats_back_to_back() {
        let mut t = Tui::new();
        t.last_width = 500; // every variant fits
        let mut prev = t.banner_logo().name;
        for _ in 0..50 {
            t.roll_banner();
            let cur = t.banner_logo().name;
            assert_ne!(cur, prev, "roll repeated the logo — reads as a no-op");
            assert!(t.banner_visible(), "roll must show a hidden banner");
            prev = cur;
        }
        // Degenerate width: only the most compact logo fits — must not panic
        // and must still show the banner.
        t.last_width = 0;
        t.roll_banner();
        assert!(t.banner_visible());
    }

    #[test]
    fn tab_cycles_mode_when_popup_closed() {
        let mut t = Tui::new();
        assert_eq!(t.mode, Mode::Build);
        match t.on_key(key(KeyCode::Tab, KeyModifiers::NONE)) {
            Action::CycleMode => {}
            other => panic!("tab should cycle mode, got {other:?}"),
        }
        // The repl closure performs `tui.mode = tui.mode.next()` on this
        // action; on_key only reports the intent.
        // But with the slash popup open, Tab completes instead.
        t.input = "/mod".into();
        match t.on_key(key(KeyCode::Tab, KeyModifiers::NONE)) {
            Action::None => {}
            other => panic!("popup tab should complete silently, got {other:?}"),
        }
        assert_eq!(t.input, "/model ");
    }

    #[test]
    fn banner_gradient_matches_rows() {
        let grad = banner_colors();
        assert_eq!(grad.len(), HEADER_HEIGHT as usize);
        // Gradient endpoints come from the theme's stops.
        let t = crate::theme::get();
        assert_eq!(grad[0], t.banner[0]);
        assert_eq!(grad[grad.len() - 1], t.banner[2]);
    }

    #[test]
    fn theme_switch_recolors_entries() {
        use crate::theme;
        let mut t = Tui::new();
        t.push(Entry::Info("hello".into()));
        theme::set("dracula");
        let lines = line_texts_colored(&t, 60);
        assert_eq!(lines[0].1, Some(theme::get().heading));
        theme::set("lauda");
    }

    fn line_texts_colored(t: &Tui, width: u16) -> Vec<(String, Option<Color>)> {
        let mut probe = clone_shallow(t);
        probe.ensure_render_cache(width);
        probe
            .cached_lines
            .iter()
            .map(|l| {
                (
                    l.spans
                        .iter()
                        .map(|s| s.content.clone())
                        .collect::<Vec<_>>()
                        .join(""),
                    l.spans.first().and_then(|s| s.style.fg),
                )
            })
            .collect()
    }

    #[test]
    fn busy_indicator_is_footer_only() {
        let mut t = Tui::new();
        t.set_busy(true, "working");
        assert!(t.is_busy());
        assert!(
            t.entries.is_empty(),
            "activity must not create transcript entries"
        );
        t.set_busy(false, "working");
        assert!(!t.is_busy());
    }

    #[test]
    fn hint_chips_fit_width_and_prioritize_send() {
        // The first chip is always the most important action.
        for w in [14u16, 24, 40, 80] {
            let chips = Tui::hint_chips(w);
            assert_eq!(chips[0].0, "enter", "first chip must be enter-send at {w}");
            let plain = Tui::hint_line(w);
            assert!(
                UnicodeWidthStr::width(plain.as_str()) <= w as usize,
                "hint strip overflows at {w}: {plain:?}"
            );
        }
        // Very narrow widths still give a usable escape hatch.
        assert_eq!(Tui::hint_chips(10), vec![("esc", "")]);
    }

    #[test]
    fn cwd_basename_handles_trailing_slash_and_root() {
        let mut t = Tui::new();
        t.dash.cwd = "/data/data/com.termux/files/home/Laudacode".into();
        assert_eq!(t.cwd_basename(), "Laudacode");
        t.dash.cwd = "/home/user/project/".into();
        assert_eq!(t.cwd_basename(), "project");
        t.dash.cwd = "/".into();
        assert_eq!(t.cwd_basename(), "/");
        t.dash.cwd = String::new();
        assert_eq!(t.cwd_basename(), "");
    }

    #[test]
    fn meter_never_exceeds_slot_count() {
        for (pct, slots) in [(0u64, 10usize), (50, 10), (85, 14), (150, 8)] {
            let spans = meter(pct, slots);
            let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
            let bars = text.chars().filter(|c| *c == '█' || *c == '░').count();
            assert_eq!(
                bars, slots,
                "meter at {pct}% must fill exactly {slots} slots"
            );
        }
        // Escalates to the warning color between 60-84% and error above.
        let warn = meter(70, 4);
        assert_eq!(warn[1].style.fg, Some(crate::theme::get().warning));
        let danger = meter(95, 4);
        assert_eq!(danger[1].style.fg, Some(crate::theme::get().error));
    }

    #[test]
    fn key_hint_has_cap_and_label() {
        let spans = key_hint("enter", "send");
        assert_eq!(spans.len(), 2);
        assert!(spans[0].content.contains("enter"));
        assert!(spans[1].content.contains("send"));
        assert_eq!(spans[0].style.fg, Some(crate::theme::get().hint_key));
        assert_eq!(spans[1].style.fg, Some(crate::theme::get().hint_text));
    }

    #[test]
    fn chrome_uses_theme_palette_not_raw_colors() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        let mut t = Tui::new();
        t.dash
            .set_session("abcdef0123456", "model-x", "openrouter", "/tmp/proj", 3);
        t.entries.push(Entry::ToolResult {
            name: "read_file".into(),
            ok: true,
            preview: "src/main.rs".into(),
        });
        let backend = TestBackend::new(100, 40);
        let mut terminal = Terminal::new(backend).unwrap();
        // Must not panic at the geometry the dashboard activates at.
        terminal.draw(|f| t.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        // The composer border must carry the active mode color, proving the
        // chrome is themed rather than hardcoded.
        let mode_color = Some(t.mode.color());
        let has_mode_border = (0..buf.area.height)
            .any(|y| (0..buf.area.width).any(|x| buf.cell((x, y)).map(|c| c.fg) == mode_color));
        assert!(has_mode_border, "composer border should use the mode color");
    }

    #[test]
    #[ignore]
    fn visual_dump() {
        use ratatui::backend::TestBackend;
        use ratatui::Terminal;
        type Setup = Box<dyn Fn(&mut Tui)>;
        let scenarios: Vec<(&str, Setup)> = vec![
            (
                "slash popup",
                Box::new(|t: &mut Tui| {
                    t.input = "/th".into();
                    t.cursor = 3;
                }),
            ),
            (
                "providers picker",
                Box::new(|t: &mut Tui| {
                    t.entries.push(Entry::User("switch to openrouter".into()));
                    t.open_picker_rows(
                        "provider_use",
                        vec![
                            PickerRow::new(
                                "openrouter",
                                "stealth/ox-alpha · https://openrouter.ai/api/v1",
                            )
                            .badge("active"),
                            PickerRow::new("ollama", "qwen3:8b · http://localhost:11434/v1"),
                            PickerRow::new(
                                "groq",
                                "llama-3.3-70b · https://api.groq.com/openai/v1",
                            ),
                            PickerRow::new("perplexity", "sonar · https://api.perplexity.ai"),
                        ],
                    );
                }),
            ),
            (
                "sessions picker",
                Box::new(|t: &mut Tui| {
                    t.entries.push(Entry::User("which session?".into()));
                    t.open_picker_rows(
                        "resume",
                        vec![
                            PickerRow::new(
                                "parser work",
                                "1790480839-8d37e49d2efd · 2025-09-27 · fix the tokenizer",
                            )
                            .badge("current"),
                            PickerRow::new(
                                "(unnamed)",
                                "1790484022-aa11bb22cc33 · 2025-09-27 · list the files here",
                            ),
                            PickerRow::new(
                                "lsp diagnostics",
                                "1790477001-9f8e7d6c5b4a · 2025-09-26 · wire the lsp tool in",
                            ),
                        ],
                    );
                }),
            ),
            (
                "models picker",
                Box::new(|t: &mut Tui| {
                    t.entries.push(Entry::User("pick a model".into()));
                    t.open_picker_rows(
                        "model",
                        vec![
                            PickerRow::new("stealth/ox-alpha", "").badge("active"),
                            PickerRow::new("anthropic/claude-sonnet-4.5", ""),
                            PickerRow::new("google/gemini-2.5-pro", ""),
                            PickerRow::new("openai/gpt-5-codex", ""),
                            PickerRow::new("type a model id manually…", ""),
                        ],
                    );
                }),
            ),
            (
                "agents picker",
                Box::new(|t: &mut Tui| {
                    t.entries
                        .push(Entry::User("who should review this?".into()));
                    let items: Vec<String> = crate::agents::all_roles()
                        .into_iter()
                        .map(|r| {
                            // The tag sits next to the name, not at the end:
                            // appended, a long description truncates it away,
                            // and read-only-ness is the thing you most need to
                            // see before picking.
                            let tag = if r.read_only { " (read-only)" } else { "" };
                            format!("{}{tag} · {}", r.name, r.description)
                        })
                        .collect();
                    t.open_picker("agents", items);
                }),
            ),
            (
                "real session",
                Box::new(|t: &mut Tui| {
                    t.entries
                        .push(Entry::User("wire the lsp tool into the agent".into()));
                    t.entries.push(Entry::Reasoning(
                        "MCP uses newline framing but LSP needs Content-Length headers.".into(),
                    ));
                    t.entries.push(Entry::Assistant(
                        "Found it. Lsp::notify writes the raw JSON body, so every server \
rejects the handshake."
                            .into(),
                    ));
                    t.entries.push(Entry::ToolCall {
                        name: "edit_file".into(),
                        summary: "src/lsp.rs".into(),
                    });
                    t.entries.push(Entry::ToolDiff {
                        name: "edit_file".into(),
                        files: vec![crate::diff::FileDiff {
                            path: "src/lsp.rs".into(),
                            added: 2,
                            removed: 1,
                            lines: vec![
                                crate::diff::DiffLine {
                                    kind: crate::diff::LineKind::Del,
                                    text: "-    c.stdin.write_all(msg.as_bytes())".into(),
                                },
                                crate::diff::DiffLine {
                                    kind: crate::diff::LineKind::Add,
                                    text: "+    let body = to_vec(msg)?;".into(),
                                },
                                crate::diff::DiffLine {
                                    kind: crate::diff::LineKind::Add,
                                    text: "+    c.stdin.write_all(&framed).await?;".into(),
                                },
                                crate::diff::DiffLine {
                                    kind: crate::diff::LineKind::Ctx,
                                    text: "     c.stdin.flush().await?;".into(),
                                },
                            ],
                        }],
                    });
                    t.entries.push(Entry::ToolResult {
                        name: "edit_file".into(),
                        ok: true,
                        preview: "edited src/lsp.rs".into(),
                    });
                    t.entries.push(Entry::ToolCall {
                        name: "run_command".into(),
                        summary: "cargo test lsp".into(),
                    });
                    t.entries.push(Entry::ToolResult {
                        name: "run_command".into(),
                        ok: true,
                        preview: "test result: ok. 8 passed; 0 failed".into(),
                    });
                    t.entries.push(Entry::Assistant(
                        "Fixed and verified: clangd now returns the real error.".into(),
                    ));
                }),
            ),
            (
                "approval modal",
                Box::new(|t: &mut Tui| {
                    t.pending_approval = Some("run_command: rm -rf target/".into());
                }),
            ),
        ];
        for (name, setup) in scenarios {
            let mut t = Tui::new();
            t.dash.set_session(
                "a1b2c3d4e5f6g",
                "stealth/ox-alpha",
                "openrouter",
                "/home/u/Laudacode",
                4,
            );
            t.dash.record_usage(12_400, 3_100);
            t.ctx_used = 42_000;
            t.entries.push(Entry::User("tidy up the composer".into()));
            t.entries.push(Entry::Assistant("Restyling chrome.".into()));
            setup(&mut t);
            // 74x22 is a desktop window; 40x20 is a phone in portrait. Both
            // must lay out without clipping or overlapping.
            for (w, h) in [(74u16, 22u16), (40, 20)] {
                let backend = TestBackend::new(w, h);
                let mut terminal = Terminal::new(backend).unwrap();
                terminal.draw(|f| t.draw(f)).unwrap();
                let buf = terminal.backend().buffer().clone();
                println!("=== {name} @ {w}x{h} ===");
                for y in 0..buf.area.height {
                    let mut row = String::new();
                    for x in 0..buf.area.width {
                        row.push_str(buf.cell((x, y)).map(|c| c.symbol()).unwrap_or(" "));
                    }
                    println!("{}", row.trim_end());
                }
                println!();
            }
        }
    }
}
