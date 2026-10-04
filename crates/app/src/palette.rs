//! Search everything (Ctrl+Shift+Space) and the command palette (Ctrl+Shift+P).
//!
//! One popup, filter chips across the top:
//!   All · Files · Commands · Tabs · History · Commits
//! Tab / Shift+Tab switches chips, ↑↓ moves, Enter opens, Esc closes.
//!
//! Sources:
//!   Commands  every action (with keys) and your custom shortcuts
//!   Tabs      every workspace and tab
//!   Files     the project of the focused tab (`git ls-files`, or a bounded
//!             folder walk outside git) → opens in your editor
//!   History   your shell history (zsh, bash or fish) → typed into the terminal
//!   Commits   `git log` of the project → opens it in the git panel
//!
//! Files, history and commits load in the background each time the popup
//! opens, so it appears instantly even in big repositories.

use std::cell::{Cell, RefCell};
use std::path::{Path, PathBuf};
use std::rc::{Rc, Weak};
use std::time::{Duration, Instant};

use gtk::prelude::*;
use gtk::{gdk, glib};

use crate::shortcuts::{self, BUILTINS};
use crate::state::{self, AppState, Shared};

const MAX_ROWS: usize = 60;
const MAX_RECENT: usize = 8;
const MAX_FILES: usize = 50_000;
const MAX_HISTORY: usize = 5_000;
const MAX_COMMITS: &str = "400";
const CACHE_FOR: Duration = Duration::from_secs(30);

/// What a result is (and which chip shows it).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cat {
    Command,
    Tab,
    File,
    History,
    Commit,
}

const CHIPS: &[(Option<Cat>, &str)] = &[
    (None, "All"),
    (Some(Cat::File), "Files"),
    (Some(Cat::Command), "Commands"),
    (Some(Cat::Tab), "Tabs"),
    (Some(Cat::History), "History"),
    (Some(Cat::Commit), "Commits"),
];

impl Cat {
    fn tag(self) -> &'static str {
        match self {
            Cat::Command => "Command",
            Cat::Tab => "Tab",
            Cat::File => "File",
            Cat::History => "History",
            Cat::Commit => "Commit",
        }
    }
}

fn placeholder(mode: Option<Cat>) -> &'static str {
    match mode {
        None => "Search files, commands, tabs, history and commits…",
        Some(Cat::File) => "Find a file in this project…",
        Some(Cat::Command) => "Type a command…",
        Some(Cat::Tab) => "Go to a workspace or tab…",
        Some(Cat::History) => "Search your shell history…",
        Some(Cat::Commit) => "Search commits by message, author or hash…",
    }
}

type Run = Rc<dyn Fn(&Shared)>;

/// Commands, workspaces and tabs: small lists rebuilt on every open.
struct Item {
    id: String,
    cat: Cat,
    title: String,
    subtitle: String,
    icon: &'static str,
    keys: Vec<String>,
    run: Run,
}

struct CommitRow {
    hash: String,
    short: String,
    subject: String,
    author: String,
    time: i64,
}

/// A row on screen, pointing into one of the sources.
#[derive(Clone, Copy)]
enum Entry {
    Item(usize),
    File(usize),
    History(usize),
    Commit(usize),
}

#[derive(Default)]
struct Files {
    root: Option<PathBuf>,
    list: Vec<String>,
    loaded: Option<Instant>,
}

pub struct Palette {
    pub root: gtk::Box,
    entry: gtk::SearchEntry,
    list: gtk::ListBox,
    scroll: gtk::ScrolledWindow,
    hint: gtk::Label,
    chips: Vec<(Option<Cat>, gtk::ToggleButton)>,
    mode: Cell<Option<Cat>>,
    items: RefCell<Vec<Item>>,
    files: RefCell<Files>,
    history: RefCell<(Vec<String>, Option<Instant>)>,
    commits: RefCell<(Option<String>, Vec<CommitRow>)>,
    shown: RefCell<Vec<Entry>>,
    recent: RefCell<Vec<String>>,
    shared: RefCell<Weak<RefCell<AppState>>>,
    /// Bumped on every open, so late background results for an old open are ignored.
    generation: Cell<u64>,
}

pub fn build() -> Rc<Palette> {
    let entry = gtk::SearchEntry::builder().hexpand(true).build();
    entry.add_css_class("palette-entry");
    let chip_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    chip_box.add_css_class("palette-chips");
    let mut chips = Vec::new();
    for (mode, label) in CHIPS {
        let chip = gtk::ToggleButton::with_label(label);
        chip.add_css_class("chip");
        if let Some((_, first)) = chips.first() {
            chip.set_group(Some(first));
        }
        chip_box.append(&chip);
        chips.push((*mode, chip));
    }
    let list = gtk::ListBox::new();
    list.add_css_class("palette-list");
    list.set_selection_mode(gtk::SelectionMode::Browse);
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(440)
        .child(&list)
        .build();
    // Footer: key hints on the left, what's searchable on the right.
    let keys_hint = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    for (key, what) in [("↑↓", "Navigate"), ("↵", "Open"), ("⇥", "Filter"), ("esc", "Close")] {
        let k = gtk::Label::new(Some(key));
        k.add_css_class("hint-key");
        let w = gtk::Label::new(Some(what));
        w.add_css_class("hint-text");
        let pair = gtk::Box::new(gtk::Orientation::Horizontal, 5);
        pair.append(&k);
        pair.append(&w);
        keys_hint.append(&pair);
    }
    let hint = gtk::Label::builder().xalign(1.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Start).build();
    hint.add_css_class("hint-text");
    let footer = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    footer.add_css_class("palette-footer");
    footer.append(&keys_hint);
    footer.append(&hint);

    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.add_css_class("palette");
    root.set_halign(gtk::Align::Center);
    root.set_valign(gtk::Align::Start);
    root.set_width_request(700);
    root.set_visible(false);
    root.append(&entry);
    root.append(&chip_box);
    root.append(&scroll);
    root.append(&footer);

    let palette = Rc::new(Palette {
        root,
        entry: entry.clone(),
        list: list.clone(),
        scroll,
        hint,
        chips,
        mode: Cell::new(None),
        items: RefCell::default(),
        files: RefCell::default(),
        history: RefCell::default(),
        commits: RefCell::default(),
        shown: RefCell::default(),
        recent: RefCell::default(),
        shared: RefCell::new(Weak::new()),
        generation: Cell::new(0),
    });

    for (mode, chip) in &palette.chips {
        let (p, mode) = (Rc::downgrade(&palette), *mode);
        chip.connect_toggled(move |c| {
            if let (true, Some(p)) = (c.is_active(), p.upgrade()) {
                if p.mode.get() != mode {
                    p.mode.set(mode);
                    p.entry.set_placeholder_text(Some(placeholder(mode)));
                    p.render();
                }
                p.entry.grab_focus();
            }
        });
    }
    entry.connect_search_changed({
        let p = Rc::downgrade(&palette);
        move |_| {
            if let Some(p) = p.upgrade() {
                p.render();
            }
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed({
        let p = Rc::downgrade(&palette);
        move |_, key, _, mods| {
            let Some(p) = p.upgrade() else { return glib::Propagation::Proceed };
            match key {
                gdk::Key::Escape => p.hide(),
                gdk::Key::Down => p.move_selection(1),
                gdk::Key::Up => p.move_selection(-1),
                gdk::Key::Tab => p.cycle_mode(1),
                gdk::Key::ISO_Left_Tab => p.cycle_mode(-1),
                gdk::Key::Return | gdk::Key::KP_Enter => {
                    let index = p.list.selected_row().map(|r| r.index()).unwrap_or(0);
                    p.run(index.max(0) as usize);
                }
                _ => return glib::Propagation::Proceed,
            }
            let _ = mods;
            glib::Propagation::Stop
        }
    });
    entry.add_controller(keys);
    list.connect_row_activated({
        let p = Rc::downgrade(&palette);
        move |_, row| {
            if let Some(p) = p.upgrade() {
                p.run(row.index() as usize);
            }
        }
    });
    palette
}

impl Palette {
    /// Open with a chip preselected (`None` = All). Pressing the same shortcut again closes it.
    pub fn open(self: &Rc<Self>, shared: &Shared, mode: Option<Cat>) {
        if self.root.is_visible() && self.mode.get() == mode {
            self.hide();
            return;
        }
        *self.shared.borrow_mut() = Rc::downgrade(shared);
        *self.items.borrow_mut() = collect(shared);
        self.set_mode(mode);
        self.entry.set_text("");
        self.root.set_visible(true);
        self.entry.grab_focus();
        self.render();
        self.load_sources(shared);
    }

    /// Open (never toggle) with a filter and a pre-filled query.
    pub fn open_with(self: &Rc<Self>, shared: &Shared, mode: Option<Cat>, query: &str) {
        if !self.root.is_visible() || self.mode.get() != mode {
            self.open(shared, mode);
        }
        self.entry.set_text(query);
        self.entry.set_position(-1);
    }

    fn set_mode(&self, mode: Option<Cat>) {
        self.mode.set(mode);
        self.entry.set_placeholder_text(Some(placeholder(mode)));
        if let Some((_, chip)) = self.chips.iter().find(|(m, _)| *m == mode) {
            chip.set_active(true);
        }
    }

    fn cycle_mode(&self, delta: i32) {
        let i = CHIPS.iter().position(|(m, _)| *m == self.mode.get()).unwrap_or(0) as i32;
        let next = (i + delta).rem_euclid(CHIPS.len() as i32) as usize;
        self.set_mode(CHIPS[next].0);
        self.render();
    }

    fn hide(&self) {
        self.root.set_visible(false);
        if let Some(shared) = self.shared.borrow().upgrade() {
            let focused = shared.borrow().focused_surface();
            if let Some(sid) = focused {
                state::focus_surface(&shared, sid);
            }
        }
    }

    // ---------------------------------------------------------- sources

    /// Start loading files, history and commits for the focused tab's folder.
    fn load_sources(self: &Rc<Self>, shared: &Shared) {
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let dir = {
            let st = shared.borrow();
            st.focused_surface()
                .and_then(|sid| st.cwd_of(sid))
                .or_else(|| st.active.and_then(|a| st.ws(a)).map(|w| w.cwd.clone()))
                .unwrap_or_else(|| std::env::var("HOME").unwrap_or_default())
        };

        // Shell history: a quick local read, cached briefly.
        let stale = self.history.borrow().1.is_none_or(|t| t.elapsed() > CACHE_FOR);
        if stale {
            *self.history.borrow_mut() = (read_history(), Some(Instant::now()));
        }

        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let top = crate::git::run(&dir, &["git", "rev-parse", "--show-toplevel"]).await.map(|s| s.trim().to_string());
            let root = PathBuf::from(top.clone().unwrap_or_else(|| dir.clone()));

            // Files (cached per folder for a short while).
            let fresh = weak.upgrade().is_some_and(|p| {
                let f = p.files.borrow();
                f.root.as_deref() == Some(root.as_path()) && f.loaded.is_some_and(|t| t.elapsed() < CACHE_FOR)
            });
            if !fresh {
                let list = match &top {
                    Some(top) => crate::git::run(top, &["git", "ls-files", "--cached", "--others", "--exclude-standard"])
                        .await
                        .map(|out| out.lines().take(MAX_FILES).map(String::from).collect())
                        .unwrap_or_default(),
                    None => walk_in_background(root.clone()).await,
                };
                let Some(p) = weak.upgrade() else { return };
                *p.files.borrow_mut() = Files { root: Some(root.clone()), list, loaded: Some(Instant::now()) };
                if p.generation.get() == generation && p.root.is_visible() {
                    p.render();
                }
            }

            // Commits.
            if let Some(top) = &top {
                let format = "--pretty=format:%H%x1f%h%x1f%s%x1f%an%x1f%at";
                let log = crate::git::run(top, &["git", "log", "--max-count", MAX_COMMITS, format]).await.unwrap_or_default();
                let rows = log
                    .lines()
                    .filter_map(|l| {
                        let f: Vec<&str> = l.split('\u{1f}').collect();
                        (f.len() == 5).then(|| CommitRow {
                            hash: f[0].into(),
                            short: f[1].into(),
                            subject: f[2].into(),
                            author: f[3].into(),
                            time: f[4].parse().unwrap_or(0),
                        })
                    })
                    .collect();
                let Some(p) = weak.upgrade() else { return };
                *p.commits.borrow_mut() = (Some(top.clone()), rows);
            } else if let Some(p) = weak.upgrade() {
                *p.commits.borrow_mut() = (None, Vec::new());
            }
            if let Some(p) = weak.upgrade() {
                if p.generation.get() == generation && p.root.is_visible() {
                    p.render();
                }
            }
        });
    }

    // ---------------------------------------------------------- results

    fn render(&self) {
        while let Some(row) = self.list.first_child() {
            self.list.remove(&row);
        }
        let query = self.entry.text().trim().to_lowercase();
        let mode = self.mode.get();
        let wants = |c: Cat| mode.is_none() || mode == Some(c);
        let items = self.items.borrow();
        let files = self.files.borrow();
        let history = self.history.borrow();
        let commits = self.commits.borrow();
        let recent = self.recent.borrow();
        let mut scored: Vec<(i32, Entry)> = Vec::new();

        // Commands, workspaces, tabs.
        for (i, item) in items.iter().enumerate() {
            if !wants(item.cat) {
                continue;
            }
            if query.is_empty() {
                let r = recent.iter().position(|id| *id == item.id).map(|p| 1000 - p as i32).unwrap_or(0);
                scored.push((r, Entry::Item(i)));
            } else {
                let s = fuzzy(&query, &item.title.to_lowercase()).map(|s| s + 20).max(fuzzy(&query, &item.subtitle.to_lowercase()));
                if let Some(s) = s {
                    scored.push((s + 25, Entry::Item(i)));
                }
            }
        }
        // Files: only on request (a typed query, or the Files chip).
        if wants(Cat::File) && (!query.is_empty() || mode == Some(Cat::File)) {
            for (i, path) in files.list.iter().enumerate() {
                if query.is_empty() {
                    scored.push((-(i as i32), Entry::File(i)));
                    if i > MAX_ROWS {
                        break;
                    }
                    continue;
                }
                let lower = path.to_lowercase();
                let base = lower.rsplit('/').next().unwrap_or(&lower);
                if let Some(s) = fuzzy(&query, base).map(|s| s + 30).max(fuzzy(&query, &lower)) {
                    scored.push((s, Entry::File(i)));
                }
            }
        }
        if wants(Cat::History) && (!query.is_empty() || mode == Some(Cat::History)) {
            for (i, cmd) in history.0.iter().enumerate() {
                if query.is_empty() {
                    scored.push((-(i as i32), Entry::History(i)));
                    if i > MAX_ROWS {
                        break;
                    }
                } else if let Some(s) = fuzzy(&query, &cmd.to_lowercase()) {
                    // Prefer newer commands when scores tie.
                    scored.push((s - 5 - (i as i32 / 500), Entry::History(i)));
                }
            }
        }
        if wants(Cat::Commit) && (!query.is_empty() || mode == Some(Cat::Commit)) {
            for (i, c) in commits.1.iter().enumerate() {
                if query.is_empty() {
                    scored.push((-(i as i32), Entry::Commit(i)));
                } else if c.hash.starts_with(&query) || c.short.starts_with(&query) {
                    scored.push((300, Entry::Commit(i)));
                } else if let Some(s) = fuzzy(&query, &c.subject.to_lowercase()).max(fuzzy(&query, &c.author.to_lowercase())) {
                    scored.push((s - 5, Entry::Commit(i)));
                }
            }
        }

        scored.sort_by(|a, b| b.0.cmp(&a.0));
        let shown: Vec<Entry> = scored.into_iter().take(MAX_ROWS).map(|(_, e)| e).collect();
        let now = glib::DateTime::now_utc().map(|d| d.to_unix()).unwrap_or(0);
        for e in &shown {
            let row = match *e {
                Entry::Item(i) => {
                    let it = &items[i];
                    let tag = if query.is_empty() && recent.contains(&it.id) { "Recent" } else { it.cat.tag() };
                    row(it.icon, &it.title, &it.subtitle, tag, &it.keys, false)
                }
                Entry::File(i) => {
                    let path = &files.list[i];
                    let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
                    row("cmuxp-file-symbolic", name, if dir.is_empty() { "." } else { dir }, "File", &[], false)
                }
                Entry::History(i) => row("cmuxp-clock-symbolic", &history.0[i], "", "History", &[], true),
                Entry::Commit(i) => {
                    let c = &commits.1[i];
                    let sub = format!("{} · {} · {}", c.short, c.author, crate::chrome::ago((now - c.time).max(0) as u64));
                    row("cmuxp-commit-symbolic", &c.subject, &sub, "Commit", &[], false)
                }
            };
            self.list.append(&row);
        }
        if shown.is_empty() {
            let text = if query.is_empty() { "Nothing here yet" } else { "No matches" };
            let none = gtk::Label::new(Some(text));
            none.add_css_class("palette-empty");
            self.list.append(&gtk::ListBoxRow::builder().child(&none).activatable(false).selectable(false).build());
        }
        *self.shown.borrow_mut() = shown;
        if let Some(first) = self.list.row_at_index(0) {
            self.list.select_row(Some(&first));
        }
        self.scroll.vadjustment().set_value(0.0);

        let project = files.root.as_ref().and_then(|r| r.file_name()).map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        self.hint.set_text(&format!(
            "{}{} files · {} commands in history · {} commits",
            if project.is_empty() { String::new() } else { format!("{project} · ") },
            files.list.len(),
            history.0.len(),
            commits.1.len()
        ));
    }

    fn move_selection(&self, delta: i32) {
        let count = self.shown.borrow().len() as i32;
        if count == 0 {
            return;
        }
        let current = self.list.selected_row().map(|r| r.index()).unwrap_or(0);
        let next = (current + delta).rem_euclid(count);
        if let Some(row) = self.list.row_at_index(next) {
            self.list.select_row(Some(&row));
            // Keep typing in the entry; just scroll the row into view.
            if let Some(bounds) = row.compute_bounds(&self.list) {
                let adj = self.scroll.vadjustment();
                let (top, bottom) = (bounds.y() as f64, (bounds.y() + bounds.height()) as f64);
                if top < adj.value() {
                    adj.set_value(top);
                } else if bottom > adj.value() + adj.page_size() {
                    adj.set_value(bottom - adj.page_size());
                }
            }
        }
    }

    fn run(&self, index: usize) {
        let Some(entry) = self.shown.borrow().get(index).copied() else { return };
        let weak = self.shared.borrow().clone();
        let action: Run = match entry {
            Entry::Item(i) => {
                let items = self.items.borrow();
                let id = items[i].id.clone();
                let mut recent = self.recent.borrow_mut();
                recent.retain(|r| r != &id);
                recent.insert(0, id);
                recent.truncate(MAX_RECENT);
                items[i].run.clone()
            }
            Entry::File(i) => {
                let files = self.files.borrow();
                let path = files.root.clone().unwrap_or_default().join(&files.list[i]);
                Rc::new(move |s: &Shared| {
                    let sid = s.borrow().focused_surface();
                    match sid {
                        Some(sid) => crate::links::open_in_editor(s, sid, &path, None, None),
                        None => eprintln!("cmuxp: no tab to open {} from", path.display()),
                    }
                })
            }
            Entry::History(i) => {
                let cmd = self.history.borrow().0[i].clone();
                Rc::new(move |s: &Shared| {
                    if let Err(e) = state::send_text(s, None, &cmd) {
                        eprintln!("cmuxp: {e}");
                    }
                })
            }
            Entry::Commit(i) => {
                let commits = self.commits.borrow();
                let (dir, hash) = (commits.0.clone().unwrap_or_default(), commits.1[i].hash.clone());
                Rc::new(move |s: &Shared| state::show_commit(s, &dir, &hash))
            }
        };
        self.hide();
        // Run after the popup is gone so focus lands where the command puts it.
        state::defer(&weak, move |s| action(s));
    }
}

/// A result row: [tinted icon tile]  title / subtitle      [keys]  Type
fn row(icon: &str, title: &str, subtitle: &str, kind: &str, keys: &[String], mono: bool) -> gtk::ListBoxRow {
    // A plain monochrome icon (gray, white when selected). No coloured tiles.
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    image.add_css_class("palette-icon");
    image.set_size_request(20, -1);
    image.set_valign(gtk::Align::Center);

    let t = gtk::Label::builder().label(title).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    t.add_css_class("palette-title");
    if mono {
        t.add_css_class("mono");
    }
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    text.append(&t);
    if !subtitle.is_empty() {
        let s = gtk::Label::builder().label(subtitle).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Start).build();
        s.add_css_class("palette-subtitle");
        text.append(&s);
    }
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    line.append(&image);
    line.append(&text);
    if !keys.is_empty() {
        line.append(&crate::shortcuts_ui::keycaps_widget(&keys[..1]));
    }
    // The type, as quiet text (no coloured pill competing with the result).
    let k = gtk::Label::builder().label(kind).xalign(1.0).width_chars(8).build();
    k.add_css_class("palette-kind");
    k.set_valign(gtk::Align::Center);
    line.append(&k);
    let r = gtk::ListBoxRow::builder().child(&line).build();
    r.add_css_class("palette-row");
    r
}

/// Fuzzy subsequence score: higher is better, None = no match.
/// Consecutive letters and letters at the start of words score extra.
pub fn fuzzy(query: &str, text: &str) -> Option<i32> {
    if query.is_empty() {
        return Some(0);
    }
    if let Some(pos) = text.find(query) {
        // Whole query as a substring: best, and earlier is better.
        let at_word = pos == 0 || !text.as_bytes()[pos - 1].is_ascii_alphanumeric();
        return Some(200 - (pos.min(150)) as i32 + if at_word { 50 } else { 0 });
    }
    let chars: Vec<char> = text.chars().collect();
    let mut score = 0;
    let mut ti = 0;
    let mut last: Option<usize> = None;
    for q in query.chars() {
        if q == ' ' {
            continue;
        }
        let found = (ti..chars.len()).find(|&i| chars[i] == q)?;
        let word_start = found == 0 || !chars[found - 1].is_alphanumeric();
        score += 10;
        if last.is_some_and(|l| l + 1 == found) {
            score += 15;
        }
        if word_start {
            score += 20;
        }
        score -= (found - ti).min(20) as i32;
        last = Some(found);
        ti = found + 1;
    }
    Some(score)
}

// --------------------------------------------------------- shell history

/// Newest-first, de-duplicated commands from the user's shell history file.
pub fn read_history() -> Vec<String> {
    let home = PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let shell = std::env::var("SHELL").unwrap_or_default();
    let shell = shell.rsplit('/').next().unwrap_or("");
    let mut candidates: Vec<PathBuf> = std::env::var_os("HISTFILE").map(PathBuf::from).into_iter().collect();
    match shell {
        "zsh" => candidates.push(home.join(".zsh_history")),
        "fish" => candidates.push(home.join(".local/share/fish/fish_history")),
        _ => {}
    }
    candidates.extend([home.join(".bash_history"), home.join(".zsh_history"), home.join(".local/share/fish/fish_history")]);
    let Some(path) = candidates.into_iter().find(|p| p.is_file()) else { return Vec::new() };
    let Ok(bytes) = std::fs::read(&path) else { return Vec::new() };
    parse_history(&String::from_utf8_lossy(&bytes))
}

pub fn parse_history(text: &str) -> Vec<String> {
    let mut commands: Vec<&str> = Vec::new();
    for line in text.lines() {
        let cmd = if let Some(rest) = line.strip_prefix("- cmd: ") {
            rest // fish
        } else if line.starts_with(": ") && line.contains(';') {
            line.split_once(';').map(|x| x.1).unwrap_or("") // zsh extended history
        } else if line.starts_with('#') && line[1..].chars().all(|c| c.is_ascii_digit()) {
            continue; // bash timestamps
        } else if line.starts_with("  when: ") || line.starts_with("  paths:") || line.starts_with("    - ") {
            continue; // fish metadata
        } else {
            line
        };
        let cmd = cmd.trim();
        if !cmd.is_empty() {
            commands.push(cmd);
        }
    }
    let mut seen = std::collections::HashSet::new();
    commands.iter().rev().filter(|c| seen.insert(**c)).take(MAX_HISTORY).map(|c| c.to_string()).collect()
}

// ------------------------------------------------------------ folder walk

/// List files under `root` on a background thread (used outside git repos).
async fn walk_in_background(root: PathBuf) -> Vec<String> {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let _ = tx.send_blocking(walk(&root));
    });
    rx.recv().await.unwrap_or_default()
}

fn walk(root: &Path) -> Vec<String> {
    const SKIP: &[&str] = &["node_modules", "target", "dist", "build", "__pycache__", "venv", ".venv"];
    let mut out = Vec::new();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name.starts_with('.') || SKIP.contains(&name.as_str()) {
                continue;
            }
            let path = e.path();
            let Ok(kind) = e.file_type() else { continue };
            if kind.is_dir() && depth < 6 {
                stack.push((path, depth + 1));
            } else if kind.is_file() {
                if let Ok(rel) = path.strip_prefix(root) {
                    out.push(rel.to_string_lossy().to_string());
                }
                if out.len() >= 20_000 {
                    return out;
                }
            }
        }
    }
    out.sort_by_key(|p| p.matches('/').count());
    out
}

// ---------------------------------------------------- commands and tabs

fn collect(shared: &Shared) -> Vec<Item> {
    let st = shared.borrow();
    let mut items = Vec::new();
    let keymap = st.keymap.clone();

    for b in BUILTINS {
        if matches!(b.action, "command-palette" | "search") || st.app.lookup_action(b.action).is_none() {
            continue;
        }
        let action = b.action;
        items.push(Item {
            id: format!("action:{action}"),
            cat: Cat::Command,
            title: b.label.to_string(),
            subtitle: b.description.to_string(),
            icon: b.icon,
            keys: keymap.as_ref().map(|k| k.keys_for(action)).unwrap_or_default(),
            run: Rc::new(move |s: &Shared| {
                let app = s.borrow().app.clone();
                app.activate_action(action, None);
            }),
        });
    }
    items.push(Item {
        id: "new-shortcut".into(),
        cat: Cat::Command,
        title: "New custom shortcut…".into(),
        subtitle: "Bind a key to a command, text, URL or folder".into(),
        icon: "cmuxp-keyboard-symbolic",
        keys: Vec::new(),
        run: Rc::new(|s: &Shared| {
            let (window, keymap) = {
                let st = s.borrow();
                (st.window.clone(), st.keymap.clone())
            };
            if let Some(k) = keymap {
                crate::shortcuts_ui::open_at(window.upcast_ref(), &k, crate::shortcuts_ui::Target { new: true, ..Default::default() });
            }
        }),
    });
    if let Some(k) = &keymap {
        for c in k.config.borrow().custom.iter() {
            let custom = c.clone();
            items.push(Item {
                id: format!("custom:{}", c.name),
                cat: Cat::Command,
                title: c.name.clone(),
                subtitle: c.action.describe(&c.value),
                icon: c.action.icon(),
                keys: c.keys.clone(),
                run: Rc::new(move |s: &Shared| shortcuts::run_custom(s, &custom)),
            });
        }
    }
    for w in &st.workspaces {
        let id = w.id;
        items.push(Item {
            id: format!("ws:{id}"),
            cat: Cat::Tab,
            title: format!("Workspace: {}", w.title),
            subtitle: w.cwd.clone(),
            icon: "cmuxp-folder-symbolic",
            keys: Vec::new(),
            run: Rc::new(move |s: &Shared| {
                state::select_workspace(s, id);
            }),
        });
        for pane in st.panes.iter().filter(|(_, p)| p.workspace == w.id).map(|(id, _)| *id) {
            for sid in st.tabs_in(pane) {
                let Some(surface) = st.surfaces.get(&sid) else { continue };
                items.push(Item {
                    id: format!("tab:{sid}"),
                    cat: Cat::Tab,
                    title: surface.title.clone(),
                    subtitle: format!("Tab in {}", w.title),
                    icon: surface.icon_name(),
                    keys: Vec::new(),
                    run: Rc::new(move |s: &Shared| {
                        state::select_workspace(s, id);
                        state::focus_surface(s, sid);
                    }),
                });
            }
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_ranks_sensibly() {
        assert!(fuzzy("spr", "split right").is_some());
        assert!(fuzzy("xyz", "split right").is_none());
        assert!(fuzzy("sr", "split right") > fuzzy("sr", "first"));
        assert!(fuzzy("split", "split down") > fuzzy("sd", "split down"));
    }

    #[test]
    fn history_formats() {
        let zsh = ": 1700000000:0;git status\n: 1700000001:0;cargo build\n: 1700000002:0;git status\n";
        assert_eq!(parse_history(zsh), vec!["git status", "cargo build"]);
        let bash = "#1700000000\nls -la\n#1700000001\nmake\n";
        assert_eq!(parse_history(bash), vec!["make", "ls -la"]);
        let fish = "- cmd: npm test\n  when: 1700000000\n- cmd: git push\n  when: 1700000001\n";
        assert_eq!(parse_history(fish), vec!["git push", "npm test"]);
    }
}
