//! Git history panel: a sidebar on the right of a workspace (Ctrl+Shift+G, or
//! the git icon in any pane's tab bar).
//!
//!   ┌──────────────────────────────────┐
//!   │ [⑂] gitmind                 ⟳  × │
//!   │     ⑂ master   ↑1                │
//!   │ [ Filter commits…              ] │
//!   │ CHANGES 3                      ⌄ │
//!   │  M  main.rs        src/ui        │  click → diff in a new tab
//!   │ HISTORY 128                      │
//!   │  ●  Fix parser                   │  click → message, files with a
//!   │  │  (N) nullxbit · 2h · a1b2c3d  │          +/− bar, diff/copy/web
//!   └──────────────────────────────────┘
//!
//! All git commands run asynchronously (see git.rs); the panel refreshes when
//! opened, on ⟳, and every few seconds while it's visible.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use gtk::glib;
use gtk::prelude::*;

use crate::git;
use crate::state::{self, AppState, Content, Shared};

const PAGE: usize = 200;
const CHANGES_PREVIEW: usize = 5;
const FIELD: char = '\u{1f}';
const RECORD: char = '\u{1e}';

#[derive(Debug, Clone, PartialEq)]
pub struct Commit {
    pub hash: String,
    pub short: String,
    pub author: String,
    pub time: i64,
    pub refs: Vec<String>,
    pub subject: String,
}

/// Parse `git log --pretty=format:%H␟%h␟%an␟%at␟%D␟%s␞`.
pub fn parse_log(out: &str) -> Vec<Commit> {
    out.split(RECORD)
        .filter_map(|rec| {
            let rec = rec.trim_start_matches('\n');
            let f: Vec<&str> = rec.splitn(6, FIELD).collect();
            if f.len() < 6 || f[0].is_empty() {
                return None;
            }
            Some(Commit {
                hash: f[0].to_string(),
                short: f[1].to_string(),
                author: f[2].to_string(),
                time: f[3].parse().unwrap_or(0),
                refs: f[4].split(", ").filter(|r| !r.is_empty()).map(String::from).collect(),
                subject: f[5].to_string(),
            })
        })
        .collect()
}

/// Lines of `git status --porcelain`: [("M", "src/a.rs"), ("?", "new.txt")].
pub fn parse_changes(out: &str) -> Vec<(String, String)> {
    out.lines()
        .filter(|l| l.len() > 3)
        .map(|l| {
            let code = l[..2].trim().to_string();
            let path = l[3..].rsplit(" -> ").next().unwrap_or(&l[3..]).to_string();
            (if code == "??" { "?".to_string() } else { code }, path)
        })
        .collect()
}

pub struct GitPanel {
    pub root: gtk::Revealer,
    shared: Weak<RefCell<AppState>>,
    repo_label: gtk::Label,
    branch_name: gtk::Label,
    branch_counts: gtk::Label,
    branch_row: gtk::Box,
    filter: gtk::SearchEntry,
    content: gtk::Box,
    scroll: gtk::ScrolledWindow,
    repo: RefCell<Option<String>>,
    branch: RefCell<String>,
    commits: RefCell<Vec<Commit>>,
    changes: RefCell<Vec<(String, String)>>,
    expanded: RefCell<Option<String>>,
    changes_open: Cell<bool>,
    changes_all: Cell<bool>,
    limit: Cell<usize>,
    busy: Cell<bool>,
    /// Raw output of the last refresh, to skip redrawing when nothing changed.
    signature: RefCell<String>,
    remote_web: RefCell<Option<String>>,
}

pub fn build(shared: &Shared) -> Rc<GitPanel> {
    let tile = gtk::Image::from_icon_name("cmuxp-git-symbolic");
    tile.set_pixel_size(16);
    let tile_box = gtk::CenterBox::new();
    tile_box.add_css_class("gp-tile");
    tile_box.set_size_request(30, 30);
    tile_box.set_valign(gtk::Align::Center);
    tile_box.set_center_widget(Some(&tile));

    let repo_label = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    repo_label.add_css_class("gp-repo");
    let branch_icon = gtk::Image::from_icon_name("cmuxp-branch-symbolic");
    branch_icon.set_pixel_size(12);
    let branch_name = gtk::Label::builder().xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    let branch_pill = gtk::Box::new(gtk::Orientation::Horizontal, 5);
    branch_pill.add_css_class("gp-branch");
    branch_pill.append(&branch_icon);
    branch_pill.append(&branch_name);
    let branch_counts = gtk::Label::new(None);
    branch_counts.add_css_class("gp-counts");
    let branch_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    branch_row.append(&branch_pill);
    branch_row.append(&branch_counts);

    let titles = gtk::Box::new(gtk::Orientation::Vertical, 4);
    titles.set_hexpand(true);
    titles.append(&repo_label);
    titles.append(&branch_row);

    let refresh = crate::panes::icon_button("cmuxp-reload-symbolic", "Refresh");
    let close = crate::panes::icon_button("cmuxp-close-symbolic", "Close (Ctrl+Shift+G)");
    refresh.set_valign(gtk::Align::Start);
    close.set_valign(gtk::Align::Start);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    top.add_css_class("gp-top");
    top.append(&tile_box);
    top.append(&titles);
    top.append(&refresh);
    top.append(&close);

    let filter = gtk::SearchEntry::builder().placeholder_text("Filter by message, author or hash").build();
    filter.add_css_class("gp-filter");

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.add_css_class("gp-content");
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&content).build();

    let column = gtk::Box::new(gtk::Orientation::Vertical, 10);
    column.add_css_class("git-panel");
    column.set_width_request(400);
    // Children ask to expand; stop that from spreading to the panel itself,
    // which would make it take half the window.
    column.set_hexpand(false);
    column.append(&top);
    column.append(&filter);
    column.append(&scroll);

    let root = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideLeft)
        .transition_duration(160)
        .child(&column)
        .reveal_child(false)
        .hexpand(false)
        .build();

    let panel = Rc::new(GitPanel {
        root,
        shared: Rc::downgrade(shared),
        repo_label,
        branch_name,
        branch_counts,
        branch_row,
        filter: filter.clone(),
        content,
        scroll,
        repo: RefCell::default(),
        branch: RefCell::default(),
        commits: RefCell::default(),
        changes: RefCell::default(),
        expanded: RefCell::default(),
        changes_open: Cell::new(true),
        changes_all: Cell::new(false),
        limit: Cell::new(PAGE),
        busy: Cell::new(false),
        signature: RefCell::default(),
        remote_web: RefCell::default(),
    });

    refresh.connect_clicked({
        let p = Rc::downgrade(&panel);
        move |_| {
            if let Some(p) = p.upgrade() {
                p.signature.borrow_mut().clear();
                p.refresh();
            }
        }
    });
    close.connect_clicked({
        let p = Rc::downgrade(&panel);
        move |_| {
            if let Some(p) = p.upgrade() {
                p.hide();
            }
        }
    });
    filter.connect_search_changed({
        let p = Rc::downgrade(&panel);
        move |_| {
            if let Some(p) = p.upgrade() {
                p.render();
            }
        }
    });
    let keys = gtk::EventControllerKey::new();
    keys.connect_key_pressed({
        let p = Rc::downgrade(&panel);
        move |_, key, _, _| {
            if key == gtk::gdk::Key::Escape {
                if let Some(p) = p.upgrade() {
                    p.hide();
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    });
    filter.add_controller(keys);
    panel
}

impl GitPanel {
    pub fn is_open(&self) -> bool {
        self.root.reveals_child()
    }

    /// Show the history of the repository containing `dir`.
    pub fn show(self: &Rc<Self>, dir: &str) {
        if self.repo.borrow().as_deref() != Some(dir) {
            *self.repo.borrow_mut() = Some(dir.to_string());
            self.signature.borrow_mut().clear();
            self.expanded.borrow_mut().take();
            self.limit.set(PAGE);
        }
        self.root.set_reveal_child(true);
        self.refresh();
    }

    pub fn hide(&self) {
        self.root.set_reveal_child(false);
        if let Some(shared) = self.shared.upgrade() {
            let focused = shared.borrow().focused_surface();
            if let Some(sid) = focused {
                state::focus_surface(&shared, sid);
            }
        }
    }

    /// Expand a commit (used by the search popup).
    pub fn reveal_commit(self: &Rc<Self>, hash: &str) {
        *self.expanded.borrow_mut() = Some(hash.to_string());
        self.filter.set_text("");
        self.render();
    }

    /// Re-read status and log in the background; redraw if anything changed.
    pub fn refresh(self: &Rc<Self>) {
        let Some(dir) = self.repo.borrow().clone() else { return };
        if self.busy.replace(true) {
            return;
        }
        let weak = Rc::downgrade(self);
        let limit = self.limit.get().to_string();
        glib::spawn_future_local(async move {
            let top = git::run(&dir, &["git", "rev-parse", "--show-toplevel"]).await.map(|s| s.trim().to_string());
            let Some(top) = top else {
                if let Some(p) = weak.upgrade() {
                    p.busy.set(false);
                    p.show_not_a_repo(&dir);
                }
                return;
            };
            let format = format!("--pretty=format:%H{FIELD}%h{FIELD}%an{FIELD}%at{FIELD}%D{FIELD}%s{RECORD}");
            let log = git::run(&top, &["git", "log", "--max-count", &limit, &format]).await.unwrap_or_default();
            // -uall lists each new file instead of just its folder.
            let status = git::run(&top, &["git", "status", "--porcelain", "--branch", "--untracked-files=all"]).await.unwrap_or_default();
            let remote = git::run(&top, &["git", "remote", "get-url", "origin"]).await;
            let Some(p) = weak.upgrade() else { return };
            p.busy.set(false);
            let signature = format!("{top}\n{status}\n{log}");
            if *p.signature.borrow() == signature {
                return;
            }
            *p.signature.borrow_mut() = signature;
            let (branch_line, changes) = match status.split_once('\n') {
                Some((first, rest)) => (first.to_string(), rest.to_string()),
                None => (status.clone(), String::new()),
            };
            *p.branch.borrow_mut() = branch_line.trim_start_matches("## ").to_string();
            *p.changes.borrow_mut() = parse_changes(&changes);
            *p.commits.borrow_mut() = parse_log(&log);
            *p.remote_web.borrow_mut() = remote.as_deref().and_then(web_url);
            *p.repo.borrow_mut() = Some(top);
            p.render();
        });
    }

    fn show_not_a_repo(&self, dir: &str) {
        self.repo_label.set_text("No repository");
        self.branch_row.set_visible(false);
        clear(&self.content);
        self.content.append(&empty(
            "cmuxp-git-symbolic",
            "Not a git repository",
            &format!("{} isn't inside a git repo. Open a terminal in a project folder and press Ctrl+Shift+G again.", tilde(dir)),
        ));
    }

    fn render(self: &Rc<Self>) {
        let Some(top) = self.repo.borrow().clone() else { return };
        let scroll_to = self.scroll.vadjustment().value();
        clear(&self.content);
        self.repo_label.set_text(top.rsplit('/').next().unwrap_or(&top));
        self.repo_label.set_tooltip_text(Some(&tilde(&top)));
        let (name, ahead, behind) = branch_parts(&self.branch.borrow());
        self.branch_row.set_visible(true);
        self.branch_name.set_text(&name);
        let mut counts = Vec::new();
        if ahead > 0 {
            counts.push(format!("↑{ahead}"));
        }
        if behind > 0 {
            counts.push(format!("↓{behind}"));
        }
        self.branch_counts.set_text(&counts.join("  "));
        self.branch_counts.set_tooltip_text(Some(&format!("{ahead} to push, {behind} to pull")));

        let filter = self.filter.text().trim().to_lowercase();
        if filter.is_empty() {
            self.render_changes(&top);
        }
        self.render_history(&top, &filter);

        let adj = self.scroll.vadjustment();
        glib::idle_add_local_once(move || adj.set_value(scroll_to));
    }

    // ---------------------------------------------------------- changes

    fn render_changes(self: &Rc<Self>, top: &str) {
        let changes = self.changes.borrow().clone();
        if changes.is_empty() {
            return;
        }
        let open = self.changes_open.get();
        let toggle = gtk::Button::new();
        toggle.add_css_class("gp-section-toggle");
        toggle.set_child(Some(&section_heading("Changes", changes.len(), Some(if open { "cmuxp-down-symbolic" } else { "cmuxp-forward-symbolic" }))));
        toggle.set_tooltip_text(Some(if open { "Collapse" } else { "Expand" }));
        toggle.connect_clicked({
            let p = Rc::downgrade(self);
            move |_| {
                if let Some(p) = p.upgrade() {
                    p.changes_open.set(!p.changes_open.get());
                    p.render();
                }
            }
        });
        self.content.append(&toggle);
        if !open {
            return;
        }
        let show_all = self.changes_all.get() || changes.len() <= CHANGES_PREVIEW + 1;
        let visible: Vec<(String, String)> = if show_all { changes.clone() } else { changes[..CHANGES_PREVIEW].to_vec() };
        let list = gtk::ListBox::new();
        list.add_css_class("gp-card");
        list.set_selection_mode(gtk::SelectionMode::None);
        for (code, path) in &visible {
            let (letter, class, meaning) = status_style(code);
            let badge = gtk::Label::new(Some(letter));
            badge.add_css_class("gp-status");
            badge.add_css_class(class);
            badge.set_tooltip_text(Some(meaning));
            list.append(&file_row(&badge.upcast(), path, None));
        }
        list.connect_row_activated({
            let (p, visible, top) = (Rc::downgrade(self), visible.clone(), top.to_string());
            move |_, row| {
                let Some(p) = p.upgrade() else { return };
                let Some((code, path)) = visible.get(row.index() as usize) else { return };
                if code == "?" {
                    p.open_file(&top, path);
                } else {
                    p.run_in_tab(&format!("git -C {} diff HEAD -- {}", quote(&top), quote(path)));
                }
            }
        });
        self.content.append(&list);
        if !show_all {
            let more = gtk::Button::with_label(&format!("Show {} more", changes.len() - CHANGES_PREVIEW));
            more.add_css_class("gp-link");
            more.set_halign(gtk::Align::Start);
            more.connect_clicked({
                let p = Rc::downgrade(self);
                move |_| {
                    if let Some(p) = p.upgrade() {
                        p.changes_all.set(true);
                        p.render();
                    }
                }
            });
            self.content.append(&more);
        }
    }

    // ---------------------------------------------------------- history

    fn render_history(self: &Rc<Self>, top: &str, filter: &str) {
        let commits = self.commits.borrow().clone();
        let visible: Vec<&Commit> = commits
            .iter()
            .filter(|c| {
                filter.is_empty()
                    || c.subject.to_lowercase().contains(filter)
                    || c.author.to_lowercase().contains(filter)
                    || c.hash.starts_with(filter)
                    || c.refs.iter().any(|r| r.to_lowercase().contains(filter))
            })
            .collect();
        self.content.append(&section_heading(if filter.is_empty() { "History" } else { "Matching commits" }, visible.len(), None));
        if visible.is_empty() {
            self.content.append(&empty("cmuxp-commit-symbolic", if commits.is_empty() { "No commits yet" } else { "No matching commits" }, ""));
            return;
        }
        let expanded = self.expanded.borrow().clone();
        let now = glib::DateTime::now_utc().map(|d| d.to_unix()).unwrap_or(0);
        let list = gtk::ListBox::new();
        list.add_css_class("gp-timeline");
        list.set_selection_mode(gtk::SelectionMode::None);
        let mut expanded_row = None;
        for (i, c) in visible.iter().enumerate() {
            let is_open = expanded.as_deref() == Some(c.hash.as_str());
            let row = commit_row(c, i == 0, i + 1 == visible.len(), now, is_open);
            if is_open {
                expanded_row = Some((row.clone(), (*c).clone()));
            }
            list.append(&row);
        }
        list.connect_row_activated({
            let (p, hashes) = (Rc::downgrade(self), visible.iter().map(|c| c.hash.clone()).collect::<Vec<_>>());
            move |_, row| {
                let Some(p) = p.upgrade() else { return };
                let Some(hash) = hashes.get(row.index() as usize).cloned() else { return };
                let mut e = p.expanded.borrow_mut();
                *e = if e.as_deref() == Some(hash.as_str()) { None } else { Some(hash) };
                drop(e);
                p.render();
            }
        });
        self.content.append(&list);

        if commits.len() >= self.limit.get() && filter.is_empty() {
            let more = gtk::Button::with_label("Load older commits");
            more.add_css_class("gp-link");
            more.set_halign(gtk::Align::Center);
            more.connect_clicked({
                let p = Rc::downgrade(self);
                move |_| {
                    if let Some(p) = p.upgrade() {
                        p.limit.set(p.limit.get() + PAGE);
                        p.refresh();
                    }
                }
            });
            self.content.append(&more);
        }
        if let Some((row, commit)) = expanded_row {
            self.fill_details(&row, &commit, top);
        }
    }

    /// Load the full message and changed files of an expanded commit.
    fn fill_details(self: &Rc<Self>, row: &gtk::ListBoxRow, commit: &Commit, top: &str) {
        let Some(details) = find_named(row.upcast_ref(), "details").and_downcast::<gtk::Box>() else { return };
        details.set_visible(true);
        let loading = gtk::Spinner::builder().spinning(true).halign(gtk::Align::Start).build();
        details.append(&loading);
        let (weak, hash, top) = (Rc::downgrade(self), commit.hash.clone(), top.to_string());
        glib::spawn_future_local(async move {
            let body = git::run(&top, &["git", "show", "-s", "--format=%b", &hash]).await.unwrap_or_default();
            let stats = git::run(&top, &["git", "show", "--numstat", "--format=", &hash]).await.unwrap_or_default();
            let Some(p) = weak.upgrade() else { return };
            clear(&details);

            let body = body.trim();
            if !body.is_empty() {
                let msg = gtk::Label::builder().label(body).xalign(0.0).wrap(true).max_width_chars(44).selectable(true).build();
                msg.add_css_class("gp-body");
                details.append(&msg);
            }

            let files: Vec<(Option<u32>, Option<u32>, String)> = stats
                .lines()
                .filter_map(|l| {
                    let mut it = l.splitn(3, '\t');
                    let (a, d, path) = (it.next()?, it.next()?, it.next()?);
                    Some((a.parse().ok(), d.parse().ok(), path.to_string()))
                })
                .collect();
            if !files.is_empty() {
                let (adds, dels): (u32, u32) = files.iter().fold((0, 0), |(a, d), f| (a + f.0.unwrap_or(0), d + f.1.unwrap_or(0)));
                let summary = gtk::Label::builder()
                    .label(format!("{} file{} changed", files.len(), if files.len() == 1 { "" } else { "s" }))
                    .xalign(0.0)
                    .hexpand(true)
                    .build();
                summary.add_css_class("gp-files-title");
                let totals = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                let plus = gtk::Label::new(Some(&format!("+{adds}")));
                plus.add_css_class("gp-add");
                let minus = gtk::Label::new(Some(&format!("−{dels}")));
                minus.add_css_class("gp-del");
                totals.append(&plus);
                totals.append(&minus);
                let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
                header.append(&summary);
                header.append(&totals);
                details.append(&header);

                let list = gtk::ListBox::new();
                list.add_css_class("gp-card");
                list.set_selection_mode(gtk::SelectionMode::None);
                for (add, del, path) in &files {
                    let icon = gtk::Image::from_icon_name("cmuxp-file-symbolic");
                    icon.add_css_class("gp-file-icon");
                    list.append(&file_row(&icon.upcast(), path, Some(diffstat(*add, *del))));
                }
                list.connect_row_activated({
                    let (p, files, hash, top) = (Rc::downgrade(&p), files.clone(), hash.clone(), top.clone());
                    move |_, row| {
                        if let (Some(p), Some((_, _, path))) = (p.upgrade(), files.get(row.index() as usize)) {
                            p.run_in_tab(&format!("git -C {} show {} -- {}", quote(&top), hash, quote(path)));
                        }
                    }
                });
                details.append(&list);
            }

            let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            actions.add_css_class("gp-actions");
            let diff = action_button("cmuxp-split-right-symbolic", "View diff");
            diff.connect_clicked({
                let (p, hash, top) = (Rc::downgrade(&p), hash.clone(), top.clone());
                move |_| {
                    if let Some(p) = p.upgrade() {
                        p.run_in_tab(&format!("git -C {} show {}", quote(&top), hash));
                    }
                }
            });
            let copy = action_button("cmuxp-copy-symbolic", "Copy hash");
            copy.connect_clicked({
                let hash = hash.clone();
                move |b| {
                    b.clipboard().set_text(&hash);
                    set_action_label(b, "Copied");
                    let b = b.clone();
                    glib::timeout_add_local_once(std::time::Duration::from_millis(1400), move || set_action_label(&b, "Copy hash"));
                }
            });
            actions.append(&diff);
            actions.append(&copy);
            if let Some(web) = p.remote_web.borrow().clone() {
                let open = action_button("cmuxp-external-symbolic", "Open on web");
                open.connect_clicked({
                    let (p, url) = (Rc::downgrade(&p), format!("{web}/commit/{hash}"));
                    move |_| {
                        if let Some(shared) = p.upgrade().and_then(|p| p.shared.upgrade()) {
                            let _ = state::open_browser(&shared, None, &url, Some(cmuxp_proto::Direction::Right));
                        }
                    }
                });
                actions.append(&open);
            }
            details.append(&actions);
        });
    }

    /// Run a command in a new terminal tab next to the focused one (diffs use your pager).
    fn run_in_tab(&self, command: &str) {
        let Some(shared) = self.shared.upgrade() else { return };
        let result = state::new_tab(&shared, None, Content::Terminal).and_then(|sid| state::send_text(&shared, Some(sid), &format!("{command}\n")));
        if let Err(e) = result {
            eprintln!("cmuxp: {e}");
        }
    }

    fn open_file(&self, top: &str, path: &str) {
        let Some(shared) = self.shared.upgrade() else { return };
        let sid = shared.borrow().focused_surface();
        if let Some(sid) = sid {
            crate::links::open_in_editor(&shared, sid, &std::path::Path::new(top).join(path), None, None);
        }
    }
}

// ------------------------------------------------------------------ widgets

fn commit_row(c: &Commit, first: bool, last: bool, now: i64, expanded: bool) -> gtk::ListBoxRow {
    // Timeline rail: line above, dot, line below.
    let above = gtk::Box::new(gtk::Orientation::Vertical, 0);
    above.add_css_class("gp-line");
    above.set_size_request(-1, 10);
    above.set_opacity(if first { 0.0 } else { 1.0 });
    let dot = gtk::Box::new(gtk::Orientation::Vertical, 0);
    dot.add_css_class("gp-dot");
    if c.refs.iter().any(|r| r.starts_with("HEAD")) {
        dot.add_css_class("head");
    }
    let below = gtk::Box::new(gtk::Orientation::Vertical, 0);
    below.add_css_class("gp-line");
    below.set_vexpand(true);
    below.set_opacity(if last { 0.0 } else { 1.0 });
    let rail = gtk::Box::new(gtk::Orientation::Vertical, 0);
    rail.add_css_class("gp-rail");
    for w in [&above, &dot, &below] {
        w.set_halign(gtk::Align::Center);
        rail.append(w);
    }

    let subject = gtk::Label::builder()
        .label(&c.subject)
        .xalign(0.0)
        .max_width_chars(40)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .tooltip_text(&c.subject)
        .build();
    subject.add_css_class("gp-subject");
    if expanded {
        subject.set_wrap(true);
        subject.set_ellipsize(gtk::pango::EllipsizeMode::None);
    }

    let avatar = gtk::Label::new(Some(&c.author.chars().next().map(|ch| ch.to_uppercase().to_string()).unwrap_or_default()));
    avatar.add_css_class("gp-avatar");
    avatar.add_css_class(&format!("tone{}", tone(&c.author)));
    avatar.set_valign(gtk::Align::Center);
    let meta = gtk::Label::builder()
        .label(format!("{}  ·  {}", c.author, ago(now - c.time)))
        .xalign(0.0)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    meta.add_css_class("gp-meta");
    let hash = gtk::Label::new(Some(&c.short));
    hash.add_css_class("gp-hash");
    let meta_line = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    meta_line.append(&avatar);
    meta_line.append(&meta);
    meta_line.append(&hash);

    let text = gtk::Box::new(gtk::Orientation::Vertical, 4);
    text.set_hexpand(true);
    text.append(&subject);
    text.append(&meta_line);
    if !c.refs.is_empty() {
        let refs = gtk::FlowBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .max_children_per_line(3)
            .row_spacing(4)
            .column_spacing(4)
            .halign(gtk::Align::Start)
            .build();
        for r in &c.refs {
            let (label, class) = ref_style(r);
            let chip = gtk::Label::new(Some(&label));
            chip.add_css_class("gp-ref");
            chip.add_css_class(class);
            refs.insert(&chip, -1);
        }
        text.append(&refs);
    }
    let details = gtk::Box::new(gtk::Orientation::Vertical, 8);
    details.add_css_class("gp-details");
    details.set_widget_name("details");
    details.set_visible(false);
    text.append(&details);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    body.append(&rail);
    body.append(&text);
    let row = gtk::ListBoxRow::builder().child(&body).build();
    row.add_css_class("gp-commit");
    if expanded {
        row.add_css_class("expanded");
    }
    row
}

/// One file line: [badge or icon]  name   dir           [extra]
fn file_row(lead: &gtk::Widget, path: &str, extra: Option<gtk::Box>) -> gtk::ListBoxRow {
    let trimmed = path.trim_end_matches('/');
    let (dir, name) = trimmed.rsplit_once('/').unwrap_or(("", trimmed));
    let name = if path.ends_with('/') { format!("{name}/") } else { name.to_string() };
    let name = name.as_str();
    let name_label = gtk::Label::builder().label(name).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::Middle).build();
    name_label.add_css_class("gp-file-name");
    let dir_label = gtk::Label::builder().label(dir).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::Start).build();
    dir_label.add_css_class("gp-file-dir");
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    line.append(lead);
    line.append(&name_label);
    line.append(&dir_label);
    if let Some(extra) = extra {
        line.append(&extra);
    }
    let row = gtk::ListBoxRow::builder().child(&line).tooltip_text(path).build();
    row.add_css_class("gp-file");
    row
}

/// GitHub-style diffstat: "+12 −3 ■■■■□"
fn diffstat(add: Option<u32>, del: Option<u32>) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    b.set_valign(gtk::Align::Center);
    let (Some(add), Some(del)) = (add, del) else {
        let bin = gtk::Label::new(Some("binary"));
        bin.add_css_class("gp-binary");
        b.append(&bin);
        return b;
    };
    let plus = gtk::Label::new(Some(&format!("+{add}")));
    plus.add_css_class("gp-add");
    let minus = gtk::Label::new(Some(&format!("−{del}")));
    minus.add_css_class("gp-del");
    minus.set_margin_end(4);
    b.append(&plus);
    b.append(&minus);
    let total = add + del;
    let green = if total == 0 { 0 } else { ((add as f64 / total as f64) * 5.0).round() as u32 };
    let red = if total == 0 { 0 } else { 5 - green };
    for i in 0..5 {
        let block = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        block.add_css_class("gp-block");
        block.add_css_class(if i < green {
            "add"
        } else if i < green + red {
            "del"
        } else {
            "none"
        });
        block.set_valign(gtk::Align::Center);
        b.append(&block);
    }
    b
}

fn section_heading(title: &str, count: usize, chevron: Option<&str>) -> gtk::Box {
    let t = gtk::Label::new(Some(&title.to_uppercase()));
    t.add_css_class("gp-heading");
    let n = gtk::Label::new(Some(&count.to_string()));
    n.add_css_class("gp-count");
    let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    line.add_css_class("gp-heading-row");
    line.append(&t);
    line.append(&n);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    line.append(&spacer);
    if let Some(icon) = chevron {
        let c = gtk::Image::from_icon_name(icon);
        c.add_css_class("gp-chevron");
        line.append(&c);
    }
    line
}

fn action_button(icon: &str, label: &str) -> gtk::Button {
    let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let i = gtk::Image::from_icon_name(icon);
    i.set_pixel_size(14);
    content.append(&i);
    content.append(&gtk::Label::new(Some(label)));
    let b = gtk::Button::builder().child(&content).build();
    b.add_css_class("gp-action");
    b
}

fn set_action_label(button: &gtk::Button, text: &str) {
    if let Some(label) = button.child().and_then(|c| c.last_child()).and_downcast::<gtk::Label>() {
        label.set_text(text);
    }
}

/// (letter, css class, meaning) for a porcelain status code.
fn status_style(code: &str) -> (&'static str, &'static str, &'static str) {
    match code.chars().next() {
        Some('M') => ("M", "modified", "Modified"),
        Some('A') => ("A", "added", "Added"),
        Some('D') => ("D", "deleted", "Deleted"),
        Some('R') => ("R", "renamed", "Renamed"),
        Some('C') => ("C", "renamed", "Copied"),
        Some('?') => ("U", "untracked", "Untracked (new file)"),
        Some('U') => ("!", "conflict", "Conflict"),
        _ => ("•", "other", "Changed"),
    }
}

/// Display text and style for a ref from `%D`.
fn ref_style(r: &str) -> (String, &'static str) {
    if let Some(branch) = r.strip_prefix("HEAD -> ") {
        (branch.to_string(), "head")
    } else if let Some(tag) = r.strip_prefix("tag: ") {
        (tag.to_string(), "tag")
    } else if r.contains('/') {
        (r.to_string(), "remote")
    } else {
        (r.to_string(), "branch")
    }
}

/// A stable colour (0..6) for an author's avatar.
fn tone(name: &str) -> u32 {
    name.bytes().fold(0u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32)) % 6
}

fn clear(b: &gtk::Box) {
    while let Some(c) = b.first_child() {
        b.remove(&c);
    }
}

fn find_named(widget: &gtk::Widget, name: &str) -> Option<gtk::Widget> {
    if widget.widget_name() == name {
        return Some(widget.clone());
    }
    let mut child = widget.first_child();
    while let Some(c) = child {
        if let Some(found) = find_named(&c, name) {
            return Some(found);
        }
        child = c.next_sibling();
    }
    None
}

fn empty(icon: &str, title: &str, body: &str) -> gtk::Box {
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(28);
    image.add_css_class("empty-icon");
    let t = gtk::Label::new(Some(title));
    t.add_css_class("empty-title");
    let col = gtk::Box::new(gtk::Orientation::Vertical, 8);
    col.add_css_class("empty-state");
    col.append(&image);
    col.append(&t);
    if !body.is_empty() {
        let b = gtk::Label::builder().label(body).wrap(true).justify(gtk::Justification::Center).max_width_chars(40).build();
        b.add_css_class("empty-body");
        col.append(&b);
    }
    col
}

/// "master...origin/master [ahead 1, behind 2]" → ("master", 1, 2)
pub fn branch_parts(line: &str) -> (String, u32, u32) {
    let line = line.replace("No commits yet on ", "");
    let (name, rest) = match line.split_once("...") {
        Some((n, r)) => (n.to_string(), r.to_string()),
        None => (line.split_whitespace().next().unwrap_or(&line).to_string(), String::new()),
    };
    let (mut ahead, mut behind) = (0, 0);
    if let Some(ab) = rest.split_once('[').map(|(_, r)| r.trim_end_matches(']').to_string()) {
        for part in ab.split(", ") {
            if let Some(n) = part.strip_prefix("ahead ") {
                ahead = n.parse().unwrap_or(0);
            } else if let Some(n) = part.strip_prefix("behind ") {
                behind = n.parse().unwrap_or(0);
            }
        }
    }
    (name, ahead, behind)
}

/// git@github.com:o/r.git or https://github.com/o/r.git → https://github.com/o/r
pub fn web_url(remote: &str) -> Option<String> {
    let r = remote.trim().trim_end_matches(".git");
    let url = if let Some(rest) = r.strip_prefix("git@") {
        let (host, path) = rest.split_once(':')?;
        format!("https://{host}/{path}")
    } else if r.starts_with("https://") || r.starts_with("http://") {
        r.to_string()
    } else {
        return None;
    };
    ["github.com", "gitlab.com", "codeberg.org", "bitbucket.org"].iter().any(|h| url.contains(h)).then_some(url)
}

fn ago(secs: i64) -> String {
    let s = secs.max(0) as u64;
    match s {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        86_400..=2_591_999 => format!("{}d ago", s / 86_400),
        2_592_000..=31_535_999 => format!("{}mo ago", s / 2_592_000),
        _ => format!("{}y ago", s / 31_536_000),
    }
}

fn tilde(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_parsing() {
        let out = format!(
            "abc123{f}abc{f}Ana{f}1700000000{f}HEAD -> main, origin/main, tag: v1{f}Fix: a{f}b{r}\ndef456{f}def{f}Bo{f}1690000000{f}{f}First{r}",
            f = FIELD,
            r = RECORD
        );
        let commits = parse_log(&out);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].refs, vec!["HEAD -> main", "origin/main", "tag: v1"]);
        assert_eq!(commits[0].subject, format!("Fix: a{FIELD}b"));
        assert!(commits[1].refs.is_empty());
    }

    #[test]
    fn status_and_branch() {
        assert_eq!(parse_changes(" M src/a.rs\n?? new.txt\nR  old -> new\n"), vec![
            ("M".to_string(), "src/a.rs".to_string()),
            ("?".to_string(), "new.txt".to_string()),
            ("R".to_string(), "new".to_string()),
        ]);
        assert_eq!(branch_parts("master...origin/master [ahead 1, behind 2]"), ("master".to_string(), 1, 2));
        assert_eq!(branch_parts("feature"), ("feature".to_string(), 0, 0));
        assert_eq!(status_style("?").0, "U");
        assert_eq!(ref_style("HEAD -> main"), ("main".to_string(), "head"));
        assert_eq!(ref_style("tag: v1"), ("v1".to_string(), "tag"));
    }

    #[test]
    fn remotes() {
        assert_eq!(web_url("git@github.com:me/repo.git").as_deref(), Some("https://github.com/me/repo"));
        assert_eq!(web_url("https://gitlab.com/me/repo.git").as_deref(), Some("https://gitlab.com/me/repo"));
        assert_eq!(web_url("/srv/git/repo.git"), None);
    }
}
