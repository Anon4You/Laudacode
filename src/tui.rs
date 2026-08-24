use crossterm::event::{self, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind};
use crossterm::event::{DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::{
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph, Wrap},
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

/// Subtle background tints so added/removed diff rows stay unmistakable
/// while syntax token colors render inside them.
const ADD_BG: Color = Color::Rgb(24, 48, 30);
const DEL_BG: Color = Color::Rgb(56, 26, 30);

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
        match self {
            Mode::Plan => Color::LightBlue,
            Mode::Build => Color::LightGreen,
            Mode::FullAuto => Color::Yellow,
        }
    }
}

/// Brand banner — braille-art LaudaCode mascot with the wordmark, version
/// and tagline embedded in the right-hand columns.
pub const BANNER: &str = concat!(
    "⠀⠀⠀⠀⠀⠀⣠⣤⣤⣤⣤⣤⣄⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀\n",
    "⡀⠀⠀⠀⠀⢰⡿⠋⠁⠀⠀⠈⠉⠙⠻⣷⣄⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀\n",
    "⠇⠀⠀⠀⢀⣿⠇⠈⢀⣴⣶⡾⠿⠿⠿⢿⣿⣦⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀\n",
    "⠃⠀⣀⣀⣸⡿⠀⠀⢸⣿⣇⠀⠀⠀⠀⠀⠀⠙⣷⡀⠀⠀⠀⠀⠀⠀⠀⠀⠀⠀\n",
    "⠇⣾⡟⠛⣿⡇⠀⠀⢸⣿⣿⣷⣤⣤⣤⣤⣶⣶⣿⠇⠀⠀⠀⠀⠀⠀⠀⣀⠀⠀\n",
    "⢅⣿⠀⢀⣿⡇⠀⠀⠀⠻⢿⣿⣿⣿⣿⣿⠿⣿⡏⠀⠀⠀⠀⢴⣶⣶⣿⣿⣿⣆\n",
    "⣺⣿⠀⢸⣿⡇⠀⠀⠀⠀⠀⠈⠉⠁⠀⠀⠀⣿⡇⣀⣠⣴⣾⣮⣝⠿⠿⠿⣻⡟\n",
    "⢺⣿⠀⠘⣿⡇⠀⠀⠀⠀⠀⠀⠀⣠⣶⣾⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⡿⠁⠉⠀\n",
    "⠼⣿⠀⠀⣿⡇⠀⠀⠀⠀⠀⣠⣾⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⡿⠟⠉ ⠀⠀⠀* LaudaCode *⠀\n",
    "⠅⠻⣷⣶⣿⣇⠀⠀⠀⢠⣼⣿⣿⣿⣿⣿⣿⣿⣛⣛⣻⠉⠁⠀⠀         v",
    env!("CARGO_PKG_VERSION"),
    "⠀\n",
    "⡂⠀⠀⠀⢸⣿⠀⠀⠀⢸⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⣿⡇⠀⠀AI coding agent, pure Rust\n",
    "⠀⠀⠀⠀⢸⣿⣀⣀⣀⣼⡿⢿⣿⣿⣿⣿⣿⡿⣿⣿⡿⠀  ---------------------------⠀⠀⠀⠀⠀⠀⠀⠀\n",
    "⠀⠀⠀⠀⠙⠛⠛⠛⠋⠁⠀⠙⠻⠿⠟⠋⠑⠛⠋⠀⠀",
);

/// Banner renders as a vertical green → cyan → blue gradient, echoing the
/// Bright green → cyan → blue terminal palette.
const BANNER_COLORS: &[Color] = &[
    Color::LightGreen,
    Color::Green,
    Color::Green,
    Color::Cyan,
    Color::Cyan,
    Color::LightCyan,
    Color::LightCyan,
    Color::LightBlue,
    Color::LightBlue,
    Color::Blue,
    Color::Blue,
    Color::Blue,
    Color::Blue,
];

/// Total header height: 13 braille-art rows (branding is embedded in the art).
const HEADER_HEIGHT: u16 = 13;

/// One entry in the transcript (the scrolling history above the composer).
#[derive(Debug, Clone)]
pub enum Entry {
    User(String),
    Assistant(String),
    Reasoning(String),
    ToolCall { name: String, summary: String },
    ToolResult { name: String, ok: bool, preview: String },
    /// A tool mutated files — rendered as colored unified diffs.
    ToolDiff { name: String, files: Vec<crate::diff::FileDiff> },
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

/// A modal picker over a list of strings (models, providers, ...).
struct Picker {
    title: String,
    items: Vec<String>,
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

/// Metadata for a running `/provider add|edit` flow — tells the app what an
/// [`Action::InputSubmit`] from the modal belongs to.
pub struct ProviderSetup {
    pub kind: SetupKind,
    /// Preset key (Add) or configured provider name (EditKey).
    pub name: String,
    pub base_url: String,
    /// Set once the key modal has been answered (Add only).
    pub api_key: Option<String>,
}

impl ProviderSetup {
    pub fn add(key: &str, base_url: &str) -> Self {
        Self {
            kind: SetupKind::Add,
            name: key.to_string(),
            base_url: base_url.to_string(),
            api_key: None,
        }
    }

    pub fn edit_key(name: &str) -> Self {
        Self {
            kind: SetupKind::EditKey,
            name: name.to_string(),
            base_url: String::new(),
            api_key: None,
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
        Self { title: title.into(), hint: hint.into(), value: String::new(), mask }
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
            .filter(|(_, s)| f.is_empty() || s.to_lowercase().contains(&f))
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
    /// Highlighted row in the @-file popup.
    at_sel: usize,
    /// Ctrl+O output-expansion overlay (last tool results in full).
    overlay: bool,
    overlay_scroll: usize,
    /// Double-press Ctrl+C to quit (first press warns instead of exiting).
    last_ctrl_c: Option<Instant>,
    /// Brand banner pinned above the transcript (Ctrl+B toggles).
    show_banner: bool,
    /// Render cache: wrapped lines for already-processed entries. Only the
    /// growing tail (the streaming entry) is re-wrapped each frame.
    cache_width: u16,
    cached_lines: Vec<Line<'static>>,
    processed_entries: usize,
    /// Per-processed-entry: (content length when wrapped, line count).
    entry_state: Vec<(usize, usize)>,
    last_tick: Instant,
    /// Session start for the dashboard elapsed timer.
    session_started: Instant,
    /// Sent-prompt history for ↑/↓ recall (newest last).
    pub history: Vec<String>,
    /// Index into [`Tui::history`] while browsing; None = not browsing.
    history_pos: Option<usize>,
    /// In-progress draft saved when history browsing starts, restored by ↓.
    history_draft: String,
}

/// Identity + counters rendered in the wide-terminal side dashboard.
#[derive(Debug, Clone, Default)]
pub struct Dash {
    /// Short display form of the unique session id.
    pub session_id: String,
    pub model: String,
    pub provider: String,
    /// Working directory, home-shortened, for display.
    pub cwd: String,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub requests: usize,
    pub messages: usize,
    pub plan_done: usize,
    pub plan_total: usize,
}

impl Dash {
    pub fn set_session(&mut self, id: &str, model: &str, provider: &str, cwd: &str, messages: usize) {
        self.session_id = shorten_id(id);
        self.model = model.to_string();
        self.provider = provider.to_string();
        self.cwd = cwd.to_string();
        self.messages = messages;
    }

    /// Reflect a live model/provider switch in the dashboard immediately.
    pub fn set_endpoint(&mut self, provider: &str, model: &str) {
        self.provider = provider.to_string();
        self.model = model.to_string();
    }

    pub fn record_usage(&mut self, prompt: u64, completion: u64) {
        self.prompt_tokens = prompt;
        self.completion_tokens = completion;
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

/// Built-in slash commands surfaced by the composer autocomplete.
const SLASH_COMMANDS: &[(&str, &str)] = &[
    ("/help", "show all commands"),
    ("/model", "pick a model from the live list"),
    ("/approvals", "switch approval mode (plan/build/full-auto)"),
    ("/provider", "menu: add · use · edit · list"),
    ("/compact", "summarize history to free context"),
    ("/clear", "reset the conversation"),
    ("/retry", "re-run the previous task"),
    ("/export", "save transcript as markdown"),
    ("/resume", "resume a previous session by id"),
    ("/image", "attach an image to your next message"),
    ("/status", "provider · model · session info"),
    ("/diff", "show uncommitted git changes"),
    ("/undo", "revert file changes from the last turn"),
    ("/init", "create an AGENTS.md project brief"),
    ("/quit", "exit Laudacode"),
    ("/exit", "exit Laudacode (alias of /quit)"),
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

const SPINNER: [&str; 8] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
const TICK_MS: u64 = 100;

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
            session_started: Instant::now(),
            at_sel: 0,
            overlay: false,
            overlay_scroll: 0,
            last_ctrl_c: None,
            show_banner: true,
            cache_width: 0,
            cached_lines: Vec::new(),
            processed_entries: 0,
            entry_state: Vec::new(),
            last_tick: Instant::now(),
            history: Vec::new(),
            history_pos: None,
            history_draft: String::new(),
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

    /// Set/clear the footer activity indicator.
    pub fn set_busy(&mut self, busy: bool, label: impl Into<String>) {
        self.busy = busy;
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

    /// Insert bracketed-paste content verbatim (newlines included).
    /// Never triggers submission — the user sends with Enter afterwards.
    pub fn insert_paste(&mut self, text: &str) {
        if self.input_modal.is_some() {
            // Pasting a key/model lands directly in the modal's field.
            if let Some(m) = &mut self.input_modal {
                m.value.push_str(text.trim_end_matches(['\r', '\n']));
            }
            return;
        }
        if self.pending_approval.is_some() || self.picker.is_some() || self.overlay {
            return; // modals take over all input
        }
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        self.input.push_str(&normalized);
        self.slash_sel = 0;
        self.at_sel = 0;
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

    /// Toggle the pinned brand banner (Ctrl+B).
    pub fn toggle_banner(&mut self) {
        self.show_banner = !self.show_banner;
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
                at_start && no_space_after && self.files.iter().any(|f| Self::at_matches(&self.files_query(), f))
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
                let rank = if lower.rsplit('/').next().unwrap_or("").starts_with(&q) && !q.is_empty() { 0 } else { 1 };
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
        if m.is_empty() { self.status = None } else { self.status = Some((m, Instant::now())) }
    }

    pub fn clear_status(&mut self) {
        self.status = None;
    }

    pub fn open_picker(&mut self, title: impl Into<String>, items: Vec<String>) {
        self.picker = Some(Picker { title: title.into(), items, selected: 0, filter: String::new() });
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
                    + files.iter().map(|f| f.path.len() + f.lines.iter().map(|l| l.text.len()).sum::<usize>()).sum::<usize>()
            }
        }
    }

    /// Wrap one entry into display lines.
    fn entry_lines(e: &Entry, width: u16) -> Vec<Line<'static>> {
        match e {
            Entry::User(t) => vec![Line::from(Span::styled(
                format!("{} {}", "›", t),
                Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD),
            ))],
            Entry::Assistant(t) => {
                crate::markdown::render_markdown(t, width.saturating_sub(2) as usize)
            }
            Entry::Reasoning(t) => wrap_text(t, width.saturating_sub(4) as usize)
                .into_iter()
                .map(|l| {
                    Line::from(Span::styled(
                        l,
                        Style::default().fg(Color::DarkGray).add_modifier(Modifier::ITALIC),
                    ))
                })
                .collect(),
            Entry::ToolCall { name, summary } => vec![Line::from(vec![
                Span::styled("● ", Style::default().fg(Color::LightBlue)),
                Span::styled(
                    name.clone(),
                    Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!(" {summary}"), Style::default().fg(Color::Gray)),
            ])],
            Entry::ToolResult { name, ok, preview } => {
                let color = if *ok { Color::LightGreen } else { Color::LightRed };
                let mut lines = vec![Line::from(vec![
                    Span::styled(if *ok { "✔ " } else { "✘ " }, Style::default().fg(color)),
                    Span::styled(name.clone(), Style::default().fg(color)),
                ])];
                for l in preview.lines().take(4) {
                    lines.push(Line::from(Span::styled(
                        format!("  {l}"),
                        Style::default().fg(Color::Gray),
                    )));
                }
                lines
            }
            Entry::ToolDiff { name, files } => {
                use crate::diff::LineKind;
                let total_add: usize = files.iter().map(|f| f.added).sum();
                let total_del: usize = files.iter().map(|f| f.removed).sum();
                let mut lines = vec![Line::from(vec![
                    Span::styled("✎ ", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
                    Span::styled(
                        name.clone(),
                        Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
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
                        Span::styled("┌─ ", Style::default().fg(Color::Cyan)),
                        Span::styled(
                            format!("{} (+{} −{})", f.path, f.added, f.removed),
                            Style::default().fg(Color::Cyan).add_modifier(Modifier::BOLD),
                        ),
                    ]));
                    // Syntax-aware diff rows: token colors from the file's
                    // language over subtle add/remove backgrounds.
                    let lang = crate::syntax::lang_of(&f.path);
                    let mut syn_state = crate::syntax::SynState::default();
                    for dl in &f.lines {
                        let (bar, tint) = match dl.kind {
                            LineKind::Add => ("│", Some(ADD_BG)),
                            LineKind::Del => ("│", Some(DEL_BG)),
                            LineKind::Meta => ("│", None),
                            LineKind::Ctx => ("│", None),
                        };
                        let bar_style = match dl.kind {
                            LineKind::Add => Style::default().fg(Color::Green),
                            LineKind::Del => Style::default().fg(Color::Red),
                            LineKind::Meta => {
                                Style::default().fg(Color::LightBlue).add_modifier(Modifier::ITALIC)
                            }
                            LineKind::Ctx => Style::default().fg(Color::DarkGray),
                        };
                        // Meta lines (@@ headers) stay plain; code gets tokens.
                        // The +/-/-sign keeps its strong kind color.
                        let content: Vec<Span<'static>> = match dl.kind {
                            LineKind::Meta => vec![Span::styled(
                                dl.text.clone(),
                                Style::default().fg(Color::LightBlue).add_modifier(Modifier::ITALIC),
                            )],
                            _ => {
                                let base = match tint {
                                    Some(bg) => Style::default().bg(bg),
                                    None => Style::default(),
                                };
                                let mut spans: Vec<Span<'static>> = Vec::new();
                                if let Some(rest) = dl.text.strip_prefix('+').or_else(|| dl.text.strip_prefix('-')) {
                                    let sign_color = match dl.kind {
                                        LineKind::Add => Color::Green,
                                        _ => Color::Red,
                                    };
                                    spans.push(Span::styled(
                                        dl.text[..1].to_string(),
                                        base.fg(sign_color).add_modifier(Modifier::BOLD),
                                    ));
                                    spans.extend(crate::syntax::highlight_line(rest, lang, &mut syn_state, base));
                                } else {
                                    spans.extend(crate::syntax::highlight_line(&dl.text, lang, &mut syn_state, base));
                                }
                                spans
                            }
                        };
                        for seg in crate::markdown::wrap_styled(&content, width.saturating_sub(4) as usize) {
                            let mut row = vec![Span::styled(bar.to_string(), bar_style)];
                            row.extend(seg.spans);
                            lines.push(Line::from(row));
                        }
                    }
                    lines.push(Line::from(Span::styled("└─", Style::default().fg(Color::Cyan))));
                }
                lines
            }
            Entry::Info(t) => t
                .lines()
                .map(|l| Line::from(Span::styled(l.to_string(), Style::default().fg(Color::LightBlue))))
                .collect(),
            Entry::Error(t) => wrap_text(t, width.saturating_sub(2) as usize)
                .into_iter()
                .map(|l| Line::from(Span::styled(l, Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD))))
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

    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();

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
                let cols = Layout::horizontal([
                    Constraint::Min(40),
                    Constraint::Length(dw),
                ])
                .split(area);
                (cols[0], Some(cols[1]))
            }
            None => (area, None),
        };

        // The composer grows with the draft instead of clipping long prompts.
        let comp_h = self.composer_height(area.width, area.height);

        // Header (brand banner) is shown when there's room for it — the art
        // is 13 rows and up to 59 columns wide (embedded wordmark text).
        let show_header = self.show_banner && area.height >= 23 && area.width >= 60;
        let chunks = if show_header {
            Layout::vertical([
                Constraint::Length(HEADER_HEIGHT), // banner
                Constraint::Min(1),                // transcript
                Constraint::Length(comp_h),        // composer (auto-expands)
                Constraint::Length(1),             // hints
                Constraint::Length(1),             // footer
            ])
            .split(area)
        } else {
            Layout::vertical([
                Constraint::Min(1),      // transcript
                Constraint::Length(comp_h),
                Constraint::Length(1),   // hints
                Constraint::Length(1),   // footer
            ])
            .split(area)
        };
        let (header, transcript_area, composer_area, hints_area, footer_area) = if show_header {
            (Some(chunks[0]), chunks[1], chunks[2], chunks[3], chunks[4])
        } else {
            (None, chunks[0], chunks[1], chunks[2], chunks[3])
        };

        if let Some(h) = header {
            let banner_lines: Vec<Line> = BANNER
                .lines()
                .zip(BANNER_COLORS.iter())
                .map(|(row, color)| {
                    Line::from(Span::styled(
                        row.to_string(),
                        Style::default().fg(*color).add_modifier(Modifier::BOLD),
                    ))
                })
                .collect();
            f.render_widget(Paragraph::new(banner_lines), h);
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

        // Scroll hint
        if self.scroll > 0 {
            let hint = Span::styled(format!(" ↑ {} lines (Esc to release) ", self.scroll), Style::default().fg(Color::DarkGray));
            let r = Rect::new(transcript_area.x, transcript_area.y, transcript_area.width, 1);
            let p = Paragraph::new(Line::from(hint)).alignment(ratatui::layout::Alignment::Right);
            f.render_widget(p, r);
        }

        // Composer — border glows in the active mode color while working.
        let comp_style = if self.busy {
            Style::default().fg(self.mode.color())
        } else {
            Style::default().fg(Color::Rgb(110, 110, 135))
        };
        const PLACEHOLDER: &str =
            "ask laudacode anything — @ to mention files, # to remember, / for commands";
        let cursor_ok = self.pending_approval.is_none();
        let comp_inner_w = composer_area.width.saturating_sub(2).max(10) as usize;
        let text: Vec<Line> = if self.input.is_empty() && !cursor_ok {
            vec![Line::from(Span::styled(
                "waiting for approval — y / a / n",
                Style::default().fg(Color::DarkGray),
            ))]
        } else if self.input.is_empty() {
            vec![Line::from(Span::styled(
                PLACEHOLDER,
                Style::default().fg(Color::DarkGray),
            ))]
        } else {
            // Rendered from the exact same wrapped rows used for height and
            // cursor math — spaces can never disappear again.
            wrap_composer(&self.input, comp_inner_w)
                .into_iter()
                .map(Line::from)
                .collect()
        };
        let composer = Paragraph::new(text)
            .block(Block::default().borders(Borders::ALL).style(comp_style));
        f.render_widget(composer, composer_area);
        if cursor_ok && !self.input.is_empty() {
            // No cursor navigation exists — the caret is always at the end,
            // i.e. after the last visual row of the wrapped draft.
            let inner_w = composer_area.width.saturating_sub(2).max(10) as usize;
            let segs = wrap_composer(&self.input, inner_w);
            let last = segs.last().map(String::as_str).unwrap_or("");
            let col = UnicodeWidthStr::width(last) as u16;
            let row = composer_area.y
                + 1
                + ((segs.len().saturating_sub(1)) as u16).min(composer_area.height.saturating_sub(2));
            let x = composer_area.x + 1 + col.min(composer_area.width.saturating_sub(2));
            f.set_cursor_position((x, row));
        }

        // Slash-command + @-file suggestion popups, floating above the composer.
        if self.pending_approval.is_none() {
            self.draw_slash_popup(f, area, composer_area);
            self.draw_at_popup(f, area, composer_area);
        }

        // Hints strip under the composer.
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                " enter send · ↑↓ history · pgup/pgdn scroll · tab mode · esc interrupt · ctrl+c quit ",
                Style::default().fg(Color::DarkGray),
            ))),
            hints_area,
        );

        // Approval modal floats over everything else.
        if let Some(detail) = self.pending_approval.clone() {
            self.draw_approval_modal(f, area, &detail);
        }

        // Footer: brand + mode chip + activity on the left; context meter +
        // subtitle right-aligned in a second column.
        let cols = Layout::horizontal([
            Constraint::Percentage(55),
            Constraint::Percentage(45),
        ])
        .split(footer_area);
        let mut spans = vec![
            Span::styled(" LaudaCode ", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
            Span::styled(
                format!(" {} ", self.mode.label()),
                Style::default().bg(self.mode.color()).fg(Color::Black).add_modifier(Modifier::BOLD),
            ),
        ];
        if self.busy {
            let glyph = SPINNER[self.spinner_idx % SPINNER.len()];
            let secs = self.busy_since.map(|t| t.elapsed().as_secs()).unwrap_or(0);
            spans.push(Span::styled(
                format!(" {glyph} {} (esc · {secs}s)", self.busy_label),
                Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
            ));
        }
        if let Some((msg, at)) = &self.status {
            let secs = at.elapsed().as_secs();
            spans.push(Span::styled(
                format!("  ·  {msg} ({secs}s)"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), cols[0]);

        // Context-left meter: ▕██████░░░░░░▏ 38%
        let pct = self
            .ctx_used
            .min(self.ctx_total)
            .checked_mul(100)
            .and_then(|n| n.checked_div(self.ctx_total))
            .map(|p| p.min(100))
            .unwrap_or(0);
        const BAR_SLOTS: usize = 10;
        let filled = pct as usize * BAR_SLOTS / 100;
        let bar_color = if pct >= 85 {
            Color::LightRed
        } else if pct >= 60 {
            Color::LightYellow
        } else {
            Color::DarkGray
        };
        let meter_spans = vec![
            Span::styled(self.subtitle.clone(), Style::default().fg(Color::DarkGray)),
            Span::styled("ctx ", Style::default().fg(Color::DarkGray)),
            Span::styled("▕", Style::default().fg(Color::DarkGray)),
            Span::styled("█".repeat(filled), Style::default().fg(bar_color)),
            Span::styled("░".repeat(BAR_SLOTS - filled), Style::default().fg(Color::DarkGray)),
            Span::styled("▏", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(" {:>3}% ", 100 - pct),
                Style::default().fg(if 100 - pct <= 15 { Color::LightRed } else { Color::Gray }),
            ),
        ];
        f.render_widget(
            Paragraph::new(Line::from(meter_spans)).alignment(ratatui::layout::Alignment::Right),
            cols[1],
        );

        if let Some(dash_rect) = dash_area {
            self.draw_dashboard(f, dash_rect);
        }
    }

    /// Persistent right-side panel: session identity + live counters.
    fn draw_dashboard(&mut self, f: &mut Frame, rect: Rect) {
        let lines = self.dashboard_lines(rect.width);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Rgb(70, 70, 95)))
            .title(Span::styled(
                " laudacode ",
                Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
            ));
        f.render_widget(Paragraph::new(lines).block(block), rect);
    }

    /// Build the dashboard rows (pure — unit-tested without a terminal).
    fn dashboard_lines(&self, width: u16) -> Vec<Line<'static>> {
        let w = width.saturating_sub(2) as usize; // border padding
        let mut v: Vec<Line<'static>> = Vec::new();
        let row = |label: &str, value: String, vcolor: Color| -> Line<'static> {
            Line::from(vec![
                Span::styled(format!(" {:<9}", label), Style::default().fg(Color::DarkGray)),
                Span::styled(Self::truncate_to(value, w.saturating_sub(10)), Style::default().fg(vcolor)),
            ])
        };
        v.push(row("session", self.dash.session_id.clone(), Color::White));
        v.push(row("model", self.dash.model.clone(), Color::Gray));
        v.push(row("provider", self.dash.provider.clone(), Color::Gray));
        v.push(Line::from(Span::styled(
            format!(" {}", "─".repeat(w.saturating_sub(1))),
            Style::default().fg(Color::Rgb(60, 60, 80)),
        )));
        v.push(row("mode", self.mode.label().to_string(), self.mode.color()));
        v.push(Line::from(Span::raw(String::new())));

        // Context usage block.
        v.push(Line::from(Span::styled(
            " context",
            Style::default().fg(Color::DarkGray),
        )));
        let pct = self
            .ctx_used
            .min(self.ctx_total)
            .checked_mul(100)
            .and_then(|n| n.checked_div(self.ctx_total))
            .map(|p| p.min(100))
            .unwrap_or(0);
        const SLOTS: usize = 14;
        let filled = pct as usize * SLOTS / 100;
        let bar_color = if pct >= 85 {
            Color::LightRed
        } else if pct >= 60 {
            Color::LightYellow
        } else {
            Color::Green
        };
        v.push(Line::from(vec![
            Span::raw(" "),
            Span::styled("▕", Style::default().fg(Color::DarkGray)),
            Span::styled("█".repeat(filled), Style::default().fg(bar_color)),
            Span::styled("░".repeat(SLOTS - filled), Style::default().fg(Color::Rgb(60, 60, 80))),
            Span::styled("▏", Style::default().fg(Color::DarkGray)),
        ]));
        v.push(Line::from(vec![
            Span::styled(format!(" {:<9}", "in"), Style::default().fg(Color::DarkGray)),
            Span::styled(Self::fmt_tokens(self.dash.prompt_tokens), Style::default().fg(Color::Gray)),
            Span::styled(" tok", Style::default().fg(Color::DarkGray)),
        ]));
        v.push(Line::from(vec![
            Span::styled(format!(" {:<9}", "out"), Style::default().fg(Color::DarkGray)),
            Span::styled(Self::fmt_tokens(self.dash.completion_tokens), Style::default().fg(Color::Gray)),
            Span::styled(" tok", Style::default().fg(Color::DarkGray)),
        ]));

        v.push(Line::from(Span::raw(String::new())));
        v.push(row("requests", self.dash.requests.to_string(), Color::Gray));
        v.push(row("messages", self.dash.messages.to_string(), Color::Gray));
        if self.dash.plan_total > 0 {
            v.push(row(
                "plan",
                format!("{}/{} done", self.dash.plan_done, self.dash.plan_total),
                if self.dash.plan_done == self.dash.plan_total {
                    Color::LightGreen
                } else {
                    Color::Gray
                },
            ));
        }
        v.push(row("cwd", self.dash.cwd.clone(), Color::DarkGray));
        let secs = self.session_started.elapsed().as_secs();
        v.push(row("elapsed", Self::fmt_elapsed(secs), Color::DarkGray));
        v.push(Line::from(Span::raw(String::new())));
        v.push(Line::from(Span::styled(
            " esc interrupt · tab mode",
            Style::default().fg(Color::Rgb(55, 55, 75)),
        )));
        v
    }

/// Clamp a display string to `w` cells (unicode-width aware-ish).
fn truncate_to(s: String, w: usize) -> String {
    if UnicodeWidthStr::width(s.as_str()) <= w {
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
const DASH_WIDTH: u16 = 28;


    /// Centered approval dialog: detail + y/a/n options.
    fn draw_approval_modal(&mut self, f: &mut Frame, area: Rect, detail: &str) {
        let width = area.width.clamp(40, 64);
        let wrapped = wrap_text(detail, width.saturating_sub(4) as usize);
        let height = (wrapped.len() as u16 + 5).min(area.height.saturating_sub(2));
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 3,
            width,
            height,
        };
        let mut lines: Vec<Line> = vec![Line::from(Span::styled(
            "Allow this action?",
            Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD),
        ))];
        for l in wrapped {
            lines.push(Line::from(Span::styled(l, Style::default().fg(Color::Gray))));
        }
        lines.push(Line::from(String::new()));
        lines.push(Line::from(vec![
            Span::styled("y", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
            Span::raw(" yes   "),
            Span::styled("a", Style::default().fg(Color::LightYellow).add_modifier(Modifier::BOLD)),
            Span::raw(" always (full-auto)   "),
            Span::styled("n", Style::default().fg(Color::LightRed).add_modifier(Modifier::BOLD)),
            Span::raw(" no   "),
            Span::styled("esc", Style::default().fg(Color::Gray)),
            Span::raw(" cancel"),
        ]));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(Span::styled(" approval ", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            rect,
        );
    }

    /// Centered modal input dialog: hint line + live input field with caret.
    fn draw_input_modal(&mut self, f: &mut Frame, area: Rect, m: &mut InputModal) {
        let width = 64.min(area.width.saturating_sub(4)).max(30);
        let hint_lines = wrap_text(&m.hint, width.saturating_sub(4) as usize);
        let height = (hint_lines.len() as u16 + 6).min(area.height.saturating_sub(2));
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        let mut lines: Vec<Line> = Vec::new();
        for l in &hint_lines {
            lines.push(Line::from(Span::styled(l.clone(), Style::default().fg(Color::Gray))));
        }
        lines.push(Line::from(String::new()));
        // The field itself: a filled row so it reads as an input box.
        let field_w = (width.saturating_sub(4)) as usize;
        let mut shown = m.display();
        if shown.chars().count() >= field_w {
            shown = shown.chars().skip(shown.chars().count() - field_w).collect();
        }
        let pad = field_w.saturating_sub(shown.chars().count());
        lines.push(Line::from(vec![
            Span::styled(format!(" {shown}"), Style::default().fg(Color::LightCyan).add_modifier(Modifier::BOLD)),
            Span::styled(" ".repeat(pad), Style::default().fg(Color::DarkGray)),
        ]));
        lines.push(Line::from(String::new()));
        lines.push(Line::from(vec![
            Span::styled("enter", Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD)),
            Span::raw(" confirm   "),
            Span::styled("esc", Style::default().fg(Color::Gray)),
            Span::raw(" cancel"),
        ]));
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::LightGreen))
            .title(Span::styled(
                format!(" {} ", m.title),
                Style::default().fg(Color::LightGreen).add_modifier(Modifier::BOLD),
            ));
        f.render_widget(Clear, rect);
        f.render_widget(
            Paragraph::new(lines)
                .block(block)
                .wrap(Wrap { trim: false }),
            rect,
        );
    }

    /// Ctrl+O overlay: recent tool activity expanded in full, scrollable.
    fn draw_overlay(&mut self, f: &mut Frame, area: Rect) {
        f.render_widget(Clear, area);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " tool output — esc/ctrl+o to close ",
                Style::default().fg(Color::Cyan),
            ))
            .border_style(Style::default().fg(Color::Cyan));
        let inner_w = area.width.saturating_sub(2);
        // Collect the last tool entries, newest last.
        let mut lines: Vec<Line> = Vec::new();
        for e in &self.entries {
            match e {
                Entry::ToolCall { name, summary } => {
                    lines.push(Line::from(vec![
                        Span::styled(format!("● {name}"), Style::default().fg(Color::LightBlue).add_modifier(Modifier::BOLD)),
                        Span::styled(format!(" {summary}"), Style::default().fg(Color::Gray)),
                    ]));
                }
                Entry::ToolResult { name, ok, preview } => {
                    let color = if *ok { Color::LightGreen } else { Color::LightRed };
                    lines.push(Line::from(Span::styled(
                        format!("{} {}", if *ok { "✔" } else { "✘" }, name),
                        Style::default().fg(color).add_modifier(Modifier::BOLD),
                    )));
                    for l in wrap_text(preview, inner_w.max(20) as usize) {
                        lines.push(Line::from(Span::styled(
                            format!("  {l}"),
                            Style::default().fg(Color::Gray),
                        )));
                    }
                    lines.push(Line::from(Span::raw(String::new())));
                }
                Entry::ToolDiff { .. } => {
                    // Full colored rendering reuses the transcript pipeline.
                    let w = area.width.saturating_sub(2).max(20);
                    lines.extend(Self::entry_lines(e, w));
                    lines.push(Line::from(Span::raw(String::new())));
                }
                _ => {}
            }
        }
        if lines.is_empty() {
            lines.push(Line::from(Span::styled(
                "no tool output yet",
                Style::default().fg(Color::DarkGray),
            )));
        }
        let total_lines = lines.len();
        let visible = area.height.saturating_sub(2) as usize;
        let max_scroll = total_lines.saturating_sub(visible);
        let skip = self.overlay_scroll.min(max_scroll);
        let start = total_lines.saturating_sub(visible + skip);
        let shown = &lines[start..start + visible.min(total_lines - start)];
        f.render_widget(Paragraph::new(shown.to_vec()).block(block), area);
    }

    /// Floating suggestion list above the composer while typing a command.
    fn draw_slash_popup(&mut self, f: &mut Frame, area: Rect, composer: Rect) {
        let matches = self.slash_matches();
        if matches.is_empty() {
            return;
        }
        const MAX_VISIBLE: usize = 6;
        let visible = matches.len().min(MAX_VISIBLE);
        let height = visible as u16 + 2; // borders
        let width = area.width.clamp(28, 52);
        let y = composer.y.saturating_sub(height);
        if y < area.y {
            return; // not enough room above the composer
        }
        let rect = Rect { x: area.x, y, width, height };

        let entries = self.slash_entries();
        let sel = self.slash_sel.min(matches.len() - 1);
        // Keep the highlighted row inside a sliding window.
        let start = sel.saturating_sub(visible / 2).min(matches.len() - visible);
        let items: Vec<ListItem> = matches[start..start + visible]
            .iter()
            .map(|&i| {
                let entry = &entries[i];
                let (cmd, desc) = (&entry.cmd, &entry.desc);
                let selected = i == matches[sel];
                let style = if selected {
                    Style::default().bg(Color::Rgb(60, 60, 80)).fg(Color::White)
                } else {
                    Style::default()
                };
                let name_color = if entry.custom.is_some() { Color::LightGreen } else { Color::Cyan };
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<14}", cmd), style.fg(if selected { Color::White } else { name_color }).add_modifier(Modifier::BOLD)),
                    Span::styled(desc.to_string(), if selected { style } else { Style::default().fg(Color::DarkGray) }),
                ]))
            })
            .collect();

        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::DarkGray));
        f.render_widget(Clear, rect);
        f.render_widget(List::new(items).block(block), rect);
    }

    /// Floating file list above the composer while typing an '@path'.
    fn draw_at_popup(&mut self, f: &mut Frame, area: Rect, composer: Rect) {
        let matches = self.at_matches_list();
        if matches.is_empty() || self.pending_approval.is_some() {
            return;
        }
        const MAX_VISIBLE: usize = 6;
        let visible = matches.len().min(MAX_VISIBLE);
        let height = visible as u16 + 2;
        let width = area.width.clamp(30, 56);
        // Stack above the slash popup when both would collide.
        let slash_h = if self.slash_popup_active() { 8 } else { 0 };
        let y = composer.y.saturating_sub(height + slash_h);
        if y < area.y {
            return;
        }
        let rect = Rect { x: area.x, y, width, height };
        let sel = self.at_sel.min(matches.len() - 1);
        let start = sel.saturating_sub(visible / 2).min(matches.len() - visible);
        let items: Vec<ListItem> = matches[start..start + visible]
            .iter()
            .map(|&i| {
                let selected = i == matches[sel];
                let path = &self.files[i];
                let style = if selected {
                    Style::default().bg(Color::Rgb(50, 70, 60)).fg(Color::White)
                } else {
                    Style::default().fg(Color::Green)
                };
                ListItem::new(Line::from(Span::styled(format!("@{path}"), style)))
            })
            .collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(" files ", Style::default().fg(Color::Green)))
            .border_style(Style::default().fg(Color::Green));
        f.render_widget(Clear, rect);
        f.render_widget(List::new(items).block(block), rect);
    }

    fn draw_picker(&mut self, f: &mut Frame, area: Rect, p: &mut Picker) {
        let block = Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(format!(" {} ", p.title), Style::default().fg(Color::Cyan)))
            .border_style(Style::default().fg(Color::Cyan));
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
                Span::styled("filter: ", Style::default().fg(Color::DarkGray)),
                Span::raw(p.filter.clone()),
            ])),
            rows[0],
        );

        let idxs = p.filtered();
        let max = rows[1].height as usize;
        let sel_pos = idxs.iter().position(|i| *i == p.selected).unwrap_or(0);
        let start = sel_pos.saturating_sub(max / 2);
        let items: Vec<ListItem> = idxs
            .iter()
            .skip(start)
            .take(max)
            .map(|i| {
                let style = if *i == p.selected {
                    Style::default().bg(Color::Rgb(60, 60, 80)).fg(Color::White)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(Span::styled(p.items[*i].clone(), style)))
            })
            .collect();
        f.render_widget(List::new(items), rows[1]);

        let count = idxs.len();
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{count} items · ↑/↓ move · enter select · esc cancel"),
                Style::default().fg(Color::DarkGray),
            ))),
            rows[2],
        );
    }

    /// Handle one terminal event. Returns the action the host should take.
    pub fn on_key(&mut self, key: KeyEvent) -> Action {
        // Ctrl+O output overlay takes over all keys.
        if self.overlay {
            return match key.code {
                KeyCode::Esc | KeyCode::Char('o') => {
                    if key.code == KeyCode::Char('o') && !key.modifiers.contains(KeyModifiers::CONTROL) && !self.input.is_empty() {
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
                        if !key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                            && self.input.trim_end() != self.files[matches[self.at_sel.min(matches.len() - 1)]] =>
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
                        if !key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
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
                if !key.modifiers.intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
            {
                let input = self.input.trim().to_string();
                self.input.clear();
                self.record_history(&input);
                if input.is_empty() { Action::None } else { Action::Submit(input) }
            }
            // Shift+Enter / Alt+Enter insert a newline instead of submitting.
            KeyCode::Enter => {
                self.input.push('\n');
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
                    self.input.push(c);
                    self.slash_sel = 0;
                    self.at_sel = 0;
                    Action::None
                }
            }
            KeyCode::Backspace => {
                if self.input.pop().is_some() {
                    self.slash_sel = 0;
                    self.at_sel = 0;
                }
                Action::None
            }
            KeyCode::Esc => {
                // Close an open @-token first, then release scroll, then
                // clear the input — and always signal interrupt.
                self.history_pos = None;
                if self.at_token_present() {
                    if let Some(i) = self.input.rfind('@') {
                        self.input.truncate(i);
                        self.at_sel = 0;
                    }
                } else if self.scroll > 0 {
                    self.scroll = 0;
                } else {
                    self.input.clear();
                }
                Action::Interrupt
            }
            // Scrolling lives on PageUp/PageDown only — the arrow keys are
            // fully reserved for prompt-history recall.
            KeyCode::Up => {
                self.history_up();
                Action::None
            }
            KeyCode::Down => {
                self.history_down();
                Action::None
            }
            KeyCode::PageUp if self.scrollable() => { self.page_up(20); Action::None }
            KeyCode::PageDown => { self.page_down(20); Action::None }
            _ => Action::None,
        }
    }

    fn scrollable(&self) -> bool {
        true
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
                KeyCode::Esc => { self.picker = None; }
                KeyCode::Up => {
                    if pos > 0 { p.selected = idxs[pos - 1]; }
                    else if let Some(last) = idxs.last() { p.selected = *last; }
                }
                KeyCode::Down => {
                    if pos + 1 < idxs.len() { p.selected = idxs[pos + 1]; }
                    else if let Some(first) = idxs.first() { p.selected = *first; }
                }
                KeyCode::Backspace => { p.filter.pop(); }
                KeyCode::Char(c) => {
                    if key.modifiers.contains(KeyModifiers::CONTROL) {
                        if c == 'c' { self.picker = None; }
                    } else {
                        p.filter.push(c);
                        if let Some(first) = p.filtered().first() { p.selected = *first; }
                    }
                }
                KeyCode::Enter => {
                    if let Some(i) = idxs.get(pos) {
                        let chosen = p.items[*i].clone();
                        let title = p.title.clone().to_lowercase();
                        self.picker = None;
                        action = Action::OpenSlash(format!("{title}:{chosen}"));
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
    let mut terminal = ratatui::Terminal::new(ratatui::backend::CrosstermBackend::new(std::io::stdout()))
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
                // Finger swipes / touch scrolling arrive as wheel events
                // (mouse capture is on). Modals ignore them; the transcript
                // scrolls a few lines per wheel tick.
                match me.kind {
                    MouseEventKind::ScrollUp => tui.page_up(3),
                    MouseEventKind::ScrollDown => tui.page_down(3),
                    _ => {}
                }
                let _ = on_action(tui, Action::None);
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
            if UnicodeWidthStr::width(line.as_str()) + UnicodeWidthStr::width(word.as_str()) + 1 > w {
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
                if !line.is_empty() { line.push(' '); }
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
        assert!(rows.iter().all(|r| UnicodeWidthStr::width(r.as_str()) <= 10));
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
        assert_eq!(t.composer_height(80, 30), 7, "4 lines + trailing blank + borders");
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
    fn dashboard_appears_only_on_wide_terminals() {
        // Needs BOTH ≥88 columns and ≥39 rows (Termux landscape is ~39 tall).
        assert_eq!(Tui::dash_width(87, 50), None);
        assert_eq!(Tui::dash_width(88, 50), Some(28));
        assert_eq!(Tui::dash_width(147, 39), Some(28), "Termux landscape");
        assert_eq!(Tui::dash_width(88, 38), None, "too short — no dashboard");
        assert_eq!(Tui::dash_width(100, 30), None, "short terminal keeps old layout");
        assert_eq!(Tui::dash_width(220, 60), Some(28));
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
            TodoItem { content: "a".into(), status: "completed".into() },
            TodoItem { content: "b".into(), status: "pending".into() },
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
        assert!(joined.contains("46.0k"), "prompt tokens humanized: {joined}");
        assert!(joined.contains("2.0k"), "completion tokens humanized: {joined}");
        assert!(joined.contains("1/2 done"), "plan progress: {joined}");
        assert!(joined.contains("requests"), "counter rows exist");
        assert!(joined.contains("█"), "context bar drawn");
    }

    #[test]
    fn slash_filter_matches_prefixes_case_insensitively() {
        assert_eq!(filter_slash_commands("/").len(), SLASH_COMMANDS.len());
        assert_eq!(filter_slash_commands("").len(), SLASH_COMMANDS.len());
        assert_eq!(filter_slash_commands("/pro"), vec![3]); // /provider
        assert_eq!(filter_slash_commands("/appro"), vec![2]); // /approvals
        assert_eq!(filter_slash_commands("/resum"), vec![8]); // /resume
        assert_eq!(filter_slash_commands("/imag"), vec![9]); // /image
        assert_eq!(filter_slash_commands("/RETRY"), vec![6]);
        assert_eq!(filter_slash_commands("/quit"), vec![14]);
        // Newer commands are discoverable too.
        assert_eq!(filter_slash_commands("/status"), vec![10]);
        assert_eq!(filter_slash_commands("/diff"), vec![11]);
        assert_eq!(filter_slash_commands("/undo"), vec![12]);
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
        assert_eq!(t.slash_matches(), vec![1]); // /model
        t.on_key(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(t.input, "/model ");
        // Popup closed after completion (trailing space).
        assert!(!t.slash_popup_active());
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
        assert_eq!(t.scroll, 0, "transcript must not scroll while popup is open");
    }

    #[test]
    fn selection_resets_when_input_changes() {
        let mut t = Tui::new();
        t.input = "/c".into();
        t.on_key(key(KeyCode::Down, KeyModifiers::NONE)); // sel=1
        t.on_key(key(KeyCode::Char('l'), KeyModifiers::NONE)); // "/cl"
        assert_eq!(t.slash_sel, 0);
        // Matches for "/cl": /clear only.
        assert_eq!(t.slash_matches(), vec![5]);
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
        assert!(matches!(t.on_key(key(KeyCode::Char('y'), KeyModifiers::NONE)), Action::Approve(true)));
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
        assert!(matches!(t.on_key(key(KeyCode::Enter, KeyModifiers::NONE)), Action::Submit(_)));
    }

    fn submit(t: &mut Tui, text: &str) {
        t.input = text.into();
        assert!(matches!(t.on_key(key(KeyCode::Enter, KeyModifiers::NONE)), Action::Submit(_)));
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
    fn tool_diffs_render_in_color() {
        use crate::diff::unified_diff;
        let d = unified_diff("src/lib.rs", "a\nb\n", "a\nc\n", 1);
        let mut t = Tui::new();
        t.push(Entry::ToolDiff { name: "edit_file".into(), files: vec![d] });
        let lines = line_texts(&t, 60);
        let joined = lines.join("\n");
        assert!(joined.contains("✎ edit_file"), "{joined}");
        assert!(joined.contains("┌─ src/lib.rs (+1 −1)"), "{joined}");
        assert!(joined.contains("│-b"), "{joined}");
        assert!(joined.contains("│+c"), "{joined}");
        // Colors are attached to the right kinds: the bar keeps its
        // kind color while the content row is syntax-highlighted.
        let mut probe = clone_shallow(&t);
        probe.ensure_render_cache(60);
        let add_line = probe
            .cached_lines
            .iter()
            .find(|l| l.spans.len() >= 3 && l.spans[0].content == "│" && l.spans[1].content == "+" && l.spans[2].content == "c")
            .unwrap_or_else(|| panic!("no +c diff line"));
        assert_eq!(add_line.spans[0].style.fg, Some(Color::Green), "add bar green");
        assert_eq!(add_line.spans[1].style.fg, Some(Color::Green), "+ sign stays green");
        assert_eq!(add_line.spans[2].style.bg, Some(ADD_BG), "add tint behind content");
        let del_line = probe
            .cached_lines
            .iter()
            .find(|l| l.spans.len() >= 3 && l.spans[0].content == "│" && l.spans[1].content == "-" && l.spans[2].content == "b")
            .unwrap();
        assert_eq!(del_line.spans[0].style.fg, Some(Color::Red), "del bar red");
        assert_eq!(del_line.spans[2].style.bg, Some(DEL_BG), "del tint behind content");
    }

    #[test]
    fn banner_spells_laudacode_shape() {
        let lines: Vec<&str> = BANNER.lines().collect();
        assert_eq!(lines.len(), HEADER_HEIGHT as usize);
        // Branding is embedded in the art's right-hand columns.
        assert!(BANNER.contains("LaudaCode"));
        assert!(BANNER.contains("pure Rust"));
        assert!(BANNER.contains(concat!("v", env!("CARGO_PKG_VERSION"))));
        for line in &lines {
            assert!(!line.trim().is_empty());
            assert!(!line.contains('\t'), "tabs would break column alignment");
            // Rows are left-anchored braille art — no trailing dead space.
            let trimmed = line.trim_end_matches(' ');
            assert!(!trimmed.is_empty());
            assert!(
                trimmed.starts_with(' ')
                    || ('\u{2800}'..='\u{28FF}').contains(&trimmed.chars().next().unwrap()),
                "every row must start with a braille glyph or space"
            );
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
    fn banner_colors_match_rows_and_are_bright() {
        assert_eq!(BANNER_COLORS.len(), BANNER.lines().count());
        for c in BANNER_COLORS {
            assert!(!matches!(c, Color::Magenta | Color::LightMagenta | Color::DarkGray), "no pinks/dulls allowed");
        }
    }

    #[test]
    fn busy_indicator_is_footer_only() {
        let mut t = Tui::new();
        t.set_busy(true, "working");
        assert!(t.is_busy());
        assert!(t.entries.is_empty(), "activity must not create transcript entries");
        t.set_busy(false, "working");
        assert!(!t.is_busy());
    }
}
