//! Wire protocol shared by the GUI (`cmuxp`) and the CLI (`cmuxpctl`).
//!
//! Transport: a Unix domain socket. Each message is one line of JSON
//! (newline-delimited JSON). A client writes a `Request` line and reads
//! back exactly one `Response` line. A connection may carry many requests.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Env var injected into every terminal so child processes can find the app.
pub const ENV_SOCKET: &str = "CMUXP_SOCKET_PATH";
/// Env var holding the workspace id of the terminal a process runs in.
pub const ENV_WORKSPACE: &str = "CMUXP_WORKSPACE_ID";
/// Env var holding the surface (pane) id of the terminal a process runs in.
pub const ENV_SURFACE: &str = "CMUXP_SURFACE_ID";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    Right,
    Down,
}

/// Where a moved tab lands in the target pane: join its tabs, or split off to one side.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Zone {
    #[default]
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "kebab-case")]
pub enum Request {
    Ping,
    ListWorkspaces,
    NewWorkspace {
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        title: Option<String>,
    },
    SelectWorkspace {
        id: u64,
    },
    /// Raise a notification for a pane: blue ring + sidebar text + desktop toast.
    Notify {
        #[serde(default)]
        surface: Option<u64>,
        title: String,
        #[serde(default)]
        body: String,
    },
    Split {
        #[serde(default)]
        surface: Option<u64>,
        direction: Direction,
    },
    /// Type text into a terminal pane as if the user typed it.
    SendText {
        #[serde(default)]
        surface: Option<u64>,
        text: String,
    },
    /// Open a new terminal tab in the same pane as `surface`.
    NewTab {
        #[serde(default)]
        surface: Option<u64>,
    },
    /// Open a browser tab in the pane of `surface`, or in a new split if `split` is set.
    BrowserOpen {
        #[serde(default)]
        surface: Option<u64>,
        url: String,
        #[serde(default)]
        split: Option<Direction>,
    },
    /// Close a tab (terminal or browser).
    CloseSurface {
        #[serde(default)]
        surface: Option<u64>,
    },
    /// Set (or clear with `null`) the status line shown under the workspace title in the sidebar.
    SetStatus {
        #[serde(default)]
        surface: Option<u64>,
        #[serde(default)]
        text: Option<String>,
    },
    /// Show a progress bar (0.0–1.0) in the workspace's sidebar row; `null` hides it.
    SetProgress {
        #[serde(default)]
        surface: Option<u64>,
        #[serde(default)]
        value: Option<f64>,
        #[serde(default)]
        label: Option<String>,
    },
    /// Name a workspace. The name sticks instead of following the terminal title.
    RenameWorkspace {
        #[serde(default)]
        surface: Option<u64>,
        title: String,
    },
    /// Move a tab into another pane (same as dragging it there).
    MoveTab {
        #[serde(default)]
        surface: Option<u64>,
        pane: u64,
        #[serde(default)]
        zone: Zone,
    },
    /// Pop a tab out into its own floating window.
    FloatTab {
        #[serde(default)]
        surface: Option<u64>,
    },
    /// Show or hide the sidebar.
    ToggleSidebar,
    /// Enable, disable, or toggle (when `enable` is omitted) the workspace's
    /// network sandbox. Responds with `{ "sandbox": <bool> }`.
    NetworkSandbox {
        #[serde(default)]
        surface: Option<u64>,
        #[serde(default)]
        enable: Option<bool>,
    },
    /// Notification center history, newest first.
    ListNotifications,
    /// Empty the notification center.
    ClearNotifications,
    /// Open the find bar in the pane of `surface` and search for `text`.
    Find {
        #[serde(default)]
        surface: Option<u64>,
        text: String,
    },
    /// Open `path[:line[:col]]` (relative to that tab's folder) in the editor, or a URL in the browser.
    Open {
        #[serde(default)]
        surface: Option<u64>,
        target: String,
    },
    /// Open the git history panel for the folder of `surface`, optionally at a commit.
    GitHistory {
        #[serde(default)]
        surface: Option<u64>,
        #[serde(default)]
        commit: Option<String>,
    },
    /// Open the search popup, optionally with a query and a filter
    /// ("files", "commands", "tabs", "history", "commits").
    Search {
        #[serde(default)]
        query: String,
        #[serde(default)]
        mode: Option<String>,
    },
    /// Keyboard shortcuts: built-in actions with their keys, plus custom ones.
    ListShortcuts,
    /// Open the Keyboard Shortcuts window, optionally at a category, the
    /// "new shortcut" editor, or the key recorder for one action.
    ShowShortcuts {
        #[serde(default)]
        section: Option<String>,
        #[serde(default)]
        new: bool,
        #[serde(default)]
        change: Option<String>,
    },
    /// Run an app action by name, e.g. "split-right" or "custom-0".
    Activate {
        action: String,
    },
    /// Render a window to a PNG file (for docs, bug reports and tests).
    Screenshot {
        path: String,
        /// "main" (default), "shortcuts", or "dialog".
        #[serde(default)]
        window: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Response {
    Ok {
        #[serde(default)]
        data: serde_json::Value,
    },
    Error {
        message: String,
    },
}

impl Response {
    pub fn ok() -> Self {
        Response::Ok { data: serde_json::Value::Null }
    }

    pub fn data(value: impl Serialize) -> Self {
        match serde_json::to_value(value) {
            Ok(data) => Response::Ok { data },
            Err(e) => Response::error(e.to_string()),
        }
    }

    pub fn error(message: impl Into<String>) -> Self {
        Response::Error { message: message.into() }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceInfo {
    pub id: u64,
    pub title: String,
    pub cwd: String,
    pub active: bool,
    pub unread: bool,
    pub unread_count: u32,
    pub notification: Option<String>,
    pub status: Option<String>,
    pub progress: Option<f64>,
    pub git_branch: Option<String>,
    #[serde(default)]
    pub git_dirty: bool,
    #[serde(default)]
    pub git_ahead: u32,
    #[serde(default)]
    pub git_behind: u32,
    /// URL of the open pull request for the branch (needs the `gh` CLI).
    #[serde(default)]
    pub pull_request: Option<String>,
    pub focused_surface: Option<u64>,
    /// Panes in on-screen order.
    pub panes: Vec<PaneInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PaneInfo {
    pub id: u64,
    /// True for a pane torn out into its own window.
    pub floating: bool,
    /// Tabs in tab-bar order.
    pub surfaces: Vec<SurfaceInfo>,
    /// The tab currently shown in this pane.
    pub current: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NotificationInfo {
    pub id: u64,
    pub workspace: u64,
    pub surface: u64,
    pub title: String,
    pub body: String,
    pub seconds_ago: u64,
    pub read: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SurfaceInfo {
    pub id: u64,
    /// "terminal" or "browser".
    pub kind: String,
    pub title: String,
    /// Foreground command running in a terminal, if any.
    pub task: Option<TaskInfo>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskInfo {
    pub command: String,
    pub seconds: u64,
}

/// `$CMUXP_SOCKET_PATH`, else `$XDG_RUNTIME_DIR/cmuxp.sock`, else `/tmp/cmuxp-$USER.sock`.
pub fn default_socket_path() -> PathBuf {
    if let Some(p) = std::env::var_os(ENV_SOCKET) {
        return PathBuf::from(p);
    }
    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        return PathBuf::from(dir).join("cmuxp.sock");
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "user".into());
    PathBuf::from(format!("/tmp/cmuxp-{user}.sock"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_wire_format() {
        let req = Request::Notify { surface: Some(3), title: "Claude".into(), body: "done".into() };
        let json = serde_json::to_string(&req).unwrap();
        assert_eq!(json, r#"{"cmd":"notify","surface":3,"title":"Claude","body":"done"}"#);
        assert_eq!(serde_json::from_str::<Request>(&json).unwrap(), req);
    }

    #[test]
    fn optional_fields_can_be_omitted() {
        let req: Request = serde_json::from_str(r#"{"cmd":"split","direction":"down"}"#).unwrap();
        assert_eq!(req, Request::Split { surface: None, direction: Direction::Down });
    }

    #[test]
    fn browser_split_is_optional() {
        let req: Request = serde_json::from_str(r#"{"cmd":"browser-open","url":"localhost:3000"}"#).unwrap();
        assert_eq!(req, Request::BrowserOpen { surface: None, url: "localhost:3000".into(), split: None });
    }

    #[test]
    fn clear_progress_with_null() {
        let req: Request = serde_json::from_str(r#"{"cmd":"set-progress","value":null}"#).unwrap();
        assert_eq!(req, Request::SetProgress { surface: None, value: None, label: None });
    }

    #[test]
    fn network_sandbox_toggle() {
        let req: Request = serde_json::from_str(r#"{"cmd":"network-sandbox"}"#).unwrap();
        assert_eq!(req, Request::NetworkSandbox { surface: None, enable: None });
        let req: Request = serde_json::from_str(r#"{"cmd":"network-sandbox","enable":true}"#).unwrap();
        assert_eq!(req, Request::NetworkSandbox { surface: None, enable: Some(true) });
    }

    #[test]
    fn response_wire_format() {
        let json = serde_json::to_string(&Response::error("nope")).unwrap();
        assert_eq!(json, r#"{"status":"error","message":"nope"}"#);
    }
}
