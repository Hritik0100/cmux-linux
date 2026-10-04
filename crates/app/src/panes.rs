//! Pane widgets and split-tree surgery. No app state lives here, only GTK widgets.
//!
//! A workspace's layout is a binary tree made of GTK widgets:
//!   root (gtk::Box)
//!     └─ Paned ─┬─ leaf
//!               └─ Paned ─┬─ leaf
//!                         └─ leaf
//! A leaf is `gtk::Box.pane` → `gtk::Overlay` → `gtk::Notebook` (the pane's
//! tab bar). The overlay draws the drop-zone highlight while a tab is dragged.
//! Each notebook page is one surface (a terminal or a browser).

use gtk::prelude::*;
use gtk::{gdk, glib, pango};
use vte::prelude::*;

/// Notebooks in the same group accept each other's tabs by drag and drop.
const TAB_GROUP: &str = "cmuxp-tabs";

pub fn new_terminal() -> vte::Terminal {
    let term = vte::Terminal::new();
    term.set_hexpand(true);
    term.set_vexpand(true);
    term.set_scrollback_lines(10_000);
    term.set_mouse_autohide(true);
    term.set_font(Some(&pango::FontDescription::from_string("Monospace 11")));
    let fg = gdk::RGBA::parse("#d4d4d8").unwrap();
    let bg = gdk::RGBA::parse("#141518").unwrap();
    term.set_colors(Some(&fg), Some(&bg), &[]);
    term
}

/// Start the user's shell in `term`. `env` entries ("KEY=VALUE") are added to
/// the inherited environment. `on_spawned` receives the shell's pid.
///
/// When `sandbox` is set, the shell runs inside that workspace's network
/// namespace (spawned by the keeper); otherwise the shell is spawned directly.
pub fn spawn_shell(
    term: &vte::Terminal,
    cwd: &str,
    env: &[String],
    sandbox: Option<&crate::sandbox::NetworkSandbox>,
    on_spawned: impl FnOnce(i32) + 'static,
) {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    match sandbox {
        Some(sb) => spawn_shell_sandboxed(term, &shell, cwd, env, sb, on_spawned),
        None => spawn_shell_direct(term, cwd, env, &shell, on_spawned),
    }
}

fn spawn_shell_direct(
    term: &vte::Terminal,
    cwd: &str,
    env: &[String],
    shell: &str,
    on_spawned: impl FnOnce(i32) + 'static,
) {
    let env: Vec<&str> = env.iter().map(String::as_str).collect();
    term.spawn_async(
        vte::PtyFlags::DEFAULT,
        Some(cwd),
        &[shell],
        &env,
        glib::SpawnFlags::DEFAULT,
        || {},
        -1,
        gtk::gio::Cancellable::NONE,
        move |res| match res {
            Ok(pid) => on_spawned(pid.0),
            Err(e) => eprintln!("cmuxp: failed to spawn shell: {e}"),
        },
    );
}

fn spawn_shell_sandboxed(
    term: &vte::Terminal,
    shell: &str,
    cwd: &str,
    env: &[String],
    sandbox: &crate::sandbox::NetworkSandbox,
    on_spawned: impl FnOnce(i32) + 'static,
) {
    let (master, slave) = match crate::sandbox::NetworkSandbox::open_pty() {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("cmuxp: openpty failed: {e}");
            return;
        }
    };
    let pty = match vte::Pty::foreign_sync(master, None::<&gtk::gio::Cancellable>) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cmuxp: foreign pty failed: {e}");
            return;
        }
    };
    term.set_pty(Some(&pty));
    let argv = vec![shell.to_string()];
    match sandbox.spawn(slave, &argv, env, cwd) {
        Ok(pid) => on_spawned(pid),
        Err(e) => eprintln!("cmuxp: sandbox spawn failed: {e}"),
    }
}

/// The notebook page for a terminal: the terminal plus a scrollbar.
pub fn terminal_page(term: &vte::Terminal) -> gtk::Widget {
    gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(term)
        .build()
        .upcast()
}

pub fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let b = gtk::Button::builder().icon_name(icon).tooltip_text(tooltip).build();
    b.add_css_class("flat");
    b.add_css_class("icon-button");
    b
}

/// Widgets of one pane. `state.rs` wires the buttons and drag-and-drop.
pub struct PaneWidgets {
    pub leaf: gtk::Box,
    pub overlay: gtk::Overlay,
    pub notebook: gtk::Notebook,
    pub drop_indicator: gtk::Box,
    pub search: SearchBar,
    pub new_terminal: gtk::Button,
    pub new_browser: gtk::Button,
    pub git: gtk::Button,
    pub split_right: gtk::Button,
    pub split_down: gtk::Button,
}

pub fn new_pane() -> PaneWidgets {
    let notebook = gtk::Notebook::builder()
        .scrollable(true)
        .show_border(false)
        .group_name(TAB_GROUP)
        .hexpand(true)
        .vexpand(true)
        .build();

    let new_terminal = icon_button("cmuxp-terminal-symbolic", "New terminal tab (Ctrl+Shift+T)");
    let new_browser = icon_button("cmuxp-globe-symbolic", "New browser tab (Ctrl+Shift+B)");
    let git = icon_button("cmuxp-git-symbolic", "Git history (Ctrl+Shift+G)");
    let split_right = icon_button("cmuxp-split-right-symbolic", "Split right (Ctrl+Shift+D)");
    let split_down = icon_button("cmuxp-split-down-symbolic", "Split down (Ctrl+Shift+E)");
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 2);
    actions.add_css_class("pane-actions");
    for b in [&new_terminal, &new_browser, &git, &split_right, &split_down] {
        actions.append(b);
    }
    notebook.set_action_widget(&actions, gtk::PackType::End);

    let drop_indicator = gtk::Box::new(gtk::Orientation::Vertical, 0);
    drop_indicator.add_css_class("drop-indicator");
    drop_indicator.set_can_target(false);
    drop_indicator.set_visible(false);

    let search = SearchBar::new();

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&notebook));
    overlay.add_overlay(&drop_indicator);
    overlay.add_overlay(&search.root);

    let leaf = gtk::Box::new(gtk::Orientation::Vertical, 0);
    leaf.add_css_class("pane");
    leaf.set_hexpand(true);
    leaf.set_vexpand(true);
    leaf.append(&overlay);

    PaneWidgets { leaf, overlay, notebook, drop_indicator, search, new_terminal, new_browser, git, split_right, split_down }
}

/// Find-in-scrollback bar, floating at the top right of a pane (Ctrl+Shift+F).
#[derive(Clone)]
pub struct SearchBar {
    pub root: gtk::Box,
    pub entry: gtk::SearchEntry,
    pub regex: gtk::ToggleButton,
    pub older: gtk::Button,
    pub newer: gtk::Button,
    pub close: gtk::Button,
    pub status: gtk::Label,
}

impl SearchBar {
    fn new() -> Self {
        let entry = gtk::SearchEntry::builder().placeholder_text("Find in scrollback").width_chars(24).build();
        let status = gtk::Label::new(None);
        status.add_css_class("search-status");
        let regex = gtk::ToggleButton::with_label(".*");
        regex.set_tooltip_text(Some("Regular expression"));
        regex.add_css_class("flat");
        regex.add_css_class("regex-toggle");
        let older = icon_button("cmuxp-up-symbolic", "Older match (Enter)");
        let newer = icon_button("cmuxp-down-symbolic", "Newer match (Shift+Enter)");
        let close = icon_button("cmuxp-close-symbolic", "Close (Esc)");
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 4);
        root.add_css_class("search-bar");
        root.set_halign(gtk::Align::End);
        root.set_valign(gtk::Align::Start);
        root.set_visible(false);
        for w in [entry.upcast_ref::<gtk::Widget>(), status.upcast_ref(), regex.upcast_ref(), older.upcast_ref(), newer.upcast_ref(), close.upcast_ref()] {
            root.append(w);
        }
        SearchBar { root, entry, regex, older, newer, close, status }
    }
}

/// A tab's label: icon (or spinner while a command runs), title, unread dot, close button.
#[derive(Clone)]
pub struct TabLabel {
    pub widget: gtk::Box,
    pub close: gtk::Button,
    icon: gtk::Image,
    spinner: gtk::Spinner,
    title: gtk::Label,
}

impl TabLabel {
    pub fn new(icon_name: &str, title: &str) -> Self {
        let icon = gtk::Image::from_icon_name(icon_name);
        icon.add_css_class("tab-icon");
        icon.set_pixel_size(14);
        let spinner = gtk::Spinner::new();
        spinner.add_css_class("tab-spinner");
        spinner.set_visible(false);
        let title = gtk::Label::builder()
            .label(title)
            .width_chars(16)
            .max_width_chars(30)
            .ellipsize(pango::EllipsizeMode::End)
            .xalign(0.0)
            .build();
        title.add_css_class("tab-title");
        let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        dot.add_css_class("tab-dot");
        dot.set_valign(gtk::Align::Center);
        let close = icon_button("cmuxp-close-symbolic", "Close tab (Ctrl+Shift+W)");
        close.add_css_class("tab-close");

        let widget = gtk::Box::new(gtk::Orientation::Horizontal, 7);
        widget.add_css_class("tab-label");
        widget.append(&icon);
        widget.append(&spinner);
        widget.append(&title);
        widget.append(&dot);
        widget.append(&close);
        TabLabel { widget, close, icon, spinner, title }
    }

    pub fn set_title(&self, text: &str) {
        self.title.set_text(text);
        self.widget.set_tooltip_text(Some(text));
    }

    #[cfg_attr(not(feature = "browser"), allow(dead_code))]
    pub fn set_favicon(&self, texture: Option<&gdk::Texture>) {
        match texture {
            Some(t) => self.icon.set_paintable(Some(t)),
            None => self.icon.set_icon_name(Some("cmuxp-globe-symbolic")),
        }
    }

    /// Spinner instead of the icon while a command runs.
    pub fn set_busy(&self, busy: bool) {
        self.spinner.set_visible(busy);
        self.spinner.set_spinning(busy);
        self.icon.set_visible(!busy);
    }

    /// Show/hide the blue "needs attention" dot.
    pub fn set_notified(&self, on: bool) {
        if on {
            self.widget.add_css_class("notified");
        } else {
            self.widget.remove_css_class("notified");
        }
    }

    pub fn is_notified(&self) -> bool {
        self.widget.has_css_class("notified")
    }

    /// Carry visual state over to a fresh label (used when a tab moves panes).
    pub fn copy_from(&self, other: &TabLabel) {
        self.set_title(&other.title.text());
        if let Some(p) = other.icon.paintable().filter(|_| other.icon.storage_type() == gtk::ImageType::Paintable) {
            self.icon.set_paintable(Some(&p));
        }
        self.set_busy(other.spinner.is_visible());
        self.set_notified(other.is_notified());
    }
}

/// Append a page to a pane's notebook and show it.
pub fn add_tab(notebook: &gtk::Notebook, page: &gtk::Widget, label: &TabLabel) {
    let index = notebook.append_page(page, Some(&label.widget));
    notebook.set_tab_reorderable(page, true);
    notebook.set_tab_detachable(page, true);
    notebook.set_current_page(Some(index));
}

// ------------------------------------------------------------- drop zones

/// Where a dragged tab would land inside a pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DropZone {
    /// Join this pane's tabs.
    Center,
    /// Split this pane and put the tab on that side.
    Left,
    Right,
    Top,
    Bottom,
}

/// Outer 25% bands are split zones; the middle joins the pane.
pub fn zone_at(width: f64, height: f64, x: f64, y: f64) -> DropZone {
    let fx = x / width.max(1.0);
    let fy = y / height.max(1.0);
    let candidates = [(fx, DropZone::Left), (1.0 - fx, DropZone::Right), (fy, DropZone::Top), (1.0 - fy, DropZone::Bottom)];
    let (dist, zone) = candidates.into_iter().fold((f64::MAX, DropZone::Center), |best, c| if c.0 < best.0 { c } else { best });
    if dist < 0.25 {
        zone
    } else {
        DropZone::Center
    }
}

/// Highlight the area the tab would occupy, or hide the highlight.
pub fn show_drop_zone(indicator: &gtk::Box, zone: Option<DropZone>, width: i32, height: i32) {
    let Some(zone) = zone else {
        indicator.set_visible(false);
        return;
    };
    use gtk::Align::{End, Fill, Start};
    let (h, v, w, hh) = match zone {
        DropZone::Center => (Fill, Fill, -1, -1),
        DropZone::Left => (Start, Fill, width / 2, -1),
        DropZone::Right => (End, Fill, width / 2, -1),
        DropZone::Top => (Fill, Start, -1, height / 2),
        DropZone::Bottom => (Fill, End, -1, height / 2),
    };
    indicator.set_halign(h);
    indicator.set_valign(v);
    indicator.set_size_request(w, hh);
    indicator.set_visible(true);
}

// ------------------------------------------------------------- split tree

/// Replace `old` in the tree with a Paned holding `old` and `new`.
/// `new_first` puts `new` on the left/top instead of the right/bottom.
pub fn split(old: &gtk::Widget, new: &gtk::Widget, orientation: gtk::Orientation, new_first: bool) -> gtk::Paned {
    split_at(old, new, orientation, new_first, 0.5)
}

/// Like `split`, with the divider at `ratio` (0..1) of the space (used by session restore).
pub fn split_at(old: &gtk::Widget, new: &gtk::Widget, orientation: gtk::Orientation, new_first: bool, ratio: f64) -> gtk::Paned {
    let paned = gtk::Paned::builder()
        .orientation(orientation)
        .wide_handle(true)
        .resize_start_child(true)
        .resize_end_child(true)
        .build();
    // `old` must be detached before it can be put into the new Paned.
    let slot = detach(old);
    let (a, b) = if new_first { (new, old) } else { (old, new) };
    paned.set_start_child(Some(a));
    paned.set_end_child(Some(b));
    attach(&slot, paned.upcast_ref());
    set_ratio_when_sized(&paned, ratio);
    paned
}

/// Put the divider at `ratio` once GTK has given the Paned a size. A tick
/// callback only runs while the widget is on screen, so panes restored into a
/// hidden workspace wait (for free) until that workspace is first shown.
fn set_ratio_when_sized(paned: &gtk::Paned, ratio: f64) {
    let ratio = ratio.clamp(0.05, 0.95);
    paned.add_tick_callback(move |paned, _| {
        let size = paned_size(paned);
        if size > 0 {
            paned.set_position((size as f64 * ratio).round() as i32);
            glib::ControlFlow::Break
        } else {
            glib::ControlFlow::Continue
        }
    });
}

/// Width (horizontal split) or height (vertical split) of a Paned.
pub fn paned_size(paned: &gtk::Paned) -> i32 {
    match paned.orientation() {
        gtk::Orientation::Horizontal => paned.width(),
        _ => paned.height(),
    }
}

pub enum Removed {
    /// The sibling that took the removed leaf's place.
    Sibling(gtk::Widget),
    /// The leaf was the last pane in its tree.
    Empty,
}

/// Remove a leaf from the tree; its sibling is promoted into the parent's slot.
pub fn remove(leaf: &gtk::Widget) -> Removed {
    let Some(parent) = leaf.parent() else { return Removed::Empty };
    let Some(paned) = parent.downcast_ref::<gtk::Paned>() else {
        detach(leaf);
        return Removed::Empty;
    };
    let is_start = paned.start_child().as_ref() == Some(leaf);
    let sibling = if is_start { paned.end_child() } else { paned.start_child() };
    let slot = detach(paned.upcast_ref());
    paned.set_start_child(None::<&gtk::Widget>);
    paned.set_end_child(None::<&gtk::Widget>);
    match sibling {
        Some(sibling) => {
            attach(&slot, &sibling);
            Removed::Sibling(sibling)
        }
        None => Removed::Empty,
    }
}

/// All leaves under `widget`, in reading order (left→right, top→bottom).
pub fn leaves(widget: &gtk::Widget) -> Vec<gtk::Widget> {
    if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
        let mut out = Vec::new();
        out.extend(paned.start_child().map(|w| leaves(&w)).unwrap_or_default());
        out.extend(paned.end_child().map(|w| leaves(&w)).unwrap_or_default());
        out
    } else if widget.has_css_class("pane") {
        vec![widget.clone()]
    } else {
        // A root box: descend into its single child.
        widget.first_child().map(|w| leaves(&w)).unwrap_or_default()
    }
}

/// Where a widget sat in the tree, so something else can be put back there.
enum Slot {
    Start(gtk::Paned),
    End(gtk::Paned),
    Box(gtk::Box),
    None,
}

fn detach(widget: &gtk::Widget) -> Slot {
    let Some(parent) = widget.parent() else { return Slot::None };
    if let Some(paned) = parent.downcast_ref::<gtk::Paned>() {
        if paned.start_child().as_ref() == Some(widget) {
            paned.set_start_child(None::<&gtk::Widget>);
            Slot::Start(paned.clone())
        } else {
            paned.set_end_child(None::<&gtk::Widget>);
            Slot::End(paned.clone())
        }
    } else if let Some(b) = parent.downcast_ref::<gtk::Box>() {
        b.remove(widget);
        Slot::Box(b.clone())
    } else {
        widget.unparent();
        Slot::None
    }
}

fn attach(slot: &Slot, widget: &gtk::Widget) {
    match slot {
        Slot::Start(p) => p.set_start_child(Some(widget)),
        Slot::End(p) => p.set_end_child(Some(widget)),
        Slot::Box(b) => b.append(widget),
        Slot::None => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_zones() {
        assert_eq!(zone_at(400.0, 300.0, 200.0, 150.0), DropZone::Center);
        assert_eq!(zone_at(400.0, 300.0, 10.0, 150.0), DropZone::Left);
        assert_eq!(zone_at(400.0, 300.0, 390.0, 150.0), DropZone::Right);
        assert_eq!(zone_at(400.0, 300.0, 200.0, 5.0), DropZone::Top);
        assert_eq!(zone_at(400.0, 300.0, 200.0, 295.0), DropZone::Bottom);
    }
}
