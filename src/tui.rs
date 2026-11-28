//! `mcfsck tui`: one full-screen session for the whole flow - scan (with
//! progress and time remaining), browse and filter the findings, select some,
//! preview the fix as a dry run, apply it.
//!
//! Everything is reachable from the keyboard. With mouse support on, rows,
//! column headers and the buttons along the bottom can be clicked and lists
//! scroll with the wheel.

use crate::fix::{self, FixOptions, Status};
use crate::progress::{human_duration, Eta};
use crate::repair;
use crate::report;
use crate::scan::{self, Finding, Fix, ScanOutput, Severity};
use crate::util::{human_bytes, human_count, s};
use crate::world;
use anyhow::{anyhow, bail, Result};
use ratatui::backend::Backend;
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton,
    MouseEvent, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Gauge, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use std::io::{stdin, stdout, IsTerminal};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub struct Config {
    pub world: PathBuf,
    pub scan: scan::Options,
    /// Where `w` saves the report.
    pub report_path: PathBuf,
    pub mouse: bool,
    pub theme: ThemeName,
    pub backup: bool,
    pub force: bool,
    pub linked: bool,
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum SortKey {
    Num,
    Sev,
    Place,
    Chunk,
    Problem,
}

enum RowState {
    Fresh,
    /// Removed or repaired in this session.
    Done(String),
    Skipped(String),
}

struct ScanJob {
    stats: Arc<scan::Stats>,
    eta: Eta,
    rx: Receiver<Result<ScanOutput>>,
}

struct FixJob {
    progress: Arc<fix::Progress>,
    eta: Eta,
    rx: Receiver<Result<fix::Report>>,
    targets: Vec<usize>,
    dry: bool,
    /// Repair instead of remove.
    repair: bool,
}

enum PagerKind {
    Info,
    /// A dry run; Enter applies it for real if it would do anything.
    DryRun { targets: Vec<usize>, actionable: bool, repair: bool },
    /// The world was modified.
    Done,
}

enum Mode {
    Scanning(ScanJob),
    Browse,
    Filter,
    Working(FixJob),
    Pager { title: String, lines: Vec<String>, scroll: usize, kind: PagerKind },
}

/// Something on screen that reacts to a click.
#[derive(Clone, Copy)]
enum Hit {
    Key(KeyCode),
    Sort(SortKey),
}

struct Button {
    key: &'static str,
    label: String,
    code: KeyCode,
}

impl Button {
    fn new(key: &'static str, label: impl Into<String>, code: KeyCode) -> Self {
        Button { key, label: label.into(), code }
    }

    fn width(&self) -> u16 {
        (self.key.chars().count() + self.label.chars().count() + 3) as u16
    }
}

/// Lay buttons out left to right, wrapping. Returns each one's (x, row) and
/// the number of rows used.
fn flow(buttons: &[Button], width: u16) -> (Vec<(u16, u16)>, u16) {
    let (mut x, mut row) = (0u16, 0u16);
    let mut pos = Vec::with_capacity(buttons.len());
    for b in buttons {
        let w = b.width();
        if x > 0 && x + w > width {
            row += 1;
            x = 0;
        }
        pos.push((x, row));
        x += w + 2;
    }
    (pos, row + 1)
}

/// Pad or cut `s` to exactly `w` columns.
fn fit(s: &str, w: usize) -> String {
    let clean = |c: char| if c.is_control() { ' ' } else { c };
    let n = s.chars().count();
    if n <= w {
        let mut out: String = s.chars().map(clean).collect();
        out.extend(std::iter::repeat_n(' ', w - n));
        out
    } else if w == 0 {
        String::new()
    } else {
        let mut out: String = s.chars().take(w - 1).map(clean).collect();
        out.push('…');
        out
    }
}

/// Break `line` into pieces of at most `width` columns, at spaces where possible.
fn wrap(line: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let chars: Vec<char> = line.chars().collect();
    let mut out = Vec::new();
    let mut start = 0;
    while chars.len() - start > width {
        let end = start + width;
        // Never break inside the indentation.
        let text = start + chars[start..end].iter().take_while(|c| **c == ' ').count();
        match (text + 1..=end).rev().find(|&i| chars[i] == ' ') {
            Some(i) => {
                out.push(chars[start..i].iter().collect());
                start = i + 1;
            }
            None => {
                out.push(chars[start..end].iter().collect());
                start = end;
            }
        }
    }
    out.push(chars[start..].iter().collect());
    out
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(area.x + (area.width - w) / 2, area.y + (area.height - h) / 2, w, h)
}

fn inside(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

struct Cols {
    num: usize,
    place: usize,
    chunk: usize,
    problem: usize,
}

const CHECK_W: usize = 4; // "[x] "
const SEV_W: usize = 5; // "ERROR"

// ---------------------------------------------------------------------------
// Themes
// ---------------------------------------------------------------------------

/// Colour scheme of the full-screen interface.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default, clap::ValueEnum)]
pub enum ThemeName {
    /// The terminal's own background and text colours
    #[default]
    Default,
    /// Midnight Commander: blue panel, cyan bars, grey dialogs
    #[value(alias = "midnight-commander")]
    Mc,
}

impl ThemeName {
    fn next(self) -> Self {
        match self {
            ThemeName::Default => ThemeName::Mc,
            ThemeName::Mc => ThemeName::Default,
        }
    }

    fn flag(self) -> &'static str {
        match self {
            ThemeName::Default => "default",
            ThemeName::Mc => "mc",
        }
    }

    fn label(self) -> &'static str {
        match self {
            ThemeName::Default => "default",
            ThemeName::Mc => "Midnight Commander",
        }
    }

    fn theme(self) -> Theme {
        let s = Style::new();
        match self {
            ThemeName::Default => {
                let p = Palette {
                    base: s,
                    label: s.fg(Color::Cyan),
                    dim: s.fg(Color::DarkGray),
                    error: s.fg(Color::Red),
                    accent: s.fg(Color::Yellow),
                    border: s.fg(Color::Cyan),
                    gauge: s.fg(Color::Cyan).bg(Color::DarkGray),
                    cursor: None,
                };
                Theme {
                    main: Palette { border: s, ..p },
                    popup: p,
                    title: s.fg(Color::Black).bg(Color::Cyan),
                    key: s.fg(Color::Black).bg(Color::Cyan).add_modifier(Modifier::BOLD),
                    key_label: s,
                    head: s.add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
                    mark: s.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                }
            }
            ThemeName::Mc => {
                let cursor = Some(s.fg(Color::Black).bg(Color::Cyan));
                Theme {
                    main: Palette {
                        base: s.fg(Color::Gray).bg(Color::Blue),
                        label: s.fg(Color::LightCyan),
                        dim: s.fg(Color::Cyan),
                        error: s.fg(Color::LightRed),
                        accent: s.fg(Color::Yellow),
                        border: s.fg(Color::Gray),
                        gauge: s.fg(Color::Cyan).bg(Color::Black),
                        cursor,
                    },
                    popup: Palette {
                        base: s.fg(Color::Black).bg(Color::Gray),
                        label: s.fg(Color::Blue),
                        dim: s.fg(Color::DarkGray),
                        error: s.fg(Color::Red),
                        accent: s.fg(Color::Blue),
                        border: s.fg(Color::Black),
                        gauge: s.fg(Color::Blue).bg(Color::White),
                        cursor,
                    },
                    title: s.fg(Color::Black).bg(Color::Cyan),
                    key: s.fg(Color::White).bg(Color::Black),
                    key_label: s.fg(Color::Black).bg(Color::Cyan),
                    head: s.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                    mark: s.fg(Color::Yellow).add_modifier(Modifier::BOLD),
                }
            }
        }
    }
}

/// Colours for one surface: the main screen, or a popup on top of it.
#[derive(Clone, Copy)]
struct Palette {
    /// Background and ordinary text; empty leaves the terminal's own.
    base: Style,
    label: Style,
    dim: Style,
    error: Style,
    accent: Style,
    border: Style,
    gauge: Style,
    /// The highlighted row; `None` reverses whatever the row looks like.
    cursor: Option<Style>,
}

impl Palette {
    fn cursor(&self, row: Style) -> Style {
        match self.cursor {
            Some(c) => row.patch(c),
            None => row.add_modifier(Modifier::REVERSED),
        }
    }
}

#[derive(Clone, Copy)]
struct Theme {
    main: Palette,
    popup: Palette,
    title: Style,
    key: Style,
    key_label: Style,
    head: Style,
    /// A ticked row's checkbox.
    mark: Style,
}

struct App {
    cfg: Config,
    /// The last finished scan.
    out: Option<Arc<ScanOutput>>,
    /// Lower-cased text of each finding, for the filter.
    hay: Vec<String>,
    state: Vec<RowState>,
    selected: Vec<bool>,
    /// Finding indices that pass the filter, in display order.
    view: Vec<usize>,
    cursor: usize,
    offset: usize,
    filter: String,
    sort: SortKey,
    sort_desc: bool,
    mode: Mode,
    mouse: bool,
    theme: ThemeName,
    /// Whether to really switch the terminal's mouse reporting (off in tests).
    control_terminal: bool,
    hits: Vec<(Rect, Hit)>,
    table_area: Rect,
    pager_height: usize,
    /// Rows of the open pager's text once wrapped to its width.
    pager_rows: usize,
    last_click: Option<(Instant, usize)>,
    status: String,
    /// The world was changed since the findings were scanned.
    stale: bool,
    quit: bool,
    fatal: Option<anyhow::Error>,
}

impl App {
    /// With `loaded` the findings are shown straight away; otherwise a scan starts.
    fn new(cfg: Config, loaded: Option<ScanOutput>) -> Self {
        let mut app = App {
            mouse: cfg.mouse,
            theme: cfg.theme,
            cfg,
            out: None,
            hay: Vec::new(),
            state: Vec::new(),
            selected: Vec::new(),
            view: Vec::new(),
            cursor: 0,
            offset: 0,
            filter: String::new(),
            sort: SortKey::Num,
            sort_desc: false,
            mode: Mode::Browse,
            control_terminal: false,
            hits: Vec::new(),
            table_area: Rect::default(),
            pager_height: 10,
            pager_rows: 0,
            last_click: None,
            status: String::new(),
            stale: false,
            quit: false,
            fatal: None,
        };
        match loaded {
            Some(out) => app.set_output(out),
            None => app.start_scan(),
        }
        app
    }

    fn findings(&self) -> &[Finding] {
        self.out.as_ref().map_or(&[], |o| &o.findings)
    }

    fn set_output(&mut self, out: ScanOutput) {
        self.hay = out
            .findings
            .iter()
            .map(|f| {
                format!(
                    "{} {} {} {} {} {} {}",
                    f.severity().name(),
                    f.kind.code(),
                    f.problem(),
                    f.file.rel,
                    f.file.dim,
                    f.file.store.name(),
                    f.chunk_text()
                )
                .to_lowercase()
            })
            .collect();
        self.state = out.findings.iter().map(|_| RowState::Fresh).collect();
        self.selected = vec![false; out.findings.len()];
        self.out = Some(Arc::new(out));
        self.cursor = 0;
        self.offset = 0;
        self.stale = false;
        self.refilter();
    }

    /// Recompute `view` from the filter and sort order, keeping the cursor on
    /// the same finding where possible.
    fn refilter(&mut self) {
        let current = self.view.get(self.cursor).copied();
        let terms: Vec<String> = self.filter.to_lowercase().split_whitespace().map(str::to_string).collect();
        let hay = &self.hay;
        let mut view: Vec<usize> = (0..hay.len()).filter(|&i| terms.iter().all(|t| hay[i].contains(t.as_str()))).collect();
        let f = self.findings();
        match self.sort {
            SortKey::Num => view.sort_by_key(|&i| f[i].n),
            SortKey::Sev => view.sort_by_key(|&i| (f[i].severity(), f[i].n)),
            SortKey::Place => view.sort_by_cached_key(|&i| (f[i].place(), f[i].n)),
            SortKey::Chunk => view.sort_by_key(|&i| (f[i].pos.is_none(), f[i].pos, f[i].n)),
            SortKey::Problem => view.sort_by_key(|&i| (f[i].kind, f[i].n)),
        }
        if self.sort_desc {
            view.reverse();
        }
        self.view = view;
        self.cursor = current.and_then(|c| self.view.iter().position(|&i| i == c)).unwrap_or(0);
    }

    fn set_sort(&mut self, k: SortKey) {
        if self.sort == k {
            self.sort_desc = !self.sort_desc;
        } else {
            self.sort = k;
            self.sort_desc = false;
        }
        self.refilter();
    }

    fn selected_count(&self) -> usize {
        self.selected.iter().filter(|s| **s).count()
    }

    /// What a fix applies to: the selection, or the row under the cursor if
    /// nothing is selected. Rows already removed in this session are left out.
    fn targets(&self) -> Vec<usize> {
        let fresh = |i: &usize| !matches!(self.state[*i], RowState::Done(_));
        if self.selected.iter().any(|s| *s) {
            (0..self.selected.len()).filter(|&i| self.selected[i]).filter(fresh).collect()
        } else {
            self.view.get(self.cursor).copied().into_iter().filter(fresh).collect()
        }
    }

    fn move_cursor(&mut self, delta: i64) {
        if self.view.is_empty() {
            self.cursor = 0;
            return;
        }
        let max = self.view.len() as i64 - 1;
        self.cursor = (self.cursor as i64 + delta).clamp(0, max) as usize;
    }

    fn toggle(&mut self, row: usize) {
        if let Some(&i) = self.view.get(row) {
            self.selected[i] = !self.selected[i];
        }
    }

    fn set_mouse(&mut self, on: bool) {
        if self.control_terminal {
            let ok = if on { execute!(stdout(), EnableMouseCapture) } else { execute!(stdout(), DisableMouseCapture) };
            self.mouse = on && ok.is_ok();
        } else {
            self.mouse = on;
        }
    }

    // -- background jobs ----------------------------------------------------

    fn start_scan(&mut self) {
        let stats = Arc::new(scan::Stats::default());
        let (tx, rx) = mpsc::channel();
        let (world, opts, shared) = (self.cfg.world.clone(), self.cfg.scan.clone(), stats.clone());
        std::thread::spawn(move || {
            let _ = tx.send(scan::run(&world, &opts, &shared));
        });
        self.status.clear();
        self.mode = Mode::Scanning(ScanJob { stats, eta: Eta::new(), rx });
    }

    fn finish_scan(&mut self, job: ScanJob, result: Result<ScanOutput>) {
        self.mode = Mode::Browse;
        let cancelled = job.stats.cancel.load(Ordering::Relaxed);
        match result {
            Ok(out) => {
                let mut status = format!("{} {}", out.verdict(), out.speed_line());
                if out.quick {
                    status.push_str(" - QUICK scan: chunk data was not checked");
                }
                if world::in_use(&out.root) {
                    status.push_str(" - the world is open in a running game/server: stop it before fixing anything");
                }
                self.status = status;
                self.set_output(out);
            }
            // Nothing to fall back to: leave the TUI (and say why, unless the user asked for it).
            Err(e) if self.out.is_none() => {
                if !cancelled {
                    self.fatal = Some(e);
                }
                self.quit = true;
            }
            Err(_) if cancelled => self.status = "Rescan cancelled; these are the earlier findings.".to_string(),
            Err(e) => self.status = format!("Rescan failed: {e:#}"),
        }
    }

    /// Remove or repair `targets` in the background, as a dry run or for real.
    fn start_fix(&mut self, targets: Vec<usize>, dry: bool, repair: bool) {
        let Some(out) = self.out.clone() else { return };
        let progress = Arc::new(fix::Progress::default());
        let (tx, rx) = mpsc::channel();
        let opts = FixOptions { dry_run: dry, backup: self.cfg.backup, force: self.cfg.force, linked: self.cfg.linked };
        let (shared, picked) = (progress.clone(), targets.clone());
        std::thread::spawn(move || {
            let selected: Vec<&Finding> = picked.iter().map(|&i| &out.findings[i]).collect();
            let _ = tx.send(if repair {
                repair::execute(&out.root, &selected, &opts, &shared)
            } else {
                fix::execute(&out.root, &selected, &opts, &shared)
            });
        });
        self.mode = Mode::Working(FixJob { progress, eta: Eta::new(), rx, targets, dry, repair });
    }

    fn finish_fix(&mut self, job: FixJob, result: Result<fix::Report>) {
        let report = match result {
            Ok(r) => r,
            Err(e) => {
                self.message("Could not apply", vec![format!("{e:#}")]);
                return;
            }
        };
        let (chunks, files, _, _) = report.counts();
        let (repaired, hopeless, _) = report.repair_counts();
        let changes = chunks + files + repaired;
        let mut lines = vec![report.summary()];
        if let Some(dir) = &report.backup_dir {
            lines.push(format!("The original of every changed file is in {}", dir.display()));
        } else if report.dry_run && !self.cfg.backup {
            lines.push("Backups are OFF (--no-backup): applying this cannot be undone.".to_string());
        }
        for w in &report.warnings {
            lines.push(format!("warning: {w}"));
        }
        if hopeless > 0 {
            lines.push("What cannot be repaired can only be removed (d), so that the game generates it anew.".to_string());
        }
        lines.push(String::new());
        let findings = self.findings();
        for (o, &i) in report.outcomes.iter().zip(&job.targets) {
            lines.push(o.line(&findings[i], report.dry_run));
        }

        if job.dry {
            self.mode = Mode::Pager {
                title: "Preview (dry run) - nothing has been written yet".to_string(),
                lines,
                scroll: 0,
                kind: PagerKind::DryRun { targets: job.targets, actionable: changes > 0, repair: job.repair },
            };
            return;
        }
        for (o, &i) in report.outcomes.iter().zip(&job.targets) {
            self.selected[i] = false;
            match &o.status {
                Status::Removed => self.state[i] = RowState::Done("removed".to_string()),
                Status::Repaired(_) => self.state[i] = RowState::Done("repaired".to_string()),
                Status::Skipped(why) => self.state[i] = RowState::Skipped(why.clone()),
                Status::NoFix | Status::NotRepairable(_) => {}
            }
        }
        if changes > 0 {
            self.stale = true;
        }
        self.status = report.summary();
        self.mode = Mode::Pager { title: "Applied".to_string(), lines, scroll: 0, kind: PagerKind::Done };
    }

    fn tick(&mut self) {
        fn poll<T>(rx: &Receiver<Result<T>>) -> Option<Result<T>> {
            match rx.try_recv() {
                Ok(r) => Some(r),
                Err(TryRecvError::Empty) => None,
                Err(TryRecvError::Disconnected) => Some(Err(anyhow!("the worker thread stopped unexpectedly"))),
            }
        }
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Scanning(mut job) => {
                job.eta.update(job.stats.bytes_done.load(Ordering::Relaxed));
                match poll(&job.rx) {
                    Some(result) => self.finish_scan(job, result),
                    None => self.mode = Mode::Scanning(job),
                }
            }
            Mode::Working(mut job) => {
                job.eta.update(job.progress.done.load(Ordering::Relaxed));
                match poll(&job.rx) {
                    Some(result) => self.finish_fix(job, result),
                    None => self.mode = Mode::Working(job),
                }
            }
            other => self.mode = other,
        }
    }

    fn busy(&self) -> bool {
        matches!(self.mode, Mode::Scanning(_) | Mode::Working(_))
    }

    // -- popups -------------------------------------------------------------

    fn message(&mut self, title: &str, lines: Vec<String>) {
        self.mode = Mode::Pager { title: title.to_string(), lines, scroll: 0, kind: PagerKind::Info };
    }

    fn help(&mut self) {
        let text = "\
Moving
  Up/Down, j/k        one finding            PgUp/PgDn   one page
  Home/End, g/G       first / last           mouse wheel scroll

Selecting
  Space               select / unselect the current finding
  a / n / i           select all shown / none / invert shown
  e                   select the errors that are shown (not the warnings)
  click [ ] or right-click a row   select it

Looking
  Enter, double-click everything about the finding: where it is, what the
                      game does with it, what fixing it means
  /                   filter: words that must all appear (error, warning,
                      nether, entities, corrupt, a file name, 12,-5 ...)
  s / S               next sort column / reverse    (or click a column header)

Fixing  (the selection, or the current finding if nothing is selected)
  d                   remove: unloadable chunks are taken out of their region
                      file so the game generates them anew; unused files are
                      deleted. A terrain chunk takes its entity and
                      point-of-interest data with it.
  p                   repair: put right what can be put right without losing
                      anything - bad length fields, compression markers and
                      sector counts, header entries that lost their data,
                      chunks filed under the wrong position. Removes nothing;
                      what cannot be repaired is listed and left as it is.
  Both first run as a dry run and show exactly what they would do; nothing
  is written until you confirm that preview.

Other
  c                   what the last scan looked into
  w                   save the findings as a JSON-lines report
  r                   scan the world again
  m                   mouse support on/off (off lets the terminal select text)
  t                   switch theme: default / Midnight Commander
  ?                   this help              q, Ctrl-C   quit

Scanning only reads. Changes are refused while a game or server has the
world open, chunks saved since the scan are skipped, and every file is copied
to <world>/mcfsck-backups/ before it is changed.";
        self.message("Keys", text.lines().map(str::to_string).collect());
    }

    fn details(&mut self) {
        let Some(&i) = self.view.get(self.cursor) else { return };
        let f = &self.findings()[i];
        let mut lines = report::details(f);
        match &self.state[i] {
            RowState::Fresh => {}
            RowState::Done(what) => lines.push(format!("Status: {what} in this session.")),
            RowState::Skipped(why) => lines.push(format!("Status: skipped - {why}")),
        }
        let title = format!("Finding #{}", f.n);
        self.message(&title, lines);
    }

    fn coverage(&mut self) {
        let Some(out) = &self.out else { return };
        let mut lines = out.coverage_lines();
        lines.push(out.verdict());
        lines.push(String::new());
        lines.push(format!("World: {}", out.root.display()));
        self.message("What the scan looked into", lines);
    }

    fn save_report(&mut self) {
        let Some(out) = &self.out else { return };
        self.status = match report::save_jsonl(&self.cfg.report_path, out) {
            Ok(()) => format!(
                "Report saved to {}{}",
                self.cfg.report_path.display(),
                if self.stale { " (as scanned, before the changes made since)" } else { "" }
            ),
            Err(e) => format!("Report NOT saved: {e:#}"),
        };
    }

    // -- actions ------------------------------------------------------------

    /// Start the dry run of the fix.
    fn begin_fix(&mut self, repair: bool) {
        let targets = self.targets();
        if targets.is_empty() {
            self.status = "Nothing to act on.".to_string();
        } else if repair {
            self.start_fix(targets, true, true);
        } else if targets.iter().all(|&i| self.findings()[i].fix() == Fix::None) {
            self.message(
                "Nothing to remove",
                vec![
                    "There is nothing to remove for this: the file could not be read at all, or".to_string(),
                    "the chunk could not be checked. Enter on the finding shows the details.".to_string(),
                ],
            );
        } else {
            self.start_fix(targets, true, false);
        }
    }

    // -- input --------------------------------------------------------------

    fn on_key(&mut self, k: KeyEvent) {
        if k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char('c') {
            // Never while files are being changed.
            if matches!(&self.mode, Mode::Working(job) if !job.dry) {
                return;
            }
            if let Mode::Scanning(job) = &self.mode {
                job.stats.cancel.store(true, Ordering::Relaxed);
            }
            self.quit = true;
            return;
        }
        match std::mem::replace(&mut self.mode, Mode::Browse) {
            Mode::Scanning(job) => {
                if matches!(k.code, KeyCode::Esc | KeyCode::Char('q')) {
                    job.stats.cancel.store(true, Ordering::Relaxed);
                    self.status = "Cancelling...".to_string();
                }
                self.mode = Mode::Scanning(job);
            }
            // Never interrupted: it may be in the middle of changing files.
            Mode::Working(job) => self.mode = Mode::Working(job),
            Mode::Browse => self.browse_key(k),
            Mode::Filter => match k.code {
                KeyCode::Enter => {}
                KeyCode::Esc => {
                    self.filter.clear();
                    self.refilter();
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.refilter();
                    self.mode = Mode::Filter;
                }
                KeyCode::Char(c) => {
                    self.filter.push(c);
                    self.refilter();
                    self.mode = Mode::Filter;
                }
                _ => self.mode = Mode::Filter,
            },
            Mode::Pager { title, lines, scroll, kind } => {
                let page = self.pager_height.max(1);
                let last = self.pager_rows.max(lines.len()).saturating_sub(page);
                let scrolled = match k.code {
                    KeyCode::Up | KeyCode::Char('k') => Some(scroll.saturating_sub(1)),
                    KeyCode::Down | KeyCode::Char('j') => Some((scroll + 1).min(last)),
                    KeyCode::PageUp => Some(scroll.saturating_sub(page)),
                    KeyCode::PageDown | KeyCode::Char(' ') => Some((scroll + page).min(last)),
                    KeyCode::Home | KeyCode::Char('g') => Some(0),
                    KeyCode::End | KeyCode::Char('G') => Some(last),
                    _ => None,
                };
                if let Some(scroll) = scrolled {
                    self.mode = Mode::Pager { title, lines, scroll, kind };
                    return;
                }
                match (k.code, kind) {
                    (KeyCode::Esc | KeyCode::Char('q'), _) => {}
                    (KeyCode::Enter | KeyCode::Char('y'), PagerKind::DryRun { targets, actionable: true, repair }) => {
                        self.start_fix(targets, false, repair)
                    }
                    (KeyCode::Enter, PagerKind::Info | PagerKind::Done) => {}
                    (KeyCode::Char('r'), PagerKind::Done) => self.start_scan(),
                    (_, kind) => self.mode = Mode::Pager { title, lines, scroll, kind },
                }
            }
        }
    }

    fn browse_key(&mut self, k: KeyEvent) {
        let page = (self.table_area.height as i64).max(1);
        match k.code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                    self.refilter();
                }
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_cursor(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_cursor(1),
            KeyCode::PageUp => self.move_cursor(-page),
            KeyCode::PageDown => self.move_cursor(page),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = self.view.len().saturating_sub(1),
            KeyCode::Char(' ') => {
                self.toggle(self.cursor);
                self.move_cursor(1);
            }
            KeyCode::Char('a') => {
                for &i in &self.view {
                    self.selected[i] = true;
                }
            }
            KeyCode::Char('n') => self.selected.iter_mut().for_each(|s| *s = false),
            KeyCode::Char('i') => {
                for &i in &self.view {
                    self.selected[i] = !self.selected[i];
                }
            }
            KeyCode::Char('e') => {
                let errors: Vec<usize> =
                    self.view.iter().copied().filter(|&i| self.findings()[i].severity() == Severity::Error).collect();
                for &i in &errors {
                    self.selected[i] = true;
                }
                self.status = format!("Selected the {} error{} shown.", human_count(errors.len() as u64), s(errors.len() as u64));
            }
            KeyCode::Char('d') | KeyCode::Char('f') | KeyCode::Delete => self.begin_fix(false),
            KeyCode::Char('p') => self.begin_fix(true),
            KeyCode::Enter => self.details(),
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('s') => {
                let next = match self.sort {
                    SortKey::Num => SortKey::Sev,
                    SortKey::Sev => SortKey::Place,
                    SortKey::Place => SortKey::Chunk,
                    SortKey::Chunk => SortKey::Problem,
                    SortKey::Problem => SortKey::Num,
                };
                self.set_sort(next);
            }
            KeyCode::Char('S') => self.set_sort(self.sort),
            KeyCode::Char('r') => self.start_scan(),
            KeyCode::Char('c') => self.coverage(),
            KeyCode::Char('w') => self.save_report(),
            KeyCode::Char('m') => {
                self.set_mouse(!self.mouse);
                self.status = if self.mouse {
                    "Mouse support on.".to_string()
                } else {
                    "Mouse support off - the terminal's own text selection works again.".to_string()
                };
            }
            KeyCode::Char('t') => {
                self.theme = self.theme.next();
                self.status = format!("Theme: {} (start with --theme {} to keep it).", self.theme.label(), self.theme.flag());
            }
            KeyCode::Char('?') | KeyCode::F(1) => self.help(),
            _ => {}
        }
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        let wheel = match m.kind {
            MouseEventKind::ScrollUp => -3i64,
            MouseEventKind::ScrollDown => 3,
            _ => 0,
        };
        if wheel != 0 {
            match &mut self.mode {
                Mode::Browse | Mode::Filter => self.move_cursor(wheel),
                Mode::Pager { lines, scroll, .. } => {
                    let last = self.pager_rows.max(lines.len()).saturating_sub(self.pager_height.max(1));
                    *scroll = (*scroll as i64 + wheel).clamp(0, last as i64) as usize;
                }
                _ => {}
            }
            return;
        }
        let button = match m.kind {
            MouseEventKind::Down(b) => b,
            _ => return,
        };
        let (x, y) = (m.column, m.row);
        if button == MouseButton::Left {
            // Later entries are drawn on top of earlier ones.
            if let Some(&(_, hit)) = self.hits.iter().rev().find(|(r, _)| inside(*r, x, y)) {
                match hit {
                    Hit::Key(code) => self.on_key(key(code)),
                    Hit::Sort(k) => self.set_sort(k),
                }
                return;
            }
        }
        if !matches!(self.mode, Mode::Browse | Mode::Filter) || !inside(self.table_area, x, y) {
            return;
        }
        let row = self.offset + (y - self.table_area.y) as usize;
        if row >= self.view.len() {
            return;
        }
        self.mode = Mode::Browse;
        self.cursor = row;
        match button {
            MouseButton::Right => self.toggle(row),
            MouseButton::Left if (x - self.table_area.x) < CHECK_W as u16 => self.toggle(row),
            MouseButton::Left => {
                let now = Instant::now();
                let double = matches!(self.last_click, Some((t, r)) if r == row && now.duration_since(t) < Duration::from_millis(450));
                self.last_click = if double { None } else { Some((now, row)) };
                if double {
                    self.details();
                }
            }
            _ => {}
        }
    }

    // -- drawing ------------------------------------------------------------

    fn draw(&mut self, f: &mut Frame) {
        self.hits.clear();
        let area = f.area();
        f.render_widget(Block::new().style(self.theme.theme().main.base), area);
        let mode = std::mem::replace(&mut self.mode, Mode::Browse);
        match &mode {
            Mode::Scanning(job) => {
                if self.out.is_some() {
                    self.draw_browse(f, area, false, false);
                }
                self.draw_scan(f, area, job);
            }
            Mode::Browse => self.draw_browse(f, area, true, false),
            Mode::Filter => self.draw_browse(f, area, true, true),
            Mode::Working(job) => {
                self.draw_browse(f, area, false, false);
                self.draw_working(f, area, job);
            }
            Mode::Pager { title, lines, scroll, kind } => {
                self.draw_browse(f, area, false, false);
                self.draw_pager(f, area, title, lines, *scroll, kind);
            }
        }
        self.mode = mode;
    }

    fn draw_buttons(&mut self, f: &mut Frame, area: Rect, buttons: &[Button], active: bool) {
        let t = self.theme.theme();
        let (pos, _) = flow(buttons, area.width);
        for (b, (x, row)) in buttons.iter().zip(pos) {
            if row >= area.height || x >= area.width {
                break;
            }
            let rect = Rect::new(area.x + x, area.y + row, b.width().min(area.width - x), 1);
            let line = Line::from(vec![Span::styled(format!(" {} ", b.key), t.key), Span::styled(format!(" {}", b.label), t.key_label)]);
            f.render_widget(Paragraph::new(line), rect);
            if active {
                self.hits.push((rect, Hit::Key(b.code)));
            }
        }
    }

    fn browse_buttons(&self) -> Vec<Button> {
        let c = KeyCode::Char;
        vec![
            Button::new("Space", "Select", c(' ')),
            Button::new("a", "All", c('a')),
            Button::new("e", "Errors", c('e')),
            Button::new("n", "None", c('n')),
            Button::new("i", "Invert", c('i')),
            Button::new("p", "Repair", c('p')),
            Button::new("d", "Remove (both preview first)", c('d')),
            Button::new("Enter", "Details", KeyCode::Enter),
            Button::new("/", "Filter", c('/')),
            Button::new("s", "Sort", c('s')),
            Button::new("r", "Rescan", c('r')),
            Button::new("w", "Save report", c('w')),
            Button::new("m", if self.mouse { "Mouse: on" } else { "Mouse: off" }, c('m')),
            Button::new("t", "Theme", c('t')),
            Button::new("?", "Help", c('?')),
            Button::new("q", "Quit", c('q')),
        ]
    }

    fn cols(&self, width: usize) -> Cols {
        let num = format!("#{}", self.findings().len()).len().max(3);
        let rest = width.saturating_sub(CHECK_W + num + 1 + SEV_W + 2 + 2 + 2);
        let chunk = 13.min(rest);
        // "overworld region" should fit even in 80 columns.
        let place = ((rest - chunk) * 30 / 100).clamp(16, 30).min(rest - chunk);
        Cols { num, place, chunk, problem: rest - chunk - place }
    }

    fn draw_browse(&mut self, f: &mut Frame, area: Rect, active: bool, editing_filter: bool) {
        let t = self.theme.theme();
        let p = t.main;
        let buttons = self.browse_buttons();
        let (_, bar_rows) = flow(&buttons, area.width);
        let detail_rows = if area.height >= 20 { 6 } else { 0 };
        let [title, head, table, detail, status, bar] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(detail_rows),
            Constraint::Length(1),
            Constraint::Length(bar_rows),
        ])
        .areas(area);

        // Title bar.
        let total = self.findings().len();
        let errors = self.findings().iter().filter(|x| x.severity() == Severity::Error).count();
        let mut text = format!(
            " mcfsck │ {} │ {} error{}, {} warning{}",
            self.out.as_ref().map_or(self.cfg.world.as_path(), |o| o.root.as_path()).display(),
            human_count(errors as u64),
            s(errors as u64),
            human_count((total - errors) as u64),
            s((total - errors) as u64),
        );
        if self.cfg.scan.quick {
            text.push_str(" │ QUICK scan");
        }
        if self.view.len() != total {
            text.push_str(&format!(" │ {} shown", human_count(self.view.len() as u64)));
        }
        text.push_str(&format!(" │ {} selected", human_count(self.selected_count() as u64)));
        if self.stale {
            text.push_str(" │ world changed: press r to rescan");
        }
        f.render_widget(Paragraph::new(fit(&text, title.width as usize)).style(t.title), title);

        // Column headers (click to sort).
        let c = self.cols(area.width as usize);
        let mark = |k: SortKey| match (self.sort == k, self.sort_desc) {
            (false, _) => "",
            (true, false) => " ▲",
            (true, true) => " ▼",
        };
        let x_sev = CHECK_W + c.num + 1;
        let x_place = x_sev + SEV_W + 2;
        let x_chunk = x_place + c.place + 2;
        let x_problem = x_chunk + c.chunk + 2;
        let heads = [
            (SortKey::Num, "#", CHECK_W, c.num),
            (SortKey::Sev, "SEV", x_sev, SEV_W),
            (SortKey::Place, "WHERE", x_place, c.place),
            (SortKey::Chunk, "CHUNK", x_chunk, c.chunk),
            (SortKey::Problem, "PROBLEM", x_problem, c.problem),
        ];
        for (k, name, x, w) in heads {
            if w == 0 || x >= head.width as usize {
                continue;
            }
            let rect = Rect::new(head.x + x as u16, head.y, (w as u16).min(head.width - x as u16), 1);
            f.render_widget(Paragraph::new(fit(&format!("{name}{}", mark(k)), w)).style(t.head), rect);
            if active {
                self.hits.push((rect, Hit::Sort(k)));
            }
        }

        // Rows: only the visible window is formatted, so huge lists stay fast.
        self.table_area = table;
        let h = table.height as usize;
        if self.cursor < self.offset {
            self.offset = self.cursor;
        } else if h > 0 && self.cursor >= self.offset + h {
            self.offset = self.cursor + 1 - h;
        }
        self.offset = self.offset.min(self.view.len().saturating_sub(1));
        if self.view.is_empty() {
            let msg = match (&self.out, total) {
                (None, _) => "",
                (Some(_), 0) => "No problems found - every chunk that was checked can be read.",
                _ => "Nothing matches the filter (Esc clears it).",
            };
            let mut text = format!("\n  {msg}\n");
            if let Some(out) = &self.out {
                for line in out.coverage_lines() {
                    text.push_str(&format!("\n  {line}"));
                }
            }
            f.render_widget(Paragraph::new(text).style(p.dim).wrap(Wrap { trim: false }), table);
        } else {
            let findings = self.findings();
            let mut lines = Vec::with_capacity(h);
            for (row, &i) in self.view.iter().enumerate().skip(self.offset).take(h) {
                let r = &findings[i];
                let (problem, style) = match &self.state[i] {
                    RowState::Fresh => (r.problem(), Style::new()),
                    RowState::Done(what) => (format!("{what}: {}", r.kind.label()), p.dim),
                    RowState::Skipped(why) => (format!("skipped: {why}"), p.error),
                };
                let fresh = matches!(self.state[i], RowState::Fresh);
                let style = if row == self.cursor { p.cursor(style) } else { style };
                let check_style = if self.selected[i] { style.patch(t.mark) } else { style };
                let sev_style = match (fresh, r.severity()) {
                    (false, _) => style,
                    (true, Severity::Error) => style.patch(p.error).add_modifier(Modifier::BOLD),
                    (true, Severity::Warning) => style.patch(p.accent),
                };
                lines.push(Line::from(vec![
                    Span::styled(if self.selected[i] { "[x] " } else { "[ ] " }, check_style),
                    Span::styled(format!("{} ", fit(&format!("#{}", r.n), c.num)), style),
                    Span::styled(fit(r.severity().short(), SEV_W), sev_style),
                    Span::styled(
                        format!("  {}  {}  {}", fit(&r.place(), c.place), fit(&r.chunk_text(), c.chunk), fit(&problem, c.problem)),
                        style,
                    ),
                ]));
            }
            f.render_widget(Paragraph::new(lines), table);
        }

        // Details of the current finding.
        if detail_rows > 0 {
            let block = Block::new().borders(Borders::TOP).title(" Details ").border_style(p.border);
            let inner = block.inner(detail);
            f.render_widget(block, detail);
            if let Some(&i) = self.view.get(self.cursor) {
                let r = &self.findings()[i];
                let label = p.label;
                let fix = match r.fix() {
                    Fix::DeleteChunk if r.severity() == Severity::Error => "remove the chunk; the game generates it anew",
                    Fix::DeleteChunk => "none needed, it still loads (it can be removed if you select it)",
                    Fix::DeleteFile => "remove the file",
                    Fix::None => "nothing to remove",
                };
                let lines = vec![
                    Line::from(vec![
                        Span::styled(format!("#{} ", r.n), label),
                        Span::styled(r.kind.label(), Style::new().add_modifier(Modifier::BOLD)),
                        Span::raw(format!("   {}", r.target())),
                    ]),
                    Line::from(vec![Span::styled("what  ", label), Span::raw(r.detail.clone())]),
                    Line::from(vec![Span::styled("where ", label), Span::raw(format!("{}, {} store", r.file.dim, r.file.store.name()))]),
                    Line::from(vec![Span::styled("fix   ", label), Span::raw(fix)]),
                ];
                f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
            }
        }

        // Status / filter line.
        let line = if editing_filter {
            Line::from(vec![
                Span::styled("Filter: ", p.accent.add_modifier(Modifier::BOLD)),
                Span::raw(self.filter.clone()),
                Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
                Span::styled("   Enter keeps it, Esc clears it", p.dim),
            ])
        } else if !self.filter.is_empty() {
            Line::from(vec![
                Span::styled(format!("Filter: {}", self.filter), p.accent),
                Span::styled("  (Esc clears)   ", p.dim),
                Span::raw(self.status.clone()),
            ])
        } else {
            Line::from(self.status.clone())
        };
        f.render_widget(Paragraph::new(line), status);

        self.draw_buttons(f, bar, &buttons, active);
    }

    fn popup(&self, f: &mut Frame, rect: Rect, title: &str) -> Rect {
        let p = self.theme.theme().popup;
        let block = Block::bordered().title(format!(" {title} ")).style(p.base).border_style(p.border);
        let inner = block.inner(rect);
        f.render_widget(Clear, rect);
        f.render_widget(block, rect);
        // One column of padding left and right.
        Rect::new(inner.x + 1.min(inner.width), inner.y, inner.width.saturating_sub(2), inner.height)
    }

    fn draw_scan(&mut self, f: &mut Frame, area: Rect, job: &ScanJob) {
        let p = self.theme.theme().popup;
        let st = &job.stats;
        let load = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
        let total = load(&st.total_bytes);
        let done = load(&st.bytes_done).min(total);
        let ratio = if total == 0 { 0.0 } else { done as f64 / total as f64 };
        let title = if self.cfg.scan.quick { "mcfsck - quick scan (headers only)" } else { "mcfsck - scanning" };
        let inner = self.popup(f, centered(area, 76, 14), title);
        let [world_row, _, gauge, _, stats, _, status, bar] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(5),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        f.render_widget(Paragraph::new(fit(&self.cfg.world.display().to_string(), world_row.width as usize)), world_row);
        f.render_widget(
            Gauge::default()
                .gauge_style(p.gauge)
                .ratio(ratio.clamp(0.0, 1.0))
                .label(format!("{:.0}%", ratio * 100.0))
                .use_unicode(true),
            gauge,
        );
        let lines = if load(&st.total_files) == 0 {
            vec![Line::from("Looking for region files...")]
        } else {
            let label = p.label;
            let row = |a: &str, av: String, b: &str, bv: String| {
                Line::from(vec![
                    Span::styled(format!("{a:<13}"), label),
                    Span::raw(format!("{av:<24}")),
                    Span::styled(format!("{b:<13}"), label),
                    Span::raw(bv),
                ])
            };
            vec![
                row(
                    "Files",
                    format!("{} / {}", human_count(load(&st.files_done)), human_count(load(&st.total_files))),
                    "Chunks",
                    human_count(load(&st.chunks)),
                ),
                row("Data", format!("{} / {}", human_bytes(done), human_bytes(total)), "Errors", human_count(load(&st.errors))),
                row("Speed", format!("{}/s", human_bytes(job.eta.rate() as u64)), "Warnings", human_count(load(&st.warnings))),
                Line::default(),
                row("Elapsed", human_duration(job.eta.elapsed()), "Time left", job.eta.text(total)),
            ]
        };
        f.render_widget(Paragraph::new(lines), stats);
        f.render_widget(Paragraph::new(self.status.clone()).style(p.accent), status);
        self.draw_buttons(f, bar, &[Button::new("Esc", "Cancel", KeyCode::Esc)], true);
    }

    fn draw_working(&mut self, f: &mut Frame, area: Rect, job: &FixJob) {
        let p = self.theme.theme().popup;
        let total = job.progress.total.load(Ordering::Relaxed);
        let done = job.progress.done.load(Ordering::Relaxed).min(total);
        let ratio = if total == 0 { 0.0 } else { done as f64 / total as f64 };
        let title = match (job.dry, job.repair) {
            (true, false) => "Working out what would change (dry run)",
            (true, true) => "Working out what can be repaired (dry run)",
            (false, false) => "Applying changes to the world",
            (false, true) => "Repairing chunks",
        };
        let inner = self.popup(f, centered(area, 70, 8), title);
        let [what, _, gauge, _, line] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(inner);
        let n = job.targets.len() as u64;
        let what_text = format!("Removing what cannot be loaded - {} finding{}", human_count(n), s(n));
        f.render_widget(Paragraph::new(fit(&what_text, what.width as usize)), what);
        f.render_widget(
            Gauge::default()
                .gauge_style(p.gauge)
                .ratio(ratio.clamp(0.0, 1.0))
                .label(format!("{:.0}%", ratio * 100.0))
                .use_unicode(true),
            gauge,
        );
        let text = format!(
            "Files {} / {}     Elapsed {}     Time left {}",
            human_count(done),
            human_count(total),
            human_duration(job.eta.elapsed()),
            job.eta.text(total)
        );
        f.render_widget(Paragraph::new(text), line);
    }

    fn draw_pager(&mut self, f: &mut Frame, area: Rect, title: &str, lines: &[String], scroll: usize, kind: &PagerKind) {
        let p = self.theme.theme().popup;
        let widest = lines.iter().map(|l| l.chars().count()).max().unwrap_or(0).min(u16::MAX as usize - 8) as u16;
        let w = (widest + 4).clamp(50, area.width.saturating_sub(4).max(50));
        // Wrap here rather than in the widget, so the size and the scrolling know the real row count.
        let text_width = (w.min(area.width).saturating_sub(4) as usize).max(1);
        let mut rows: Vec<(String, Style)> = Vec::with_capacity(lines.len());
        for l in lines {
            let style = if l.starts_with("SKIPPED ") || l.starts_with("warning:") || l.starts_with("Backups are OFF") {
                p.error
            } else if l.starts_with("DRY RUN") || l.starts_with("Done:") {
                Style::new().add_modifier(Modifier::BOLD)
            } else if l.starts_with("NO FIX ") {
                p.dim
            } else {
                Style::new()
            };
            rows.extend(wrap(l, text_width).into_iter().map(|piece| (piece, style)));
        }
        let h = (rows.len().min(u16::MAX as usize - 8) as u16 + 4).clamp(7, area.height.saturating_sub(2).max(7));
        let inner = self.popup(f, centered(area, w, h), title);
        let [body, _, bar] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(1)]).areas(inner);
        self.pager_height = body.height as usize;
        self.pager_rows = rows.len();
        let scroll = scroll.min(rows.len().saturating_sub(body.height as usize));
        let more = rows.len() > body.height as usize;
        let shown: Vec<Line> =
            rows.into_iter().skip(scroll).take(body.height as usize).map(|(text, style)| Line::styled(text, style)).collect();
        f.render_widget(Paragraph::new(shown), body);

        let mut buttons = match kind {
            PagerKind::Info => vec![Button::new("Esc", "Close", KeyCode::Esc)],
            PagerKind::DryRun { actionable: true, repair, .. } => vec![
                Button::new("Enter", if *repair { "Make these repairs" } else { "Apply this to the world" }, KeyCode::Enter),
                Button::new("Esc", "Cancel", KeyCode::Esc),
            ],
            PagerKind::DryRun { .. } => vec![Button::new("Esc", "Close - nothing to apply", KeyCode::Esc)],
            PagerKind::Done => {
                vec![Button::new("Esc", "Close", KeyCode::Esc), Button::new("r", "Rescan", KeyCode::Char('r'))]
            }
        };
        if more {
            buttons.push(Button::new("PgDn", "More", KeyCode::PageDown));
            buttons.push(Button::new("PgUp", "Back", KeyCode::PageUp));
        }
        self.draw_buttons(f, bar, &buttons, true);
    }

    fn event_loop<B: Backend>(&mut self, terminal: &mut Terminal<B>) -> Result<()> {
        while !self.quit {
            self.tick();
            terminal.draw(|f| self.draw(f))?;
            let wait = Duration::from_millis(if self.busy() { 100 } else { 500 });
            if !event::poll(wait)? {
                continue;
            }
            // Handle everything that is queued before drawing again.
            loop {
                match event::read()? {
                    Event::Key(k) if k.kind == KeyEventKind::Press => self.on_key(k),
                    Event::Mouse(m) => self.on_mouse(m),
                    _ => {}
                }
                if self.quit || !event::poll(Duration::ZERO)? {
                    break;
                }
            }
        }
        Ok(())
    }
}

pub fn run(cfg: Config) -> Result<()> {
    if !stdout().is_terminal() || !stdin().is_terminal() {
        bail!("the TUI needs an interactive terminal; use `scan` and `fix` in scripts");
    }
    let want_mouse = cfg.mouse;
    let mut app = App::new(cfg, None);
    app.control_terminal = true;

    let mut terminal = ratatui::init();
    // ratatui's hook restores the screen on a panic; mouse reporting is ours to undo.
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(stdout(), DisableMouseCapture);
        previous(info);
    }));
    app.set_mouse(want_mouse);
    let result = app.event_loop(&mut terminal);
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();

    result?;
    if let Some(e) = app.fatal {
        return Err(e);
    }
    if let Some(out) = &app.out {
        if app.stale {
            println!("The world was changed in this session; scan it again to see what is left.");
        } else {
            println!("{}", out.verdict());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::tests::{sample_world, scan};
    use crate::scan::Kind;
    use crate::testutil::TempDir;
    use ratatui::backend::TestBackend;

    fn config(world: &std::path::Path) -> Config {
        Config {
            world: world.to_path_buf(),
            scan: scan::Options { quick: false, threads: Some(2) },
            report_path: world.join("report.jsonl"),
            mouse: true,
            theme: ThemeName::Default,
            backup: true,
            force: false,
            linked: true,
        }
    }

    /// An app showing the findings of the sample world.
    fn app(tmp: &TempDir) -> App {
        sample_world(tmp.path());
        App::new(config(tmp.path()), Some(scan(tmp.path(), false, 2)))
    }

    fn screen(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()).collect()
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(key(code));
    }

    fn click(app: &mut App, x: u16, y: u16, button: MouseButton) {
        app.on_mouse(MouseEvent { kind: MouseEventKind::Down(button), column: x, row: y, modifiers: KeyModifiers::NONE });
    }

    fn find(lines: &[String], needle: &str) -> (u16, u16) {
        for (y, line) in lines.iter().enumerate() {
            if let Some(byte) = line.find(needle) {
                return (line[..byte].chars().count() as u16, y as u16);
            }
        }
        panic!("{needle:?} is not on screen:\n{}", lines.join("\n"));
    }

    /// Let the background job finish.
    fn wait(app: &mut App) {
        for _ in 0..2000 {
            app.tick();
            if !app.busy() {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        panic!("the background job did not finish");
    }

    fn kinds(app: &App) -> Vec<Kind> {
        app.view.iter().map(|&i| app.findings()[i].kind).collect()
    }

    #[test]
    fn text_fitting() {
        assert_eq!(fit("abc", 5), "abc  ");
        assert_eq!(fit("abcdef", 4), "abc…");
        assert_eq!(fit("abc", 0), "");
        assert_eq!(fit("a\nb", 3), "a b");
        assert_eq!(fit("äöü", 2), "ä…");
    }

    #[test]
    fn text_wrapping() {
        assert_eq!(wrap("short", 10), ["short"]);
        assert_eq!(wrap("", 10), [""]);
        assert_eq!(wrap("one two three four", 9), ["one two", "three", "four"]);
        assert_eq!(wrap("exactly10!", 10), ["exactly10!"]);
        assert_eq!(wrap("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap("    indented text here", 12), ["    indented", "text here"]);
        assert_eq!(wrap("ab cd", 0), ["a", "b", "c", "d"]);
        for width in 1..30 {
            for piece in wrap("The stored data is damaged and the game cannot load it.", width) {
                assert!(piece.chars().count() <= width);
            }
        }
    }

    #[test]
    fn buttons_wrap() {
        let buttons = vec![Button::new("a", "All", KeyCode::Char('a')), Button::new("n", "None", KeyCode::Char('n'))];
        assert_eq!(flow(&buttons, 80), (vec![(0, 0), (9, 0)], 1));
        assert_eq!(flow(&buttons, 12), (vec![(0, 0), (0, 1)], 2));
    }

    #[test]
    fn renders_findings_and_buttons() {
        let tmp = TempDir::new("tui-render");
        let mut app = app(&tmp);
        let lines = screen(&mut app, 140, 40);
        let all = lines.join("\n");
        assert!(lines[0].contains("mcfsck") && lines[0].contains("19 errors, 5 warnings") && lines[0].contains("0 selected"), "{}", lines[0]);
        assert!(lines[1].contains("# ▲") && lines[1].contains("WHERE") && lines[1].contains("PROBLEM"), "{}", lines[1]);
        assert!(all.contains("[ ] #1 "), "{all}");
        assert!(all.contains("ERROR") && all.contains("warn "));
        assert!(all.contains("overworld region") && all.contains("overworld entities"));
        assert!(all.contains("wrong chunk position"));
        assert!(all.contains("Details") && all.contains("what  ") && all.contains("fix   "));
        assert!(all.contains(" p  Repair") && all.contains(" d  Remove (both preview first)") && all.contains(" q  Quit"));
        // The same in the other theme.
        press(&mut app, KeyCode::Char('t'));
        assert_eq!(app.theme, ThemeName::Mc);
        assert!(screen(&mut app, 140, 40).join("\n").contains("[ ] #1 "));
    }

    #[test]
    fn filter_and_sort() {
        let tmp = TempDir::new("tui-filter");
        let mut app = app(&tmp);
        assert_eq!(app.view.len(), 24);
        press(&mut app, KeyCode::Char('/'));
        for c in "warning orphan".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(matches!(app.mode, Mode::Filter));
        assert_eq!(kinds(&app), [Kind::OrphanMcc, Kind::OrphanMcc]);
        assert!(screen(&mut app, 120, 30).join("\n").contains("Filter: warning orphan"));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(app.mode, Mode::Browse));
        assert_eq!(app.view.len(), 2);
        assert!(screen(&mut app, 120, 30)[0].contains("2 shown"));
        press(&mut app, KeyCode::Esc);
        assert_eq!(app.view.len(), 24);

        // Sorting by severity puts the errors first; reversed, the warnings.
        press(&mut app, KeyCode::Char('s'));
        assert_eq!(app.sort, SortKey::Sev);
        assert_eq!(app.findings()[app.view[0]].severity(), Severity::Error);
        press(&mut app, KeyCode::Char('S'));
        assert_eq!(app.findings()[app.view[0]].severity(), Severity::Warning);
        // The cursor stays on the same finding when the order changes.
        press(&mut app, KeyCode::Down);
        let current = app.view[app.cursor];
        for _ in 0..4 {
            press(&mut app, KeyCode::Char('s'));
            assert_eq!(app.view[app.cursor], current);
        }
        assert_eq!(app.sort, SortKey::Num);
        // Chunks sort numerically, whole files last.
        app.set_sort(SortKey::Chunk);
        let pos: Vec<Option<(i32, i32)>> = app.view.iter().map(|&i| app.findings()[i].pos).collect();
        assert_eq!(pos[0], Some((1, 0)));
        assert_eq!(pos[23], None);
    }

    #[test]
    fn keyboard_selection() {
        let tmp = TempDir::new("tui-select");
        let mut app = app(&tmp);
        press(&mut app, KeyCode::Char(' '));
        press(&mut app, KeyCode::Char(' '));
        assert_eq!((app.selected_count(), app.cursor), (2, 2));
        assert_eq!(app.targets(), [0, 1]);
        press(&mut app, KeyCode::Char('i'));
        assert_eq!(app.selected_count(), 22);
        press(&mut app, KeyCode::Char('n'));
        assert_eq!(app.selected_count(), 0);
        // Without a selection the current row is the target.
        assert_eq!(app.targets(), [app.view[2]]);
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(app.selected_count(), 19);
        assert!(app.targets().iter().all(|&i| app.findings()[i].severity() == Severity::Error));
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(app.selected_count(), 24);
        press(&mut app, KeyCode::End);
        assert_eq!(app.cursor, 23);
        press(&mut app, KeyCode::PageDown);
        assert_eq!(app.cursor, 23);
        press(&mut app, KeyCode::Home);
        press(&mut app, KeyCode::Up);
        assert_eq!(app.cursor, 0);
    }

    #[test]
    fn mouse_clicks() {
        let tmp = TempDir::new("tui-mouse");
        let mut app = app(&tmp);
        let lines = screen(&mut app, 140, 40);
        // A checkbox.
        let (x, y) = find(&lines, "[ ] #3 ");
        click(&mut app, x + 1, y, MouseButton::Left);
        assert_eq!((app.cursor, app.selected_count()), (2, 1));
        // A right click anywhere on a row.
        click(&mut app, 60, y + 1, MouseButton::Right);
        assert_eq!((app.cursor, app.selected_count()), (3, 2));
        // A column header.
        let (x, y) = find(&lines, "PROBLEM");
        click(&mut app, x, y, MouseButton::Left);
        assert_eq!((app.sort, app.sort_desc), (SortKey::Problem, false));
        click(&mut app, x, y, MouseButton::Left);
        assert!(app.sort_desc);
        // A button.
        let (x, y) = find(&lines, " n  None");
        click(&mut app, x + 1, y, MouseButton::Left);
        assert_eq!(app.selected_count(), 0);
        // The wheel.
        let before = app.cursor;
        app.on_mouse(MouseEvent { kind: MouseEventKind::ScrollDown, column: 5, row: 5, modifiers: KeyModifiers::NONE });
        assert_eq!(app.cursor, before + 3);
    }

    #[test]
    fn double_click_opens_details() {
        let tmp = TempDir::new("tui-details");
        let mut app = app(&tmp);
        let lines = screen(&mut app, 140, 40);
        let (x, y) = find(&lines, "wrong chunk position");
        click(&mut app, x, y, MouseButton::Left);
        assert!(matches!(app.mode, Mode::Browse));
        click(&mut app, x, y, MouseButton::Left);
        let Mode::Pager { title, lines, .. } = &app.mode else { panic!("no details") };
        assert!(title.starts_with("Finding #"));
        assert!(lines.iter().any(|l| l.starts_with("go to:  /execute in minecraft:overworld run tp @s")));
        let shown = screen(&mut app, 140, 40).join("\n");
        assert!(shown.contains("Esc  Close") && shown.contains("Fix: remove"));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Browse));
        // Help and coverage open and close too.
        for k in ['?', 'c'] {
            press(&mut app, KeyCode::Char(k));
            assert!(matches!(app.mode, Mode::Pager { .. }));
            screen(&mut app, 100, 30);
            press(&mut app, KeyCode::PageDown);
            press(&mut app, KeyCode::Char('q'));
            assert!(matches!(app.mode, Mode::Browse));
        }
    }

    #[test]
    fn preview_then_apply() {
        let tmp = TempDir::new("tui-apply");
        let mut app = app(&tmp);
        let region = tmp.path().join("region/r.0.0.mca");
        let before = std::fs::read(&region).unwrap();

        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(&app.mode, Mode::Working(job) if job.dry));
        screen(&mut app, 120, 30);
        // Keys do nothing while it works.
        press(&mut app, KeyCode::Char('q'));
        assert!(!app.quit);
        wait(&mut app);
        let Mode::Pager { title, lines, kind: PagerKind::DryRun { actionable: true, repair: false, targets }, .. } = &app.mode else {
            panic!("no preview")
        };
        assert!(title.contains("dry run"));
        assert_eq!(targets.len(), 19);
        assert!(lines[0].starts_with("DRY RUN: would remove 18 chunks and 1 file"), "{}", lines[0]);
        assert!(lines.iter().any(|l| l.starts_with("WOULD REMOVE chunk 9,0 in region/r.0.0.mca (with its entities data, its poi data)")));
        assert_eq!(std::fs::read(&region).unwrap(), before, "the preview wrote to the world");
        assert!(!tmp.path().join(fix::BACKUP_DIR).exists());
        assert!(screen(&mut app, 120, 30).join("\n").contains("Enter  Apply this to the world"));

        // Esc backs out without touching anything.
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Browse));
        assert_eq!(std::fs::read(&region).unwrap(), before);

        press(&mut app, KeyCode::Char('d'));
        wait(&mut app);
        press(&mut app, KeyCode::Enter);
        assert!(matches!(&app.mode, Mode::Working(job) if !job.dry));
        // Not even Ctrl-C interrupts the real thing.
        app.on_key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        assert!(!app.quit);
        wait(&mut app);
        let Mode::Pager { lines, kind: PagerKind::Done, .. } = &app.mode else { panic!("not applied") };
        assert!(lines[0].starts_with("Done: removed 18 chunks and 1 file"), "{}", lines[0]);
        assert!(lines[1].contains(fix::BACKUP_DIR));
        assert!(app.stale && app.selected_count() == 0);
        assert_eq!(app.state.iter().filter(|s| matches!(s, RowState::Done(_))).count(), 19);
        assert_ne!(std::fs::read(&region).unwrap(), before);
        let shown = screen(&mut app, 140, 40).join("\n");
        assert!(shown.contains("world changed: press r to rescan"));

        // Removed rows are not acted on again.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('e'));
        assert!(app.targets().is_empty());
        press(&mut app, KeyCode::Char('d'));
        assert_eq!(app.status, "Nothing to act on.");

        // Rescan: only warnings are left.
        press(&mut app, KeyCode::Char('r'));
        assert!(matches!(app.mode, Mode::Scanning(_)));
        screen(&mut app, 120, 30);
        wait(&mut app);
        assert_eq!(app.findings().len(), 3);
        assert!(!app.stale);
        assert!(screen(&mut app, 140, 40)[0].contains("0 errors, 3 warnings"));
    }

    #[test]
    fn repair_previews_then_repairs() {
        let tmp = TempDir::new("tui-repair");
        let mut app = app(&tmp);
        let region = tmp.path().join("region/r.0.0.mca");
        let before = std::fs::read(&region).unwrap();
        let errors = app.findings().iter().filter(|f| f.severity() == Severity::Error).count();

        press(&mut app, KeyCode::Char('e'));
        press(&mut app, KeyCode::Char('p'));
        assert!(matches!(&app.mode, Mode::Working(job) if job.dry && job.repair));
        assert!(screen(&mut app, 120, 30).join("\n").contains("what can be repaired"));
        wait(&mut app);
        let Mode::Pager { lines, kind: PagerKind::DryRun { actionable: true, repair: true, .. }, .. } = &app.mode else {
            panic!("no repair preview")
        };
        assert!(lines[0].starts_with("DRY RUN: would repair "), "{}", lines[0]);
        let would = lines.iter().filter(|l| l.starts_with("WOULD REPAIR chunk ")).count();
        let cannot = lines.iter().filter(|l| l.starts_with("CANNOT REPAIR ")).count();
        assert!(would > 0 && cannot > 0 && would + cannot == errors, "{would} {cannot} {errors}");
        assert!(lines.iter().any(|l| l.contains("can only be removed (d)")));
        assert_eq!(std::fs::read(&region).unwrap(), before, "the preview wrote to the world");
        assert!(screen(&mut app, 120, 30).join("\n").contains("Enter  Make these repairs"));

        press(&mut app, KeyCode::Enter);
        assert!(matches!(&app.mode, Mode::Working(job) if !job.dry && job.repair));
        wait(&mut app);
        let Mode::Pager { lines, kind: PagerKind::Done, .. } = &app.mode else { panic!("not repaired") };
        assert!(lines[0].starts_with("Done: repaired "), "{}", lines[0]);
        assert!(lines[1].contains(fix::BACKUP_DIR));
        assert!(app.stale);
        assert_eq!(app.state.iter().filter(|s| matches!(s, RowState::Done(what) if what == "repaired")).count(), would);
        assert_eq!(std::fs::read(&region).unwrap().len(), before.len());

        // What could not be repaired can still be removed.
        press(&mut app, KeyCode::Esc);
        press(&mut app, KeyCode::Char('e'));
        assert_eq!(app.targets().len(), cannot);
        press(&mut app, KeyCode::Char('d'));
        wait(&mut app);
        assert!(matches!(&app.mode, Mode::Pager { kind: PagerKind::DryRun { actionable: true, repair: false, .. }, .. }));
    }

    #[test]
    fn findings_without_a_fix() {
        let tmp = TempDir::new("tui-nofix");
        let mut app = app(&tmp);
        let row = app.view.iter().position(|&i| app.findings()[i].kind == Kind::Unverified).unwrap();
        app.cursor = row;
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(&app.mode, Mode::Pager { title, kind: PagerKind::Info, .. } if title == "Nothing to remove"));
    }

    #[test]
    fn scans_on_start_and_saves_a_report() {
        let tmp = TempDir::new("tui-scan");
        sample_world(tmp.path());
        let mut app = App::new(config(tmp.path()), None);
        assert!(matches!(app.mode, Mode::Scanning(_)));
        assert!(screen(&mut app, 100, 30).join("\n").contains("mcfsck - scanning"));
        wait(&mut app);
        assert_eq!(app.findings().len(), 24);
        assert!(app.status.starts_with("Found 19 errors and 5 warnings."), "{}", app.status);
        press(&mut app, KeyCode::Char('w'));
        assert!(app.status.starts_with("Report saved to"), "{}", app.status);
        assert_eq!(std::fs::read_to_string(tmp.path().join("report.jsonl")).unwrap().lines().count(), 25);

        // A world that cannot be scanned ends the session with the reason.
        let mut app = App::new(config(&tmp.path().join("missing")), None);
        wait(&mut app);
        assert!(app.quit && app.fatal.is_some());
        // Cancelling the first scan just leaves.
        let mut app = App::new(config(tmp.path()), None);
        press(&mut app, KeyCode::Esc);
        wait(&mut app);
        assert!(app.quit || app.findings().len() == 24);
        assert!(app.fatal.is_none());
    }

    #[test]
    fn clean_world_says_so() {
        let tmp = TempDir::new("tui-clean");
        let mut r = crate::testutil::Region::new();
        r.add(0, &crate::testutil::good_terrain(0, 0));
        r.write(&tmp.path().join("region/r.0.0.mca"));
        let mut app = App::new(config(tmp.path()), Some(scan(tmp.path(), false, 1)));
        let shown = screen(&mut app, 120, 30).join("\n");
        assert!(shown.contains("No problems found - every chunk that was checked can be read."));
        assert!(shown.contains("Full scan:"));
        for k in [' ', 'a', 'e', 'i', 'd', 's', 'S'] {
            press(&mut app, KeyCode::Char(k));
        }
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::End);
        assert!(matches!(app.mode, Mode::Browse));
        assert_eq!(app.status, "Nothing to act on.");
    }

    #[test]
    fn small_terminals_do_not_panic() {
        let tmp = TempDir::new("tui-small");
        let mut app = app(&tmp);
        let sizes = [(1, 1), (2, 2), (5, 3), (10, 5), (20, 8), (30, 10), (40, 12), (60, 19), (60, 20), (80, 24), (300, 100)];
        for (w, h) in sizes {
            screen(&mut app, w, h);
        }
        press(&mut app, KeyCode::Char('/'));
        for (w, h) in sizes {
            screen(&mut app, w, h);
        }
        press(&mut app, KeyCode::Enter);
        app.help();
        for (w, h) in sizes {
            screen(&mut app, w, h);
            press(&mut app, KeyCode::PageDown);
        }
        press(&mut app, KeyCode::Esc);
        app.details();
        for (w, h) in sizes {
            screen(&mut app, w, h);
        }
        press(&mut app, KeyCode::Esc);
        // The dry-run preview and the two progress popups.
        press(&mut app, KeyCode::Char('a'));
        press(&mut app, KeyCode::Char('d'));
        for (w, h) in sizes {
            screen(&mut app, w, h);
        }
        wait(&mut app);
        for (w, h) in sizes {
            screen(&mut app, w, h);
        }
        press(&mut app, KeyCode::Esc);
        app.start_scan();
        for (w, h) in sizes {
            screen(&mut app, w, h);
        }
        wait(&mut app);
        // Clicks outside anything are ignored.
        for (x, y) in [(0, 0), (500, 500), (3, 39)] {
            click(&mut app, x, y, MouseButton::Left);
            click(&mut app, x, y, MouseButton::Right);
        }
    }
}
