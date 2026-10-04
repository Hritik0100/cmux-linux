//! Session restore: reopen cmuxp the way you left it.
//!
//! Saved to `$XDG_STATE_HOME/cmuxp/session.json` (usually
//! `~/.local/state/cmuxp/session.json`) every few seconds when something
//! changed, and when the window closes. On start the workspaces, split
//! layout (with divider positions), tabs, their folders and browser URLs are
//! rebuilt. Shells start fresh in the saved folder; running programs and
//! scrollback are not restored. Floating windows aren't saved.
//!
//! Start with `CMUXP_FRESH=1 cmuxp` to ignore the saved session once.

use std::path::PathBuf;

use gtk::prelude::*;
use serde::{Deserialize, Serialize};

use crate::panes;
use crate::state::{self, AppState, Shared, SurfaceKind};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    /// Index of the workspace that was showing.
    pub active: usize,
    pub workspaces: Vec<SavedWorkspace>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    /// Only set when the user named it; otherwise it follows the terminal title again.
    #[serde(default)]
    pub title: Option<String>,
    pub cwd: String,
    pub layout: Node,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum Node {
    Split {
        /// true = top/bottom, false = left/right
        vertical: bool,
        /// Divider position, 0..1 of the available space.
        ratio: f64,
        first: Box<Node>,
        second: Box<Node>,
    },
    Pane {
        tabs: Vec<Tab>,
        /// Which tab was visible.
        current: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Tab {
    Terminal { cwd: String },
    Browser { url: String },
}

pub fn path() -> PathBuf {
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/state"));
    base.join("cmuxp").join("session.json")
}

// ------------------------------------------------------------------- save

/// Write the session if it changed since the last save.
pub fn save(shared: &Shared) {
    let json = {
        let st = shared.borrow();
        match serde_json::to_string_pretty(&snapshot(&st)) {
            Ok(j) => j,
            Err(_) => return,
        }
    };
    if shared.borrow().last_session.as_deref() == Some(json.as_str()) {
        return;
    }
    let path = path();
    let result = (|| -> std::io::Result<()> {
        std::fs::create_dir_all(path.parent().unwrap())?;
        // Write then rename, so a crash mid-write never leaves a broken file.
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &json)?;
        std::fs::rename(&tmp, &path)
    })();
    match result {
        Ok(()) => shared.borrow_mut().last_session = Some(json),
        Err(e) => eprintln!("cmuxp: cannot save session to {}: {e}", path.display()),
    }
}

fn snapshot(st: &AppState) -> Session {
    let active = st.active.and_then(|a| st.workspaces.iter().position(|w| w.id == a)).unwrap_or(0);
    let workspaces = st
        .workspaces
        .iter()
        .filter_map(|w| {
            let layout = node_of(st, &w.root.first_child()?)?;
            Some(SavedWorkspace { title: w.named.then(|| w.title.clone()), cwd: w.cwd.clone(), layout })
        })
        .collect();
    Session { version: 1, active, workspaces }
}

fn node_of(st: &AppState, widget: &gtk::Widget) -> Option<Node> {
    if let Some(paned) = widget.downcast_ref::<gtk::Paned>() {
        let first = node_of(st, &paned.start_child()?);
        let second = node_of(st, &paned.end_child()?);
        let size = panes::paned_size(paned);
        let ratio = if size > 0 { paned.position() as f64 / size as f64 } else { 0.5 };
        return match (first, second) {
            (Some(a), Some(b)) => Some(Node::Split {
                vertical: paned.orientation() == gtk::Orientation::Vertical,
                ratio: (ratio * 1000.0).round() / 1000.0,
                first: Box::new(a),
                second: Box::new(b),
            }),
            (a, b) => a.or(b),
        };
    }
    let pane_id = st.pane_for_leaf(widget)?;
    let tab_ids = st.tabs_in(pane_id);
    let current = st.current_tab(pane_id).and_then(|c| tab_ids.iter().position(|t| *t == c)).unwrap_or(0);
    let tabs: Vec<Tab> = tab_ids
        .iter()
        .filter_map(|sid| {
            let s = st.surfaces.get(sid)?;
            Some(match &s.kind {
                SurfaceKind::Terminal(_) => Tab::Terminal { cwd: st.cwd_of(*sid).unwrap_or_default() },
                #[cfg(feature = "browser")]
                SurfaceKind::Browser(view) => {
                    Tab::Browser { url: webkit6::prelude::WebViewExt::uri(view).map(|u| u.to_string()).unwrap_or_default() }
                }
            })
        })
        .collect();
    Some(Node::Pane { tabs, current })
}

// ---------------------------------------------------------------- restore

pub fn load() -> Option<Session> {
    if std::env::var_os("CMUXP_FRESH").is_some() {
        return None;
    }
    let text = std::fs::read_to_string(path()).ok()?;
    match serde_json::from_str::<Session>(&text) {
        Ok(s) if !s.workspaces.is_empty() => Some(s),
        Ok(_) => None,
        Err(e) => {
            eprintln!("cmuxp: ignoring unreadable session file: {e}");
            None
        }
    }
}

/// Rebuild a saved session. Returns false if there was nothing to restore.
pub fn restore(shared: &Shared) -> bool {
    let Some(session) = load() else { return false };
    let mut ids = Vec::new();
    for w in &session.workspaces {
        let cwd = existing_dir(&w.cwd);
        let (ws_id, pane_id) = state::workspace_shell(shared, &cwd, w.title.clone());
        build(shared, ws_id, pane_id, &w.layout, &cwd);
        ids.push(ws_id);
    }
    if let Some(ws_id) = ids.get(session.active).or(ids.first()) {
        state::select_workspace(shared, *ws_id);
    }
    true
}

/// Fill pane `pane_id` (already in the tree) with `node`.
fn build(shared: &Shared, ws_id: u64, pane_id: u64, node: &Node, ws_cwd: &str) {
    match node {
        Node::Split { vertical, ratio, first, second } => {
            let other = state::new_pane(shared, ws_id);
            let (a, b) = {
                let st = shared.borrow();
                (st.panes[&pane_id].leaf.clone(), st.panes[&other].leaf.clone())
            };
            let orientation = if *vertical { gtk::Orientation::Vertical } else { gtk::Orientation::Horizontal };
            panes::split_at(a.upcast_ref(), b.upcast_ref(), orientation, false, *ratio);
            build(shared, ws_id, pane_id, first, ws_cwd);
            build(shared, ws_id, other, second, ws_cwd);
        }
        Node::Pane { tabs, current } => {
            let mut created = Vec::new();
            for tab in tabs {
                match tab {
                    Tab::Terminal { cwd } => created.push(state::new_terminal_surface(shared, pane_id, &existing_dir(cwd))),
                    #[cfg(feature = "browser")]
                    Tab::Browser { url } if !url.is_empty() => created.push(state::new_browser_surface(shared, pane_id, url)),
                    _ => {}
                }
            }
            if created.is_empty() {
                created.push(state::new_terminal_surface(shared, pane_id, ws_cwd));
            }
            let visible = created.get(*current).or(created.first()).copied();
            if let Some(sid) = visible {
                let mut st = shared.borrow_mut();
                let nb = st.panes[&pane_id].notebook.clone();
                if let Some(page) = st.surfaces.get(&sid).map(|s| s.page.clone()) {
                    if let Some(n) = nb.page_num(&page) {
                        nb.set_current_page(Some(n));
                    }
                }
                if let Some(ws) = st.workspaces.iter_mut().find(|w| w.id == ws_id) {
                    ws.focused.get_or_insert(sid);
                }
            }
        }
    }
}

/// The folder if it still exists, else the home folder.
fn existing_dir(dir: &str) -> String {
    if !dir.is_empty() && std::path::Path::new(dir).is_dir() {
        dir.to_string()
    } else {
        std::env::var("HOME").unwrap_or_else(|_| "/".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_roundtrip() {
        let s = Session {
            version: 1,
            active: 0,
            workspaces: vec![SavedWorkspace {
                title: Some("api".into()),
                cwd: "/tmp".into(),
                layout: Node::Split {
                    vertical: false,
                    ratio: 0.6,
                    first: Box::new(Node::Pane { tabs: vec![Tab::Terminal { cwd: "/tmp".into() }], current: 0 }),
                    second: Box::new(Node::Pane {
                        tabs: vec![Tab::Browser { url: "http://localhost:3000".into() }, Tab::Terminal { cwd: "/".into() }],
                        current: 1,
                    }),
                },
            }],
        };
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""type":"split""#) && json.contains(r#""kind":"browser""#));
        assert_eq!(serde_json::from_str::<Session>(&json).unwrap(), s);
    }
}
