//! User-configurable keyboard shortcuts.
//!
//! Every shortcut is a `gio` action on the application (`app.split-right`,
//! ...). This module decides which keys trigger which action:
//!
//!   built-in defaults  ←  overridden by  ←  ~/.config/cmuxp/shortcuts.toml
//!
//! The file also holds *custom* shortcuts the user creates ("Ctrl+Alt+G types
//! `git status`"). It is written by the Keyboard Shortcuts window and reloaded
//! automatically when edited by hand.
//!
//! Key syntax in the file is human friendly: `ctrl+shift+d`, `alt+1`,
//! `ctrl+pagedown`, `super+]`. Internally GTK wants `<Control><Shift>d`.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::{Rc, Weak};

use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use serde::{Deserialize, Serialize};

use crate::state::{self, AppState, Content};

/// A built-in action that can be rebound.
pub struct Builtin {
    pub action: &'static str,
    pub label: &'static str,
    pub description: &'static str,
    pub icon: &'static str,
    pub group: &'static str,
    pub defaults: &'static [&'static str],
}

pub const GROUPS: &[&str] = &["Workspaces", "Tabs and panes", "Window", "Terminal"];

const WS: &str = "Workspaces";
const TABS: &str = "Tabs and panes";
const WIN: &str = "Window";
const TERM: &str = "Terminal";

const fn b(
    action: &'static str,
    label: &'static str,
    description: &'static str,
    icon: &'static str,
    group: &'static str,
    defaults: &'static [&'static str],
) -> Builtin {
    Builtin { action, label, description, icon, group, defaults }
}

pub const BUILTINS: &[Builtin] = &[
    b("new-workspace", "New workspace", "Add a workspace with a fresh terminal", "cmuxp-plus-symbolic", WS, &["ctrl+shift+n"]),
    b("next-workspace", "Next workspace", "Move down the sidebar", "cmuxp-forward-symbolic", WS, &["ctrl+pagedown"]),
    b("prev-workspace", "Previous workspace", "Move up the sidebar", "cmuxp-back-symbolic", WS, &["ctrl+pageup"]),
    b("workspace-1", "Go to workspace 1", "Jump straight to the 1st workspace", "cmuxp-folder-symbolic", WS, &["alt+1"]),
    b("workspace-2", "Go to workspace 2", "Jump straight to the 2nd workspace", "cmuxp-folder-symbolic", WS, &["alt+2"]),
    b("workspace-3", "Go to workspace 3", "Jump straight to the 3rd workspace", "cmuxp-folder-symbolic", WS, &["alt+3"]),
    b("workspace-4", "Go to workspace 4", "Jump straight to the 4th workspace", "cmuxp-folder-symbolic", WS, &["alt+4"]),
    b("workspace-5", "Go to workspace 5", "Jump straight to the 5th workspace", "cmuxp-folder-symbolic", WS, &["alt+5"]),
    b("workspace-6", "Go to workspace 6", "Jump straight to the 6th workspace", "cmuxp-folder-symbolic", WS, &["alt+6"]),
    b("workspace-7", "Go to workspace 7", "Jump straight to the 7th workspace", "cmuxp-folder-symbolic", WS, &["alt+7"]),
    b("workspace-8", "Go to workspace 8", "Jump straight to the 8th workspace", "cmuxp-folder-symbolic", WS, &["alt+8"]),
    b("workspace-9", "Go to workspace 9", "Jump straight to the 9th workspace", "cmuxp-folder-symbolic", WS, &["alt+9"]),
    b("new-tab", "New terminal tab", "Open a shell next to the current tab", "cmuxp-terminal-symbolic", TABS, &["ctrl+shift+t"]),
    b("browser-tab", "New browser tab", "Open the built-in browser on Google", "cmuxp-globe-symbolic", TABS, &["ctrl+shift+b"]),
    b("browser-split", "Browser in a split", "Open the browser to the right", "cmuxp-globe-symbolic", TABS, &["ctrl+shift+alt+b"]),
    b("close-tab", "Close tab", "Close the focused terminal or browser", "cmuxp-close-symbolic", TABS, &["ctrl+shift+w"]),
    b("next-tab", "Next tab", "Cycle forward through this pane's tabs", "cmuxp-forward-symbolic", TABS, &["ctrl+tab"]),
    b("prev-tab", "Previous tab", "Cycle backward through this pane's tabs", "cmuxp-back-symbolic", TABS, &["ctrl+shift+tab"]),
    b("git-history", "Git history", "Show this project's commits and changes", "cmuxp-git-symbolic", TABS, &["ctrl+shift+g"]),
    b("split-right", "Split right", "Open a new pane to the right", "cmuxp-split-right-symbolic", TABS, &["ctrl+shift+d"]),
    b("split-down", "Split down", "Open a new pane below", "cmuxp-split-down-symbolic", TABS, &["ctrl+shift+e"]),
    b("next-pane", "Next pane", "Move focus to the next pane", "cmuxp-split-right-symbolic", TABS, &["ctrl+shift+]"]),
    b("search", "Search everything", "Files, commands, tabs, shell history and commits", "cmuxp-search-symbolic", WIN, &["ctrl+shift+space"]),
    b("command-palette", "Command palette", "Search every command, workspace and tab", "cmuxp-search-symbolic", WIN, &["ctrl+shift+p"]),
    b("show-shortcuts", "Keyboard shortcuts", "Open this window", "cmuxp-keyboard-symbolic", WIN, &["ctrl+comma", "ctrl+question"]),
    b("notifications", "Notification center", "Show recent notifications", "cmuxp-bell-symbolic", WIN, &["ctrl+shift+i"]),
    b("jump-unread", "Jump to unread", "Go to the tab that needs you", "cmuxp-bell-symbolic", WIN, &["ctrl+shift+u"]),
    b("toggle-sidebar", "Show/hide sidebar", "More room for your panes", "cmuxp-sidebar-symbolic", WIN, &["ctrl+shift+s"]),
    b("find", "Find", "Search the scrollback (or the page in a browser tab)", "cmuxp-search-symbolic", TERM, &["ctrl+shift+f"]),
    b("copy", "Copy", "Copy the selected terminal text", "cmuxp-copy-symbolic", TERM, &["ctrl+shift+c"]),
    b("paste", "Paste", "Paste into the terminal", "cmuxp-paste-symbolic", TERM, &["ctrl+shift+v"]),
];

/// What a custom shortcut does.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CustomKind {
    /// Type `value` into the focused terminal (TOML escapes like `\n` work).
    Type,
    /// Type `value` and press Enter in the focused terminal.
    Run,
    /// Open a new terminal tab next to the focused one and run `value` there.
    RunInNewTab,
    /// Open `value` (URL, `localhost:3000`, or search words) in a browser tab.
    Browser,
    /// Open folder `value` as a new workspace named after the shortcut.
    Workspace,
}

/// (kind, title, hint, example value)
pub const CUSTOM_KINDS: &[(CustomKind, &str, &str, &str)] = &[
    (CustomKind::Run, "Run a command", "In the terminal you're using", "git status"),
    (CustomKind::RunInNewTab, "Run in a new tab", "Opens a tab next to yours first", "claude"),
    (CustomKind::Type, "Type text", "Types it without pressing Enter", "git commit -m \""),
    (CustomKind::Browser, "Open in browser", "A URL, localhost:3000, or a search", "localhost:3000"),
    (CustomKind::Workspace, "Open a folder", "As a new workspace in the sidebar", "~/code/api"),
];

impl CustomKind {
    pub fn icon(self) -> &'static str {
        match self {
            CustomKind::Type => "cmuxp-keyboard-symbolic",
            CustomKind::Run | CustomKind::RunInNewTab => "cmuxp-terminal-symbolic",
            CustomKind::Browser => "cmuxp-globe-symbolic",
            CustomKind::Workspace => "cmuxp-folder-symbolic",
        }
    }

    /// Label for the value field in the editor.
    pub fn value_label(self) -> &'static str {
        match self {
            CustomKind::Type => "Text to type",
            CustomKind::Run | CustomKind::RunInNewTab => "Command",
            CustomKind::Browser => "URL or search",
            CustomKind::Workspace => "Folder",
        }
    }

    pub fn describe(self, value: &str) -> String {
        match self {
            CustomKind::Type => format!("Types “{value}”"),
            CustomKind::Run => format!("Runs `{value}`"),
            CustomKind::RunInNewTab => format!("Runs `{value}` in a new tab"),
            CustomKind::Browser => format!("Opens {value}"),
            CustomKind::Workspace => format!("Opens {value} as a workspace"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Custom {
    pub name: String,
    #[serde(default)]
    pub keys: Vec<String>,
    pub action: CustomKind,
    pub value: String,
}

/// Contents of shortcuts.toml.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Built-in action → keys. Only changed actions are stored; `[]` disables one.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, Vec<String>>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub custom: Vec<Custom>,
}

// ----------------------------------------------------------------- key syntax

/// Friendly names users may type, mapped to GDK key names.
const ALIASES: &[(&str, &str)] = &[
    ("]", "bracketright"),
    ("[", "bracketleft"),
    ("\\", "backslash"),
    ("/", "slash"),
    (",", "comma"),
    (".", "period"),
    (";", "semicolon"),
    ("'", "apostrophe"),
    ("`", "grave"),
    ("-", "minus"),
    ("=", "equal"),
    ("pgup", "Page_Up"),
    ("pageup", "Page_Up"),
    ("pgdn", "Page_Down"),
    ("pagedown", "Page_Down"),
    ("esc", "Escape"),
    ("escape", "Escape"),
    ("enter", "Return"),
    ("return", "Return"),
    ("space", "space"),
    ("tab", "Tab"),
    ("iso_left_tab", "ISO_Left_Tab"),
    ("backspace", "BackSpace"),
    ("del", "Delete"),
    ("delete", "Delete"),
    ("ins", "Insert"),
    ("insert", "Insert"),
    ("home", "Home"),
    ("end", "End"),
    ("up", "Up"),
    ("down", "Down"),
    ("left", "Left"),
    ("right", "Right"),
];

fn key_from_name(name: &str) -> Option<gdk::Key> {
    let lower = name.to_ascii_lowercase();
    if let Some((_, gdk_name)) = ALIASES.iter().find(|(alias, _)| *alias == lower) {
        return gdk::Key::from_name(*gdk_name);
    }
    // f1..f24
    if lower.len() >= 2 && lower.starts_with('f') && lower[1..].chars().all(|c| c.is_ascii_digit()) {
        return gdk::Key::from_name(lower.to_ascii_uppercase());
    }
    gdk::Key::from_name(name).or_else(|| gdk::Key::from_name(&lower))
}

/// "ctrl+shift+d" → (key, modifiers). None if it isn't a valid key combo.
pub fn parse(spec: &str) -> Option<(gdk::Key, gdk::ModifierType)> {
    let spec = spec.trim();
    // "ctrl++" means Ctrl and the plus key.
    let (mods_part, key_part) = match spec.strip_suffix("++") {
        Some(rest) => (rest, "plus"),
        None => match spec.rsplit_once('+') {
            Some((m, k)) => (m, k),
            None => ("", spec),
        },
    };
    let mut mods = gdk::ModifierType::empty();
    for m in mods_part.split('+').filter(|m| !m.is_empty()) {
        mods |= match m.trim().to_ascii_lowercase().as_str() {
            "ctrl" | "control" => gdk::ModifierType::CONTROL_MASK,
            "shift" => gdk::ModifierType::SHIFT_MASK,
            "alt" | "option" => gdk::ModifierType::ALT_MASK,
            "super" | "win" | "meta" | "cmd" => gdk::ModifierType::SUPER_MASK,
            _ => return None,
        };
    }
    let key = key_from_name(key_part.trim())?;
    Some((key.to_lower(), mods))
}

/// "ctrl+shift+d" → "<Control><Shift>d" for `set_accels_for_action`.
pub fn to_accel(spec: &str) -> Option<String> {
    let (key, mods) = parse(spec)?;
    Some(gtk::accelerator_name(key, mods).to_string())
}

/// Turn a key press into our friendly syntax: (Ctrl+Shift, D) → "ctrl+shift+d".
pub fn format(key: gdk::Key, mods: gdk::ModifierType) -> String {
    let mut out = String::new();
    for (mask, name) in [
        (gdk::ModifierType::CONTROL_MASK, "ctrl+"),
        (gdk::ModifierType::SHIFT_MASK, "shift+"),
        (gdk::ModifierType::ALT_MASK, "alt+"),
        (gdk::ModifierType::SUPER_MASK, "super+"),
    ] {
        if mods.contains(mask) {
            out.push_str(name);
        }
    }
    let name = key.to_lower().name().map(|n| n.to_string()).unwrap_or_default();
    let friendly = ALIASES.iter().find(|(_, gdk_name)| *gdk_name == name).map(|(alias, _)| alias.to_string());
    out.push_str(&friendly.unwrap_or(name).to_ascii_lowercase());
    out
}

/// Human label, e.g. "Shift+Ctrl+D", using GTK's own formatting.
pub fn label(spec: &str) -> String {
    match parse(spec) {
        Some((key, mods)) => gtk::accelerator_get_label(key, mods).to_string(),
        None => format!("{spec} (invalid)"),
    }
}

/// The parts to draw as separate keycaps: "ctrl+shift+d" → ["Ctrl", "Shift", "D"].
pub fn keycaps(spec: &str) -> Vec<String> {
    let Some((key, mods)) = parse(spec) else { return vec![spec.to_string()] };
    let mut caps = Vec::new();
    for (mask, name) in [
        (gdk::ModifierType::CONTROL_MASK, "Ctrl"),
        (gdk::ModifierType::ALT_MASK, "Alt"),
        (gdk::ModifierType::SHIFT_MASK, "Shift"),
        (gdk::ModifierType::SUPER_MASK, "Super"),
    ] {
        if mods.contains(mask) {
            caps.push(name.to_string());
        }
    }
    caps.push(gtk::accelerator_get_label(key, gdk::ModifierType::empty()).to_string());
    caps
}

/// Two specs mean the same keys? ("ctrl+shift+D" == "shift+ctrl+d")
pub fn same(a: &str, b: &str) -> bool {
    matches!((parse(a), parse(b)), (Some(x), Some(y)) if x == y)
}

/// Is this key, on its own, only a modifier (Shift, Ctrl, ...)?
pub fn is_modifier(key: gdk::Key) -> bool {
    let name = key.name().map(|n| n.to_string()).unwrap_or_default();
    ["Shift", "Control", "Alt", "Super", "Meta", "Hyper", "ISO_Level", "Caps_Lock", "Num_Lock"]
        .iter()
        .any(|p| name.starts_with(p))
}

/// Would this combo break normal typing? Plain keys and Shift+key are refused;
/// function keys are fine on their own.
pub fn needs_modifier(key: gdk::Key, mods: gdk::ModifierType) -> bool {
    let name = key.name().map(|n| n.to_string()).unwrap_or_default();
    let is_fkey = name.len() >= 2 && name.starts_with('F') && name[1..].chars().all(|c| c.is_ascii_digit());
    let strong = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK | gdk::ModifierType::SUPER_MASK;
    !is_fkey && !mods.intersects(strong)
}

// ---------------------------------------------------------------------- keymap

/// Who currently owns a key combo (for conflict warnings).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Builtin(&'static str),
    Custom(usize),
}

pub struct Keymap {
    app: gtk::Application,
    shared: Weak<RefCell<AppState>>,
    path: PathBuf,
    pub config: RefCell<Config>,
    /// Number of `app.custom-N` actions currently registered.
    registered_custom: Cell<usize>,
    /// Called after every change so open windows can redraw.
    listeners: RefCell<Vec<Box<dyn Fn()>>>,
    _monitor: RefCell<Option<gio::FileMonitor>>,
}

pub fn default_path() -> PathBuf {
    glib::user_config_dir().join("cmuxp").join("shortcuts.toml")
}

impl Keymap {
    pub fn new(app: &gtk::Application, shared: Weak<RefCell<AppState>>) -> Rc<Keymap> {
        let keymap = Rc::new(Keymap {
            app: app.clone(),
            shared,
            path: default_path(),
            config: RefCell::new(Config::default()),
            registered_custom: Cell::new(0),
            listeners: RefCell::new(Vec::new()),
            _monitor: RefCell::new(None),
        });
        // The folder must exist for the file monitor to get instant change events.
        if let Some(dir) = keymap.path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        keymap.reload();
        keymap.watch_file();
        keymap
    }

    pub fn path(&self) -> &PathBuf {
        &self.path
    }

    /// Keys currently assigned to a built-in action.
    pub fn keys_for(&self, action: &str) -> Vec<String> {
        if let Some(keys) = self.config.borrow().bindings.get(action) {
            return keys.clone();
        }
        BUILTINS.iter().find(|b| b.action == action).map(|b| b.defaults.iter().map(|s| s.to_string()).collect()).unwrap_or_default()
    }

    /// False for actions missing from this build (e.g. browser ones without the feature).
    pub fn is_available(&self, action: &str) -> bool {
        self.app.lookup_action(action).is_some()
    }

    pub fn is_default(&self, action: &str) -> bool {
        !self.config.borrow().bindings.contains_key(action)
    }

    /// Everything that currently uses `spec`, except `except`.
    pub fn owners_of(&self, spec: &str, except: Option<&Owner>) -> Vec<(Owner, String)> {
        let mut out = Vec::new();
        for b in BUILTINS {
            if matches!(except, Some(Owner::Builtin(a)) if *a == b.action) {
                continue;
            }
            if self.keys_for(b.action).iter().any(|k| same(k, spec)) {
                out.push((Owner::Builtin(b.action), b.label.to_string()));
            }
        }
        for (i, c) in self.config.borrow().custom.iter().enumerate() {
            if matches!(except, Some(Owner::Custom(j)) if *j == i) {
                continue;
            }
            if c.keys.iter().any(|k| same(k, spec)) {
                out.push((Owner::Custom(i), c.name.clone()));
            }
        }
        out
    }

    /// Take `spec` away from everything else (used when the user reassigns it).
    pub fn steal(&self, spec: &str, except: Option<&Owner>) {
        for (owner, _) in self.owners_of(spec, except) {
            match owner {
                Owner::Builtin(action) => {
                    let keys: Vec<String> = self.keys_for(action).into_iter().filter(|k| !same(k, spec)).collect();
                    self.config.borrow_mut().bindings.insert(action.to_string(), keys);
                }
                Owner::Custom(i) => {
                    if let Some(c) = self.config.borrow_mut().custom.get_mut(i) {
                        c.keys.retain(|k| !same(k, spec));
                    }
                }
            }
        }
    }

    pub fn set_builtin(&self, action: &str, keys: Vec<String>) {
        let defaults: Vec<String> =
            BUILTINS.iter().find(|b| b.action == action).map(|b| b.defaults.iter().map(|s| s.to_string()).collect()).unwrap_or_default();
        let mut config = self.config.borrow_mut();
        if keys == defaults {
            config.bindings.remove(action);
        } else {
            config.bindings.insert(action.to_string(), keys);
        }
    }

    pub fn reset_builtin(&self, action: &str) {
        self.config.borrow_mut().bindings.remove(action);
    }

    pub fn reset_all(&self) {
        self.config.borrow_mut().bindings.clear();
    }

    /// Re-read the file and apply it. A broken file keeps the previous shortcuts.
    pub fn reload(&self) {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(config) => *self.config.borrow_mut() = config,
                Err(e) => eprintln!("cmuxp: ignoring {}: {e}", self.path.display()),
            },
            Err(_) => *self.config.borrow_mut() = Config::default(),
        }
        self.apply();
    }

    /// Save to disk and apply. Call after any change.
    pub fn commit(&self) {
        if let Err(e) = self.save() {
            eprintln!("cmuxp: cannot save {}: {e}", self.path.display());
        }
        self.apply();
    }

    fn save(&self) -> std::io::Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = toml::to_string_pretty(&*self.config.borrow()).map_err(std::io::Error::other)?;
        let mut text = String::from(HEADER);
        for b in BUILTINS {
            text.push_str(&format!("#   {:<16} {:<28} default: {}\n", b.action, b.label, b.defaults.join(", ")));
        }
        text.push('\n');
        text.push_str(&body);
        // Skip identical writes so the file monitor doesn't loop.
        if std::fs::read_to_string(&self.path).ok().as_deref() != Some(text.as_str()) {
            std::fs::write(&self.path, text)?;
        }
        Ok(())
    }

    /// Push the current config into GTK: accelerators and custom actions.
    pub fn apply(&self) {
        for b in BUILTINS {
            if self.app.lookup_action(b.action).is_none() {
                continue;
            }
            let accels: Vec<String> = self.keys_for(b.action).iter().filter_map(|k| to_accel(k)).collect();
            let refs: Vec<&str> = accels.iter().map(String::as_str).collect();
            self.app.set_accels_for_action(&format!("app.{}", b.action), &refs);
        }

        for i in 0..self.registered_custom.get() {
            self.app.set_accels_for_action(&format!("app.custom-{i}"), &[]);
            self.app.remove_action(&format!("custom-{i}"));
        }
        let customs = self.config.borrow().custom.clone();
        for (i, c) in customs.iter().enumerate() {
            let action = gio::SimpleAction::new(&format!("custom-{i}"), None);
            let (shared, c2) = (self.shared.clone(), c.clone());
            action.connect_activate(move |_, _| {
                if let Some(s) = shared.upgrade() {
                    run_custom(&s, &c2);
                }
            });
            self.app.add_action(&action);
            let accels: Vec<String> = c.keys.iter().filter_map(|k| to_accel(k)).collect();
            let refs: Vec<&str> = accels.iter().map(String::as_str).collect();
            self.app.set_accels_for_action(&format!("app.custom-{i}"), &refs);
        }
        self.registered_custom.set(customs.len());

        for listener in self.listeners.borrow().iter() {
            listener();
        }
    }

    pub fn on_change(&self, f: impl Fn() + 'static) {
        self.listeners.borrow_mut().push(Box::new(f));
    }

    /// Hand-edits to shortcuts.toml take effect without restarting.
    fn watch_file(self: &Rc<Self>) {
        let file = gio::File::for_path(&self.path);
        let Ok(monitor) = file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) else { return };
        let weak = Rc::downgrade(self);
        monitor.connect_changed(move |_, _, _, event| {
            if matches!(event, gio::FileMonitorEvent::ChangesDoneHint | gio::FileMonitorEvent::Created | gio::FileMonitorEvent::Deleted) {
                if let Some(keymap) = weak.upgrade() {
                    let before = keymap.config.borrow().clone();
                    keymap.reload();
                    if *keymap.config.borrow() != before {
                        eprintln!("cmuxp: reloaded {}", keymap.path.display());
                    }
                }
            }
        });
        *self._monitor.borrow_mut() = Some(monitor);
    }

    fn accels(&self, detailed_action: &str) -> Vec<String> {
        self.app.accels_for_action(detailed_action).iter().map(|a| a.to_string()).collect()
    }

    /// For `cmuxpctl shortcuts`.
    pub fn describe(&self) -> serde_json::Value {
        let builtins: Vec<_> = BUILTINS
            .iter()
            .filter(|b| self.app.lookup_action(b.action).is_some())
            .map(|b| {
                serde_json::json!({
                    "action": b.action, "label": b.label, "group": b.group,
                    "keys": self.keys_for(b.action), "default": self.is_default(b.action),
                    // What GTK actually registered, e.g. "<Control><Shift>d".
                    "accels": self.accels(&format!("app.{}", b.action)),
                })
            })
            .collect();
        let custom: Vec<_> = self
            .config
            .borrow()
            .custom
            .iter()
            .enumerate()
            .map(|(i, c)| {
                serde_json::json!({
                    "id": format!("custom-{i}"), "name": c.name, "keys": c.keys, "action": c.action, "value": c.value,
                    "accels": self.accels(&format!("app.custom-{i}")),
                })
            })
            .collect();
        serde_json::json!({ "file": self.path, "builtin": builtins, "custom": custom })
    }
}

const HEADER: &str = "\
# cmuxp keyboard shortcuts
# Edit here or in the app (the ? button, or Ctrl+,). Changes apply as soon as you save.
#
# Key syntax: ctrl, shift, alt, super + a key, joined with '+':
#   \"ctrl+shift+d\", \"alt+1\", \"ctrl+pagedown\", \"super+]\", \"f5\"
#
# Change a built-in shortcut (several keys allowed; [] disables it):
#   [bindings]
#   split-right = [\"ctrl+shift+d\", \"alt+backslash\"]
#
# Add your own:
#   [[custom]]
#   name = \"Git status\"
#   keys = [\"ctrl+alt+g\"]
#   action = \"run\"            # run | run-in-new-tab | type | browser | workspace
#   value = \"git status\"
#
# Built-in actions:
";

/// Perform a custom shortcut.
pub fn run_custom(shared: &state::Shared, c: &Custom) {
    let result = match c.action {
        CustomKind::Type => state::send_text(shared, None, &c.value),
        CustomKind::Run => state::send_text(shared, None, &format!("{}\n", c.value)),
        CustomKind::RunInNewTab => {
            state::new_tab(shared, None, Content::Terminal).and_then(|sid| state::send_text(shared, Some(sid), &format!("{}\n", c.value)))
        }
        CustomKind::Browser => state::open_browser(shared, None, &c.value, None).map(|_| ()),
        CustomKind::Workspace => {
            let home = std::env::var("HOME").unwrap_or_default();
            let dir = match c.value.strip_prefix('~') {
                Some(rest) => format!("{home}{rest}"),
                None => c.value.clone(),
            };
            state::new_workspace(shared, Some(dir), Some(c.name.clone()));
            Ok(())
        }
    };
    if let Err(e) = result {
        eprintln!("cmuxp: shortcut “{}” failed: {e}", c.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_syntax() {
        let (k, m) = parse("ctrl+shift+d").unwrap();
        assert_eq!(k, gdk::Key::d);
        assert_eq!(m, gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::SHIFT_MASK);
        // Case and modifier order don't matter; aliases work.
        assert!(same("Shift+CTRL+D", "ctrl+shift+d"));
        assert_eq!(parse("ctrl+]").unwrap().0, gdk::Key::bracketright);
        assert_eq!(parse("ctrl+pagedown").unwrap().0, gdk::Key::Page_Down);
        assert_eq!(parse("f5").unwrap().0, gdk::Key::F5);
        assert_eq!(parse("ctrl++").unwrap().0, gdk::Key::plus);
        assert!(parse("hyper+x").is_none());
        assert!(parse("ctrl+nosuchkey").is_none());
        // What we write back is readable and parses to the same thing.
        let m = gdk::ModifierType::CONTROL_MASK | gdk::ModifierType::ALT_MASK;
        assert_eq!(format(gdk::Key::bracketright, m), "ctrl+alt+]");
        assert_eq!(format(gdk::Key::G, gdk::ModifierType::CONTROL_MASK), "ctrl+g");
        // Typing keys need a real modifier; F-keys don't.
        assert!(needs_modifier(gdk::Key::a, gdk::ModifierType::SHIFT_MASK));
        assert!(!needs_modifier(gdk::Key::F5, gdk::ModifierType::empty()));
        // Every default parses.
        for b in BUILTINS {
            for k in b.defaults {
                assert!(parse(k).is_some(), "bad default {k} for {}", b.action);
            }
        }
    }

    #[test]
    fn config_roundtrip() {
        let text = r#"
            [bindings]
            split-right = ["ctrl+shift+d", "alt+backslash"]
            close-tab = []

            [[custom]]
            name = "Git status"
            keys = ["ctrl+alt+g"]
            action = "run"
            value = "git status"

            [[custom]]
            name = "API"
            action = "workspace"
            value = "~/code/api"
        "#;
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config.bindings["close-tab"], Vec::<String>::new());
        assert_eq!(config.custom[0].action, CustomKind::Run);
        assert!(config.custom[1].keys.is_empty());
        let again: Config = toml::from_str(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(again, config);
    }
}
