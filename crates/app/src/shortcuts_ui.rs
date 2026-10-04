//! The Keyboard Shortcuts window.
//!
//!   ┌ Keyboard Shortcuts ─────────────────────────────────────────────┐
//!   │ [search      ] │  Workspaces                                     │
//!   │ All         27 │  Create workspaces and move between them.       │
//!   │ Yours        2 │  ┌───────────────────────────────────────────┐  │
//!   │ Workspaces  12 │  │ [+] New workspace        [Ctrl][Shift][N] │  │
//!   │ Tabs & panes 9 │  │     Add a workspace…                      │  │
//!   │ …              │  └───────────────────────────────────────────┘  │
//!   │ ~/.config/…    │                                   ( Saved ✓ )   │
//!   └─────────────────────────────────────────────────────────────────┘
//!
//! Click a row to record new keys. Custom shortcuts live under "Your shortcuts".
//! The window is deliberately *not* attached to the GtkApplication, so the
//! app's own shortcuts don't fire while you're recording keys in it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::{Rc, Weak};

use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use crate::shortcuts::{self, Builtin, Custom, CustomKind, Keymap, Owner, BUILTINS, CUSTOM_KINDS, GROUPS};

const ALL: &str = "All shortcuts";
const MINE: &str = "Your shortcuts";

/// One-click starters shown while you have no custom shortcuts.
const PRESETS: &[(&str, CustomKind, &str, &str)] = &[
    ("Git status", CustomKind::Run, "git status", "ctrl+alt+g"),
    ("Claude in a new tab", CustomKind::RunInNewTab, "claude", "ctrl+alt+c"),
    ("Open localhost:3000", CustomKind::Browser, "localhost:3000", "ctrl+alt+l"),
];

fn category_icon(name: &str) -> &'static str {
    match name {
        ALL => "cmuxp-keyboard-symbolic",
        MINE => "cmuxp-pencil-symbolic",
        "Workspaces" => "cmuxp-folder-symbolic",
        "Tabs and panes" => "cmuxp-split-right-symbolic",
        "Window" => "cmuxp-sidebar-symbolic",
        _ => "cmuxp-terminal-symbolic",
    }
}

fn category_subtitle(name: &str) -> &'static str {
    match name {
        ALL => "Click any shortcut to change it. Changes save automatically.",
        MINE => "Your own keys for commands, text, URLs and folders.",
        "Workspaces" => "Create workspaces and move between them.",
        "Tabs and panes" => "Open, close, split and move between tabs.",
        "Window" => "Sidebar, notifications and this window.",
        _ => "Search and clipboard inside the terminal.",
    }
}

struct Ui {
    win: gtk::Window,
    keymap: Weak<Keymap>,
    search: gtk::SearchEntry,
    nav: gtk::ListBox,
    counts: RefCell<HashMap<&'static str, gtk::Label>>,
    page: gtk::Box,
    scroll: gtk::ScrolledWindow,
    subtitle: gtk::Label,
    toast: gtk::Revealer,
    toast_label: gtk::Label,
    toast_generation: Cell<u64>,
    category: RefCell<&'static str>,
    reset_armed: Cell<bool>,
}

thread_local! {
    static OPEN: RefCell<Option<Rc<Ui>>> = const { RefCell::new(None) };
}

/// The shortcuts window if it is open (used by `cmuxpctl screenshot --window shortcuts`).
pub fn current() -> Option<gtk::Window> {
    OPEN.with(|o| o.borrow().as_ref().map(|ui| ui.win.clone())).filter(|w| w.is_visible())
}

/// What to show when opening: a category, the "new shortcut" editor, or the
/// key recorder for one built-in action (used by `cmuxpctl shortcuts --open ...`).
#[derive(Default)]
pub struct Target {
    pub section: Option<String>,
    pub new: bool,
    pub change: Option<String>,
}

/// Show the window (or bring the open one to the front).
pub fn open(parent: &gtk::Window, keymap: &Rc<Keymap>) {
    open_at(parent, keymap, Target::default());
}

pub fn open_at(parent: &gtk::Window, keymap: &Rc<Keymap>, target: Target) {
    if current().is_none() {
        build(parent, keymap);
    }
    let Some(ui) = OPEN.with(|o| o.borrow().clone()) else { return };
    ui.win.present();
    if let Some(section) = target.section.as_deref() {
        let wanted = section.to_lowercase();
        let categories = [ALL, MINE].into_iter().chain(GROUPS.iter().copied());
        if let Some(name) = categories.into_iter().find(|c| c.to_lowercase().contains(&wanted) || (wanted == "yours" && *c == MINE)) {
            let mut i = 0;
            while let Some(row) = ui.nav.row_at_index(i) {
                if row.widget_name() == name {
                    ui.nav.select_row(Some(&row));
                }
                i += 1;
            }
        }
    }
    if target.new {
        edit_custom(&ui, None, None);
    }
    if let Some(action) = target.change {
        if let Some(b) = BUILTINS.iter().find(|b| b.action == action) {
            ui.change_builtin(b);
        }
    }
}

fn build(parent: &gtk::Window, keymap: &Rc<Keymap>) {

    let win = gtk::Window::builder()
        .title("Keyboard Shortcuts")
        .transient_for(parent)
        .destroy_with_parent(true)
        .default_width(980)
        .default_height(720)
        .build();
    win.add_css_class("shortcuts-window");

    // ---- title bar
    let title = gtk::Label::new(Some("Keyboard Shortcuts"));
    title.add_css_class("title-text");
    let subtitle = gtk::Label::new(None);
    subtitle.add_css_class("header-subtitle");
    let titles = gtk::Box::new(gtk::Orientation::Vertical, 0);
    titles.set_valign(gtk::Align::Center);
    titles.append(&title);
    titles.append(&subtitle);
    let header = gtk::HeaderBar::new();
    header.add_css_class("cmuxp-header");
    header.set_title_widget(Some(&titles));
    win.set_titlebar(Some(&header));

    // ---- left: search + categories + footer
    let search = gtk::SearchEntry::builder().placeholder_text("Search shortcuts").build();
    search.add_css_class("nav-search");
    let nav = gtk::ListBox::new();
    nav.add_css_class("nav-list");
    nav.set_selection_mode(gtk::SelectionMode::Single);

    let path_label = gtk::Label::builder()
        .label(tilde(&keymap.path().display().to_string()))
        .xalign(0.0)
        .ellipsize(gtk::pango::EllipsizeMode::Start)
        .max_width_chars(30)
        .tooltip_text(keymap.path().display().to_string())
        .build();
    path_label.add_css_class("nav-path");
    let open_file = gtk::Button::with_label("Open settings file");
    open_file.add_css_class("link-button");
    open_file.set_halign(gtk::Align::Start);
    let reset_all = gtk::Button::with_label("Reset all to defaults");
    reset_all.add_css_class("link-button");
    reset_all.add_css_class("danger");
    reset_all.set_halign(gtk::Align::Start);
    let footer = gtk::Box::new(gtk::Orientation::Vertical, 2);
    footer.add_css_class("nav-footer");
    footer.append(&path_label);
    footer.append(&open_file);
    footer.append(&reset_all);

    let nav_column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    nav_column.add_css_class("nav-column");
    nav_column.set_width_request(236);
    let nav_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).child(&nav).build();
    nav_column.append(&search);
    nav_column.append(&nav_scroll);
    nav_column.append(&footer);

    // ---- right: the page
    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.add_css_class("page");
    let scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).hexpand(true).vexpand(true).child(&page).build();

    let split = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    split.append(&nav_column);
    split.append(&gtk::Separator::new(gtk::Orientation::Vertical));
    split.append(&scroll);

    // ---- toast
    let toast_label = gtk::Label::new(None);
    let toast_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    toast_box.add_css_class("toast");
    toast_box.append(&toast_label);
    let toast = gtk::Revealer::builder()
        .transition_type(gtk::RevealerTransitionType::SlideUp)
        .transition_duration(180)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::End)
        .child(&toast_box)
        .build();
    toast.set_can_target(false);
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&split));
    overlay.add_overlay(&toast);
    win.set_child(Some(&overlay));

    let ui = Rc::new(Ui {
        win: win.clone(),
        keymap: Rc::downgrade(keymap),
        search: search.clone(),
        nav: nav.clone(),
        counts: RefCell::default(),
        page,
        scroll,
        subtitle,
        toast,
        toast_label,
        toast_generation: Cell::new(0),
        category: RefCell::new(ALL),
        reset_armed: Cell::new(false),
    });

    // Categories.
    let categories: Vec<&'static str> = [ALL, MINE].into_iter().chain(GROUPS.iter().copied()).collect();
    for name in &categories {
        let icon = gtk::Image::from_icon_name(category_icon(name));
        let label = gtk::Label::builder().label(*name).xalign(0.0).hexpand(true).build();
        let count = gtk::Label::new(None);
        count.add_css_class("nav-count");
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        line.append(&icon);
        line.append(&label);
        line.append(&count);
        let row = gtk::ListBoxRow::builder().child(&line).build();
        row.add_css_class("nav-row");
        row.set_widget_name(name);
        nav.append(&row);
        ui.counts.borrow_mut().insert(name, count);
    }
    nav.select_row(nav.row_at_index(0).as_ref());
    nav.connect_row_selected({
        let ui = Rc::downgrade(&ui);
        move |_, row| {
            let (Some(ui), Some(row)) = (ui.upgrade(), row) else { return };
            let name = categories.iter().copied().find(|c| *c == row.widget_name()).unwrap_or(ALL);
            if *ui.category.borrow() != name {
                *ui.category.borrow_mut() = name;
                ui.render(false);
            }
        }
    });

    search.connect_search_changed({
        let ui = Rc::downgrade(&ui);
        move |_| {
            if let Some(ui) = ui.upgrade() {
                ui.render(false);
            }
        }
    });
    open_file.connect_clicked({
        let keymap = keymap.clone();
        move |_| {
            if !keymap.path().exists() {
                keymap.commit();
            }
            let uri = gio::File::for_path(keymap.path()).uri();
            if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
                eprintln!("cmuxp: cannot open {uri}: {e}");
            }
        }
    });
    // Two-step reset: the first click arms it, a second click within 4 s resets.
    reset_all.connect_clicked({
        let ui = Rc::downgrade(&ui);
        move |button| {
            let Some(ui) = ui.upgrade() else { return };
            if ui.reset_armed.replace(!ui.reset_armed.get()) {
                if let Some(km) = ui.keymap.upgrade() {
                    km.reset_all();
                    ui.commit(&km, "All built-in shortcuts reset");
                }
                button.set_label("Reset all to defaults");
            } else {
                button.set_label("Click again to reset everything");
                let (weak, button) = (Rc::downgrade(&ui), button.clone());
                glib::timeout_add_local_once(std::time::Duration::from_secs(4), move || {
                    if let Some(ui) = weak.upgrade() {
                        ui.reset_armed.set(false);
                        button.set_label("Reset all to defaults");
                    }
                });
            }
        }
    });

    // Redraw whenever shortcuts change (here, or by hand-editing the file).
    keymap.on_change({
        let ui = Rc::downgrade(&ui);
        move || {
            if let Some(ui) = ui.upgrade() {
                ui.render(true);
            }
        }
    });

    ui.render(false);
    // Forget the window as soon as it's asked to close. `OPEN` holds the only
    // strong reference to our state; if we kept it, GTK would hide the window
    // but could never finish destroying it, and "open" would keep re-showing
    // that half-dead window.
    win.connect_close_request(|_| {
        OPEN.with(|o| o.borrow_mut().take());
        glib::Propagation::Proceed
    });
    OPEN.with(|o| *o.borrow_mut() = Some(ui));
    search.grab_focus();
}

/// The open dialog (key recorder or shortcut editor), if any.
pub fn current_dialog() -> Option<gtk::Window> {
    gtk::Window::list_toplevels()
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Window>().ok())
        .filter(|w| w.has_css_class("cmuxp-dialog") && w.is_visible())
        .last()
}

impl Ui {
    fn commit(&self, keymap: &Keymap, message: &str) {
        keymap.commit();
        self.toast(message);
    }

    fn toast(&self, message: &str) {
        self.toast_label.set_text(message);
        self.toast.set_reveal_child(true);
        let generation = self.toast_generation.get() + 1;
        self.toast_generation.set(generation);
        let ui = OPEN.with(|o| o.borrow().as_ref().map(Rc::downgrade));
        glib::timeout_add_local_once(std::time::Duration::from_millis(2400), move || {
            if let Some(ui) = ui.and_then(|u| u.upgrade()) {
                if ui.toast_generation.get() == generation {
                    ui.toast.set_reveal_child(false);
                }
            }
        });
    }

    /// Rebuild the page. `keep_scroll` keeps the position (used after edits).
    fn render(self: &Rc<Self>, keep_scroll: bool) {
        let Some(keymap) = self.keymap.upgrade() else { return };
        let scroll_to = keep_scroll.then(|| self.scroll.vadjustment().value());
        while let Some(child) = self.page.first_child() {
            self.page.remove(&child);
        }

        let open_keys = keymap.keys_for("show-shortcuts");
        self.subtitle.set_text(&match open_keys.first() {
            Some(k) => format!("Open anytime with {}", shortcuts::label(k)),
            None => "Changes save automatically".to_string(),
        });
        self.update_counts(&keymap);

        let filter = self.search.text().trim().to_lowercase();
        let category = if filter.is_empty() { *self.category.borrow() } else { ALL };
        let (title, subtitle) = if filter.is_empty() {
            (category.to_string(), category_subtitle(category).to_string())
        } else {
            (format!("Results for “{}”", self.search.text().trim()), "Matching names, descriptions and keys.".to_string())
        };
        let heading = gtk::Label::builder().label(&title).xalign(0.0).build();
        heading.add_css_class("page-title");
        let sub = gtk::Label::builder().label(&subtitle).xalign(0.0).wrap(true).build();
        sub.add_css_class("page-subtitle");
        self.page.append(&heading);
        self.page.append(&sub);

        let matches = |texts: &[&str], keys: &[String]| {
            filter.is_empty()
                || texts.iter().any(|t| t.to_lowercase().contains(&filter))
                || keys.iter().any(|k| shortcuts::label(k).to_lowercase().contains(&filter) || k.contains(&filter))
        };
        let mut shown = 0;

        // ---- your shortcuts
        if category == ALL || category == MINE {
            let customs = keymap.config.borrow().custom.clone();
            let visible: Vec<(usize, &Custom)> = customs
                .iter()
                .enumerate()
                .filter(|(_, c)| matches(&[&c.name, &c.action.describe(&c.value)], &c.keys))
                .collect();
            if customs.is_empty() && filter.is_empty() {
                self.page.append(&self.section_heading(MINE, true));
                self.page.append(&self.empty_custom_state(&keymap));
                shown += 1;
            } else if !visible.is_empty() {
                self.page.append(&self.section_heading(MINE, true));
                let list = card_list();
                for (i, c) in visible {
                    list.append(&self.custom_row(i, c));
                    shown += 1;
                }
                self.page.append(&list);
            }
        }

        // ---- built-ins
        for group in GROUPS {
            if category != ALL && category != *group {
                continue;
            }
            let items: Vec<&'static Builtin> = BUILTINS
                .iter()
                .filter(|b| b.group == *group && keymap.is_available(b.action))
                .filter(|b| matches(&[b.label, b.description, b.action], &keymap.keys_for(b.action)))
                .collect();
            if items.is_empty() {
                continue;
            }
            if category == ALL {
                self.page.append(&self.section_heading(group, false));
            } else {
                self.page.append(&spacer(16));
            }
            let list = card_list();
            for b in items {
                list.append(&self.builtin_row(&keymap, b));
                shown += 1;
            }
            // The whole row is the "change" button; the reset button inside handles its own clicks.
            list.connect_row_activated({
                let ui = Rc::downgrade(self);
                move |_, row| {
                    let (Some(ui), Some(action)) = (ui.upgrade(), row.widget_name().strip_prefix("builtin:").map(String::from)) else {
                        return;
                    };
                    if let Some(b) = BUILTINS.iter().find(|b| b.action == action) {
                        ui.change_builtin(b);
                    }
                }
            });
            self.page.append(&list);
        }

        if shown == 0 {
            self.page.append(&empty_state("cmuxp-keyboard-symbolic", "Nothing found", "Try another word, or a key like “ctrl+d”."));
        }

        if let Some(value) = scroll_to {
            let adj = self.scroll.vadjustment();
            glib::idle_add_local_once(move || adj.set_value(value));
        } else {
            self.scroll.vadjustment().set_value(0.0);
        }
    }

    fn update_counts(&self, keymap: &Keymap) {
        let counts = self.counts.borrow();
        let available: Vec<&Builtin> = BUILTINS.iter().filter(|b| keymap.is_available(b.action)).collect();
        let customs = keymap.config.borrow().custom.len();
        for (name, label) in counts.iter() {
            let n = match *name {
                ALL => available.len() + customs,
                MINE => customs,
                group => available.iter().filter(|b| b.group == group).count(),
            };
            label.set_text(&n.to_string());
        }
    }

    fn section_heading(self: &Rc<Self>, title: &str, with_add: bool) -> gtk::Box {
        // On the category's own page the big page title already says it.
        let shown = if *self.category.borrow() == title && self.search.text().is_empty() { "" } else { title };
        let label = gtk::Label::builder().label(shown).xalign(0.0).hexpand(true).build();
        label.add_css_class("section-title");
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        line.add_css_class("section-heading");
        line.append(&label);
        if with_add {
            let add = gtk::Button::new();
            let content = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            content.append(&gtk::Image::from_icon_name("cmuxp-plus-symbolic"));
            content.append(&gtk::Label::new(Some("New shortcut")));
            add.set_child(Some(&content));
            add.add_css_class("pill-button");
            add.connect_clicked({
                let ui = Rc::downgrade(self);
                move |_| {
                    if let Some(ui) = ui.upgrade() {
                        edit_custom(&ui, None, None);
                    }
                }
            });
            line.append(&add);
        }
        line
    }

    fn empty_custom_state(self: &Rc<Self>, keymap: &Rc<Keymap>) -> gtk::Box {
        let card = empty_state(
            "cmuxp-keyboard-symbolic",
            "Make your own shortcuts",
            "Run a command, type a snippet, open a URL or a project folder with one key.",
        );
        card.add_css_class("empty-card");
        let quick = gtk::Label::new(Some("QUICK START"));
        quick.add_css_class("quick-title");
        card.append(&quick);
        let presets = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        presets.set_halign(gtk::Align::Center);
        presets.set_homogeneous(true);
        for (name, kind, value, keys) in PRESETS {
            let button = gtk::Button::new();
            button.add_css_class("preset");
            let col = gtk::Box::new(gtk::Orientation::Vertical, 10);
            let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
            top.append(&gtk::Image::from_icon_name(kind.icon()));
            let title = gtk::Label::builder().label(*name).xalign(0.0).build();
            title.add_css_class("preset-title");
            top.append(&title);
            col.append(&top);
            let caps = keycaps_widget(&[keys.to_string()]);
            caps.set_halign(gtk::Align::Start);
            col.append(&caps);
            button.set_child(Some(&col));
            button.connect_clicked({
                let (ui, keymap) = (Rc::downgrade(self), keymap.clone());
                let custom = Custom { name: name.to_string(), keys: vec![keys.to_string()], action: *kind, value: value.to_string() };
                move |_| {
                    let Some(ui) = ui.upgrade() else { return };
                    if keymap.owners_of(&custom.keys[0], None).is_empty() {
                        keymap.config.borrow_mut().custom.push(custom.clone());
                        ui.commit(&keymap, &format!("Added “{}” · {}", custom.name, shortcuts::label(&custom.keys[0])));
                    } else {
                        // Keys already taken: open the editor so the user can pick others.
                        let mut c = custom.clone();
                        c.keys.clear();
                        edit_custom(&ui, None, Some(c));
                    }
                }
            });
            presets.append(&button);
        }
        card.append(&presets);
        let create = gtk::Button::with_label("Create your own");
        create.add_css_class("suggested-action");
        create.set_halign(gtk::Align::Center);
        create.connect_clicked({
            let ui = Rc::downgrade(self);
            move |_| {
                if let Some(ui) = ui.upgrade() {
                    edit_custom(&ui, None, None);
                }
            }
        });
        card.append(&create);
        card
    }

    fn builtin_row(self: &Rc<Self>, keymap: &Rc<Keymap>, b: &'static Builtin) -> gtk::ListBoxRow {
        let changed = !keymap.is_default(b.action);
        let reset = icon_button("cmuxp-undo-symbolic", &format!("Reset to {}", defaults_text(b)));
        reset.set_visible(changed);
        reset.connect_clicked({
            let (ui, keymap) = (Rc::downgrade(self), keymap.clone());
            move |_| {
                if let Some(ui) = ui.upgrade() {
                    keymap.reset_builtin(b.action);
                    ui.commit(&keymap, &format!("“{}” reset to {}", b.label, defaults_text(b)));
                }
            }
        });
        let pencil = gtk::Image::from_icon_name("cmuxp-pencil-symbolic");
        pencil.add_css_class("row-edit");
        let row = shortcut_row(b.icon, b.label, b.description, changed, &keymap.keys_for(b.action), &[reset.upcast(), pencil.upcast()]);
        row.set_activatable(true);
        row.set_tooltip_text(Some("Click to change"));
        row.set_widget_name(&format!("builtin:{}", b.action));
        row
    }

    fn change_builtin(self: &Rc<Self>, b: &'static Builtin) {
        let Some(keymap) = self.keymap.upgrade() else { return };
        let ui = Rc::downgrade(self);
        let km = keymap.clone();
        capture(&self.win, b.label, b.icon, &keymap, Owner::Builtin(b.action), true, &keymap.keys_for(b.action), move |spec| {
            let Some(ui) = ui.upgrade() else { return };
            let msg = match &spec {
                Some(s) => {
                    km.steal(s, Some(&Owner::Builtin(b.action)));
                    format!("“{}” is now {}", b.label, shortcuts::label(s))
                }
                None => format!("“{}” has no shortcut now", b.label),
            };
            km.set_builtin(b.action, spec.into_iter().collect());
            ui.commit(&km, &msg);
        });
    }

    fn custom_row(self: &Rc<Self>, index: usize, c: &Custom) -> gtk::ListBoxRow {
        let edit = icon_button("cmuxp-pencil-symbolic", "Edit");
        edit.connect_clicked({
            let ui = Rc::downgrade(self);
            move |_| {
                if let Some(ui) = ui.upgrade() {
                    edit_custom(&ui, Some(index), None);
                }
            }
        });
        let delete = icon_button("cmuxp-trash-symbolic", "Delete");
        delete.add_css_class("danger");
        delete.connect_clicked({
            let (ui, name) = (Rc::downgrade(self), c.name.clone());
            move |_| {
                let Some(ui) = ui.upgrade() else { return };
                let Some(keymap) = ui.keymap.upgrade() else { return };
                if index < keymap.config.borrow().custom.len() {
                    keymap.config.borrow_mut().custom.remove(index);
                }
                ui.commit(&keymap, &format!("Deleted “{name}”"));
            }
        });
        shortcut_row(c.action.icon(), &c.name, &c.action.describe(&c.value), false, &c.keys, &[edit.upcast(), delete.upcast()])
    }
}

// ------------------------------------------------------------------ widgets

fn tilde(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    match path.strip_prefix(&home) {
        Some(rest) if !home.is_empty() => format!("~{rest}"),
        _ => path.to_string(),
    }
}

fn defaults_text(b: &Builtin) -> String {
    if b.defaults.is_empty() {
        "none".into()
    } else {
        b.defaults.iter().map(|d| shortcuts::label(d)).collect::<Vec<_>>().join(" or ")
    }
}

fn spacer(height: i32) -> gtk::Box {
    let s = gtk::Box::new(gtk::Orientation::Vertical, 0);
    s.set_height_request(height);
    s
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = crate::panes::icon_button(icon, tooltip);
    b.set_valign(gtk::Align::Center);
    b
}

fn tile(icon: &str) -> gtk::CenterBox {
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(16);
    let t = gtk::CenterBox::new();
    t.add_css_class("icon-tile");
    t.set_size_request(22, 22);
    t.set_valign(gtk::Align::Center);
    t.set_halign(gtk::Align::Start);
    t.set_center_widget(Some(&image));
    t
}

fn card_list() -> gtk::ListBox {
    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("card-list");
    list
}

/// Keys as keycaps: [Ctrl][Shift][D]  or  [Alt][\]
pub fn keycaps_widget(keys: &[String]) -> gtk::Box {
    let all = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    all.set_valign(gtk::Align::Center);
    if keys.is_empty() {
        let none = gtk::Label::new(Some("Not set"));
        none.add_css_class("key-none");
        all.append(&none);
    }
    for (i, spec) in keys.iter().enumerate() {
        if i > 0 {
            let or = gtk::Label::new(Some("or"));
            or.add_css_class("key-or");
            all.append(&or);
        }
        let combo = gtk::Box::new(gtk::Orientation::Horizontal, 3);
        for cap in shortcuts::keycaps(spec) {
            let l = gtk::Label::new(Some(&cap));
            l.add_css_class("keycap");
            combo.append(&l);
        }
        all.append(&combo);
    }
    all
}

fn shortcut_row(icon: &str, name: &str, description: &str, changed: bool, keys: &[String], actions: &[gtk::Widget]) -> gtk::ListBoxRow {
    let title = gtk::Label::builder().label(name).xalign(0.0).build();
    title.add_css_class("row-title");
    let title_line = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    title_line.append(&title);
    if changed {
        let badge = gtk::Label::new(Some("Changed"));
        badge.add_css_class("changed-badge");
        badge.set_valign(gtk::Align::Center);
        title_line.append(&badge);
    }
    let desc = gtk::Label::builder().label(description).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    desc.add_css_class("row-desc");
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    text.set_valign(gtk::Align::Center);
    text.append(&title_line);
    text.append(&desc);

    let line = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    line.append(&tile(icon));
    line.append(&text);
    line.append(&keycaps_widget(keys));
    let action_box = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    action_box.add_css_class("row-actions");
    action_box.set_valign(gtk::Align::Center);
    for a in actions {
        action_box.append(a);
    }
    line.append(&action_box);

    let row = gtk::ListBoxRow::builder().child(&line).activatable(false).build();
    row.add_css_class("shortcut-row");
    row
}

fn empty_state(icon: &str, title: &str, body: &str) -> gtk::Box {
    let image = gtk::Image::from_icon_name(icon);
    image.set_pixel_size(32);
    image.add_css_class("empty-icon");
    let t = gtk::Label::new(Some(title));
    t.add_css_class("empty-title");
    let b = gtk::Label::builder().label(body).wrap(true).justify(gtk::Justification::Center).max_width_chars(52).build();
    b.add_css_class("empty-body");
    let col = gtk::Box::new(gtk::Orientation::Vertical, 10);
    col.add_css_class("empty-state");
    col.append(&image);
    col.append(&t);
    col.append(&b);
    col
}

fn callout(icon: &str) -> (gtk::Box, gtk::Label) {
    let image = gtk::Image::from_icon_name(icon);
    image.set_valign(gtk::Align::Start);
    let label = gtk::Label::builder().xalign(0.0).wrap(true).hexpand(true).build();
    let b = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    b.add_css_class("callout");
    b.append(&image);
    b.append(&label);
    b.set_visible(false);
    (b, label)
}

/// A small modal window with Cancel / title / Save in its title bar.
fn dialog(parent: &gtk::Window, title: &str, width: i32) -> (gtk::Window, gtk::Button) {
    let win = gtk::Window::builder().title(title).transient_for(parent).modal(true).default_width(width).resizable(false).build();
    win.add_css_class("cmuxp-dialog");
    let header = gtk::HeaderBar::new();
    header.add_css_class("cmuxp-header");
    header.set_show_title_buttons(false);
    let label = gtk::Label::new(Some(title));
    label.add_css_class("title-text");
    header.set_title_widget(Some(&label));
    let cancel = gtk::Button::with_label("Cancel");
    cancel.add_css_class("header-button");
    let save = gtk::Button::with_label("Save");
    save.add_css_class("suggested-action");
    header.pack_start(&cancel);
    header.pack_end(&save);
    win.set_titlebar(Some(&header));
    cancel.connect_clicked({
        let win = win.clone();
        move |_| win.close()
    });
    (win, save)
}

// ------------------------------------------------------------- key recorder

/// Ask the user to press a key combo. `on_save(Some(spec))` assigns it,
/// `on_save(None)` removes the shortcut (only offered when `can_disable`).
/// Conflicts are shown here but resolved by the caller when it saves.
#[allow(clippy::too_many_arguments)]
pub fn capture(
    parent: &gtk::Window,
    what: &str,
    icon: &str,
    keymap: &Rc<Keymap>,
    owner: Owner,
    can_disable: bool,
    current: &[String],
    on_save: impl Fn(Option<String>) + 'static,
) {
    let (dialog, save) = dialog(parent, "Set shortcut", 500);
    save.set_sensitive(false);

    let name = gtk::Label::builder().label(what).xalign(0.0).build();
    name.add_css_class("row-title");
    let now = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let now_label = gtk::Label::new(Some("Current"));
    now_label.add_css_class("row-desc");
    now.append(&now_label);
    now.append(&keycaps_widget(current));
    let head_text = gtk::Box::new(gtk::Orientation::Vertical, 6);
    head_text.append(&name);
    head_text.append(&now);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    head.append(&tile(icon));
    head.append(&head_text);

    // The stage shows the keys as you press them; held modifiers appear faintly first.
    let stage = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    stage.add_css_class("capture-stage");
    let stage_inner = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    stage_inner.set_halign(gtk::Align::Center);
    stage_inner.set_valign(gtk::Align::Center);
    stage_inner.set_hexpand(true);
    stage.append(&stage_inner);

    let hint = gtk::Label::builder()
        .label("Hold Ctrl, Alt or Super and press a key.  Enter saves · Esc cancels")
        .wrap(true)
        .justify(gtk::Justification::Center)
        .build();
    hint.add_css_class("capture-hint");
    let (warn, warn_text) = callout("cmuxp-warning-symbolic");

    let remove = gtk::Button::with_label("Remove this shortcut");
    remove.add_css_class("link-button");
    remove.add_css_class("danger");
    remove.set_halign(gtk::Align::Start);
    remove.set_visible(can_disable && !current.is_empty());

    let body = gtk::Box::new(gtk::Orientation::Vertical, 16);
    body.add_css_class("dialog-body");
    body.append(&head);
    body.append(&stage);
    body.append(&hint);
    body.append(&warn);
    body.append(&remove);
    dialog.set_child(Some(&body));

    let show_caps = Rc::new(move |caps: Vec<String>, ghost: bool| {
        while let Some(c) = stage_inner.first_child() {
            stage_inner.remove(&c);
        }
        if caps.is_empty() {
            let l = gtk::Label::new(Some("Press the new keys…"));
            l.add_css_class("capture-placeholder");
            stage_inner.append(&l);
            return;
        }
        for (i, cap) in caps.iter().enumerate() {
            if i > 0 {
                let plus = gtk::Label::new(Some("+"));
                plus.add_css_class("capture-plus");
                stage_inner.append(&plus);
            }
            let l = gtk::Label::new(Some(cap));
            l.add_css_class("keycap");
            l.add_css_class("big");
            if ghost {
                l.add_css_class("ghost");
            }
            stage_inner.append(&l);
        }
    });
    show_caps(Vec::new(), false);

    let pending: Rc<RefCell<Option<String>>> = Rc::default();
    let on_save = Rc::new(on_save);
    let finish = Rc::new({
        let (dialog, pending, on_save) = (dialog.clone(), pending.clone(), on_save.clone());
        move || {
            if let Some(spec) = pending.borrow().clone() {
                on_save(Some(spec));
                dialog.close();
            }
        }
    });

    let controller = gtk::EventControllerKey::new();
    // Capture phase: see keys before buttons do (Tab would otherwise move focus).
    controller.set_propagation_phase(gtk::PropagationPhase::Capture);
    controller.connect_modifiers({
        let (show_caps, pending) = (show_caps.clone(), pending.clone());
        move |_, state| {
            if pending.borrow().is_none() {
                let mods = state & gtk::accelerator_get_default_mod_mask();
                let caps: Vec<String> = [
                    (gdk::ModifierType::CONTROL_MASK, "Ctrl"),
                    (gdk::ModifierType::ALT_MASK, "Alt"),
                    (gdk::ModifierType::SHIFT_MASK, "Shift"),
                    (gdk::ModifierType::SUPER_MASK, "Super"),
                ]
                .into_iter()
                .filter(|(mask, _)| mods.contains(*mask))
                .map(|(_, n)| n.to_string())
                .collect();
                show_caps(caps, true);
            }
            glib::Propagation::Proceed
        }
    });
    controller.connect_key_pressed({
        let (dialog, save, pending, keymap, warn, warn_text, finish) =
            (dialog.clone(), save.clone(), pending.clone(), keymap.clone(), warn.clone(), warn_text.clone(), finish.clone());
        move |_, key, _code, state| {
            if shortcuts::is_modifier(key) {
                return glib::Propagation::Stop;
            }
            let mods = state & gtk::accelerator_get_default_mod_mask();
            if mods.is_empty() && key == gdk::Key::Escape {
                dialog.close();
                return glib::Propagation::Stop;
            }
            if mods.is_empty() && (key == gdk::Key::Return || key == gdk::Key::KP_Enter) && pending.borrow().is_some() {
                finish();
                return glib::Propagation::Stop;
            }
            let spec = shortcuts::format(key, mods);
            show_caps(shortcuts::keycaps(&spec), false);
            if shortcuts::needs_modifier(key, mods) {
                warn_text.set_text("On its own that key would stop you typing it. Add Ctrl, Alt or Super.");
                warn.set_visible(true);
                save.set_sensitive(false);
                *pending.borrow_mut() = None;
                return glib::Propagation::Stop;
            }
            let owners = keymap.owners_of(&spec, Some(&owner));
            if owners.is_empty() {
                warn.set_visible(false);
            } else {
                let names: Vec<String> = owners.into_iter().map(|(_, n)| format!("“{n}”")).collect();
                warn_text.set_text(&format!("Already used by {}. Saving moves it here.", names.join(", ")));
                warn.set_visible(true);
            }
            save.set_sensitive(true);
            *pending.borrow_mut() = Some(spec);
            glib::Propagation::Stop
        }
    });
    dialog.add_controller(controller);

    save.connect_clicked(move |_| finish());
    remove.connect_clicked({
        let (dialog, on_save) = (dialog.clone(), on_save.clone());
        move |_| {
            on_save(None);
            dialog.close();
        }
    });
    dialog.present();
}

// ---------------------------------------------------------- custom editor

/// Create (`index` = None, optionally from a `preset`) or edit a custom shortcut.
fn edit_custom(ui: &Rc<Ui>, index: Option<usize>, preset: Option<Custom>) {
    let Some(keymap) = ui.keymap.upgrade() else { return };
    let existing = index.and_then(|i| keymap.config.borrow().custom.get(i).cloned()).or(preset);
    let (dialog, save) = dialog(&ui.win, if index.is_some() { "Edit shortcut" } else { "New shortcut" }, 620);

    let field = |label: &str, widget: &gtk::Widget| {
        let l = gtk::Label::builder().label(label).xalign(0.0).build();
        l.add_css_class("field-label");
        let b = gtk::Box::new(gtk::Orientation::Vertical, 8);
        b.append(&l);
        b.append(widget);
        (b, l)
    };

    let name = gtk::Entry::builder().placeholder_text("e.g. Git status").build();
    let (name_field, _) = field("Name", name.upcast_ref());

    // "What should it do?" as choice cards, two per row.
    let choices = gtk::Grid::builder().row_spacing(8).column_spacing(8).column_homogeneous(true).build();
    let mut buttons: Vec<gtk::ToggleButton> = Vec::new();
    for (i, (kind, title, hint, _)) in CUSTOM_KINDS.iter().enumerate() {
        let t = gtk::Label::builder().label(*title).xalign(0.0).build();
        t.add_css_class("choice-title");
        let h = gtk::Label::builder().label(*hint).xalign(0.0).wrap(true).build();
        h.add_css_class("choice-hint");
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.set_valign(gtk::Align::Center);
        text.append(&t);
        text.append(&h);
        let line = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        line.append(&tile(kind.icon()));
        line.append(&text);
        let button = gtk::ToggleButton::builder().child(&line).build();
        button.add_css_class("choice");
        if let Some(first) = buttons.first() {
            button.set_group(Some(first));
        }
        choices.attach(&button, (i % 2) as i32, (i / 2) as i32, 1, 1);
        buttons.push(button);
    }
    let (choices_field, _) = field("What should it do?", choices.upcast_ref());

    let value = gtk::Entry::new();
    value.add_css_class("mono");
    let (value_field, value_label) = field("Command", value.upcast_ref());

    let keys: Rc<RefCell<Vec<String>>> = Rc::default();
    let keys_button = gtk::Button::new();
    keys_button.add_css_class("recorder");
    let (keys_field, _) = field("Keys", keys_button.upcast_ref());
    let set_keys_label = Rc::new({
        let keys_button = keys_button.clone();
        move |keys: &[String]| {
            if keys.is_empty() {
                let l = gtk::Label::new(Some("Click to record keys"));
                l.add_css_class("capture-placeholder");
                keys_button.set_child(Some(&l));
            } else {
                keys_button.set_child(Some(&keycaps_widget(keys)));
            }
        }
    });

    let (error, error_text) = callout("cmuxp-warning-symbolic");

    let body = gtk::Box::new(gtk::Orientation::Vertical, 20);
    body.add_css_class("dialog-body");
    body.append(&name_field);
    body.append(&choices_field);
    body.append(&value_field);
    body.append(&keys_field);
    body.append(&error);
    dialog.set_child(Some(&body));

    // Fill in.
    let selected_kind = Rc::new(Cell::new(CustomKind::Run));
    if let Some(c) = &existing {
        name.set_text(&c.name);
        value.set_text(&c.value);
        selected_kind.set(c.action);
        *keys.borrow_mut() = c.keys.clone();
    }
    set_keys_label(&keys.borrow());
    let update_value_field = Rc::new({
        let (value, value_label) = (value.clone(), value_label.clone());
        move |kind: CustomKind| {
            value_label.set_text(kind.value_label());
            let example = CUSTOM_KINDS.iter().find(|(k, ..)| *k == kind).map(|(.., e)| *e).unwrap_or("");
            value.set_placeholder_text(Some(&format!("e.g. {example}")));
        }
    });
    for (button, (kind, ..)) in buttons.iter().zip(CUSTOM_KINDS) {
        button.set_active(*kind == selected_kind.get());
        let (selected_kind, update, kind) = (selected_kind.clone(), update_value_field.clone(), *kind);
        button.connect_toggled(move |b| {
            if b.is_active() {
                selected_kind.set(kind);
                update(kind);
            }
        });
    }
    update_value_field(selected_kind.get());

    let owner = move || index.map(Owner::Custom).unwrap_or(Owner::Custom(usize::MAX));
    keys_button.connect_clicked({
        let (dialog, keymap, keys, name, selected_kind, set_label) =
            (dialog.clone(), keymap.clone(), keys.clone(), name.clone(), selected_kind.clone(), set_keys_label.clone());
        move |_| {
            let label = if name.text().is_empty() { "your shortcut".to_string() } else { name.text().to_string() };
            let (keys, set_label) = (keys.clone(), set_label.clone());
            let current = keys.borrow().clone();
            capture(&dialog, &label, selected_kind.get().icon(), &keymap, owner(), false, &current, move |spec| {
                if let Some(spec) = spec {
                    *keys.borrow_mut() = vec![spec];
                    set_label(&keys.borrow());
                }
            });
        }
    });

    save.connect_clicked({
        let (dialog, ui, name) = (dialog.clone(), Rc::downgrade(ui), name.clone());
        move |_| {
            let (n, v) = (name.text().trim().to_string(), value.text().trim().to_string());
            if n.is_empty() || v.is_empty() {
                error_text.set_text("Give it a name and fill in what it should run or open.");
                error.set_visible(true);
                return;
            }
            let Some(ui) = ui.upgrade() else { return };
            let Some(keymap) = ui.keymap.upgrade() else { return };
            let custom = Custom { name: n, keys: keys.borrow().clone(), action: selected_kind.get(), value: v };
            for k in &custom.keys {
                keymap.steal(k, Some(&owner()));
            }
            let message = match custom.keys.first() {
                Some(k) => format!("Saved “{}” · {}", custom.name, shortcuts::label(k)),
                None => format!("Saved “{}” (no keys yet)", custom.name),
            };
            {
                let mut config = keymap.config.borrow_mut();
                match index {
                    Some(i) if i < config.custom.len() => config.custom[i] = custom,
                    _ => config.custom.push(custom),
                }
            }
            ui.commit(&keymap, &message);
            dialog.close();
        }
    });
    dialog.present();
    name.grab_focus();
}
