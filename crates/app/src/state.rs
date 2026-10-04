//! App state and every operation on it (create/split/close/notify/...).
//!
//! Model:  Workspace (sidebar row)
//!           └─ split tree of Panes   (each pane = a tab bar, `gtk::Notebook`)
//!                └─ Surfaces         (each tab = a terminal or a browser)
//! A pane can also live in its own floating window (tab dragged out of the app).
//!
//! Threading: everything here runs on the GTK main thread. State lives in
//! `Rc<RefCell<AppState>>`. Rule: never call into GTK while holding a
//! `borrow_mut()` if that call can fire one of our own signal handlers;
//! those handlers borrow the state too, and a double borrow panics. Signal
//! handlers that touch state therefore defer their work with `defer()`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::os::fd::AsRawFd;
use std::path::Path;
use std::rc::{Rc, Weak};
use std::time::Instant;

use cmuxp_proto::{
    Direction, PaneInfo, SurfaceInfo, TaskInfo, WorkspaceInfo, Zone, ENV_SOCKET, ENV_SURFACE, ENV_WORKSPACE,
};
use gtk::prelude::*;
use gtk::{gdk, gio, glib};
use vte::prelude::*;

use crate::chrome::{self, Chrome, NoteRow};
use crate::icons::APP_ICON;
use crate::panes::{self, DropZone, TabLabel};
use crate::sidebar::{format_duration, Row, TaskLine};

pub type Shared = Rc<RefCell<AppState>>;

pub const DEFAULT_URL: &str = "https://www.google.com";

/// A command must run at least this long before its completion is announced.
const FINISH_NOTIFY_SECS: u64 = 3;

/// Run `git status` for each workspace every this many ticks (seconds).
const GIT_EVERY_TICKS: u64 = 5;
/// Save the session every this many ticks (only written when it changed).
const SAVE_EVERY_TICKS: u64 = 5;

/// How many entries the notification center keeps.
const MAX_NOTES: usize = 200;

/// One entry in the notification center.
pub struct Note {
    id: u64,
    workspace: u64,
    surface: u64,
    title: String,
    body: String,
    at: Instant,
    read: bool,
}

/// What to put in a new tab.
#[cfg_attr(not(feature = "browser"), allow(dead_code))]
pub enum Content {
    Terminal,
    Browser(String),
}

pub enum SurfaceKind {
    Terminal(vte::Terminal),
    #[cfg(feature = "browser")]
    Browser(webkit6::WebView),
}

/// A foreground command running in a terminal.
pub struct Task {
    pgid: i32,
    command: String,
    started: Instant,
}

/// One tab.
pub struct Surface {
    pub pane: u64,
    /// The notebook page widget.
    pub page: gtk::Widget,
    pub tab: TabLabel,
    pub title: String,
    pub kind: SurfaceKind,
    /// The shell's pid (terminals only), known once it has spawned.
    pub shell_pid: Option<i32>,
    pub task: Option<Task>,
    /// Text sent (via `cmuxpctl send`) before the shell had started.
    pending_input: Vec<u8>,
}

impl Surface {
    fn focus_target(&self) -> gtk::Widget {
        match &self.kind {
            SurfaceKind::Terminal(t) => t.clone().upcast(),
            #[cfg(feature = "browser")]
            SurfaceKind::Browser(w) => w.clone().upcast(),
        }
    }

    pub(crate) fn terminal(&self) -> Option<&vte::Terminal> {
        match &self.kind {
            SurfaceKind::Terminal(t) => Some(t),
            #[cfg(feature = "browser")]
            _ => None,
        }
    }

    fn kind_name(&self) -> &'static str {
        match &self.kind {
            SurfaceKind::Terminal(_) => "terminal",
            #[cfg(feature = "browser")]
            SurfaceKind::Browser(_) => "browser",
        }
    }

    pub(crate) fn icon_name(&self) -> &'static str {
        icon_for(&self.kind)
    }
}

fn icon_for(kind: &SurfaceKind) -> &'static str {
    match kind {
        SurfaceKind::Terminal(_) => "cmuxp-terminal-symbolic",
        #[cfg(feature = "browser")]
        SurfaceKind::Browser(_) => "cmuxp-globe-symbolic",
    }
}

/// One leaf of the split tree: a tab bar holding surfaces.
pub struct Pane {
    pub workspace: u64,
    pub leaf: gtk::Box,
    pub notebook: gtk::Notebook,
    drop_indicator: gtk::Box,
    pub search: panes::SearchBar,
}

/// A vertical tab: a sidebar row plus a split tree of panes.
pub struct Workspace {
    pub id: u64,
    pub title: String,
    /// Set when the user named the workspace; then terminal titles don't replace it.
    pub named: bool,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub notification: Option<String>,
    pub status: Option<String>,
    pub progress: Option<(f64, Option<String>)>,
    pub unread_count: u32,
    pub root: gtk::Box,
    pub focused: Option<u64>,
    pub row: Row,
    /// Dirty/ahead/behind/PR, refreshed in the background (see git.rs).
    pub git: crate::git::GitPoll,
    /// The stack page: [split tree (`root`) | git history panel].
    pub page: gtk::Box,
    pub git_panel: Option<Rc<crate::gitpanel::GitPanel>>,
    /// The workspace's ephemeral network sandbox (created best-effort; `None`
    /// when unprivileged user namespaces are unavailable).
    pub sandbox: Option<crate::sandbox::NetworkSandbox>,
}

pub struct AppState {
    pub app: gtk::Application,
    pub window: gtk::ApplicationWindow,
    pub sidebar: gtk::ListBox,
    pub stack: gtk::Stack,
    pub workspaces: Vec<Workspace>,
    pub panes: HashMap<u64, Pane>,
    pub surfaces: HashMap<u64, Surface>,
    pub active: Option<u64>,
    pub socket_path: String,
    pub chrome: Chrome,
    pub sidebar_panel: gtk::Widget,
    pub keymap: Option<Rc<crate::shortcuts::Keymap>>,
    notes: Vec<Note>,
    ticks: u64,
    /// Last session JSON written, to skip identical saves.
    pub(crate) last_session: Option<String>,
    pub palette: Option<Rc<crate::palette::Palette>>,
    /// Host-port mirrors of the focused workspace's sandbox (for the browser).
    forwards: crate::sandbox::ForwardManager,
    next_id: u64,
}

impl AppState {
    pub fn new(
        app: gtk::Application,
        window: gtk::ApplicationWindow,
        sidebar: gtk::ListBox,
        stack: gtk::Stack,
        socket_path: String,
        chrome: Chrome,
        sidebar_panel: gtk::Widget,
    ) -> Self {
        AppState {
            app,
            window,
            sidebar,
            stack,
            workspaces: Vec::new(),
            panes: HashMap::new(),
            surfaces: HashMap::new(),
            active: None,
            socket_path,
            chrome,
            sidebar_panel,
            keymap: None,
            notes: Vec::new(),
            ticks: 0,
            last_session: None,
            palette: None,
            forwards: Default::default(),
            next_id: 1,
        }
    }

    fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn ws(&self, id: u64) -> Option<&Workspace> {
        self.workspaces.iter().find(|w| w.id == id)
    }

    fn ws_mut(&mut self, id: u64) -> Option<&mut Workspace> {
        self.workspaces.iter_mut().find(|w| w.id == id)
    }

    fn surface_ws(&self, sid: u64) -> Option<u64> {
        Some(self.panes.get(&self.surfaces.get(&sid)?.pane)?.workspace)
    }

    pub fn focused_surface(&self) -> Option<u64> {
        self.ws(self.active?)?.focused.filter(|sid| self.surfaces.contains_key(sid))
    }

    /// The explicitly requested surface if it exists, else the focused one.
    pub fn resolve_surface(&self, explicit: Option<u64>) -> Option<u64> {
        match explicit {
            Some(id) if self.surfaces.contains_key(&id) => Some(id),
            Some(_) => None,
            None => self.focused_surface(),
        }
    }

    pub(crate) fn pane_for_leaf(&self, leaf: &gtk::Widget) -> Option<u64> {
        self.panes.iter().find(|(_, p)| p.leaf.upcast_ref::<gtk::Widget>() == leaf).map(|(id, _)| *id)
    }

    fn surface_for_page(&self, page: &gtk::Widget) -> Option<u64> {
        self.surfaces.iter().find(|(_, s)| &s.page == page).map(|(id, _)| *id)
    }

    /// The floating window a pane lives in, if it is not in the main window.
    fn floating_window(&self, pane_id: u64) -> Option<gtk::Window> {
        let root = self.panes.get(&pane_id)?.leaf.root()?;
        let window = root.downcast::<gtk::Window>().ok()?;
        (window != *self.window.upcast_ref::<gtk::Window>()).then_some(window)
    }

    /// True if `sid` is the only tab left in its workspace's main window layout.
    /// Moving it out would empty the workspace and close it.
    fn is_last_main_tab(&self, sid: u64) -> bool {
        let Some(s) = self.surfaces.get(&sid) else { return false };
        let Some(pane) = self.panes.get(&s.pane) else { return false };
        let main_panes = self.ws(pane.workspace).map_or(0, |w| panes::leaves(w.root.upcast_ref()).len());
        self.floating_window(s.pane).is_none() && main_panes == 1 && pane.notebook.n_pages() == 1
    }

    /// Panes of a workspace: the main split tree in on-screen order, then floating ones.
    fn panes_in(&self, ws_id: u64) -> Vec<u64> {
        let Some(ws) = self.ws(ws_id) else { return Vec::new() };
        let mut out: Vec<u64> =
            panes::leaves(ws.root.upcast_ref()).iter().filter_map(|l| self.pane_for_leaf(l)).collect();
        let mut floating: Vec<u64> =
            self.panes.iter().filter(|(id, p)| p.workspace == ws_id && !out.contains(id)).map(|(id, _)| *id).collect();
        floating.sort();
        out.extend(floating);
        out
    }

    /// Tabs of a pane in tab-bar order.
    pub(crate) fn tabs_in(&self, pane_id: u64) -> Vec<u64> {
        let Some(pane) = self.panes.get(&pane_id) else { return Vec::new() };
        (0..pane.notebook.n_pages())
            .filter_map(|i| pane.notebook.nth_page(Some(i)))
            .filter_map(|page| self.surface_for_page(&page))
            .collect()
    }

    pub(crate) fn current_tab(&self, pane_id: u64) -> Option<u64> {
        let nb = &self.panes.get(&pane_id)?.notebook;
        self.surface_for_page(&nb.nth_page(nb.current_page())?)
    }

    /// cwd from shell integration (OSC 7) if available, else from /proc.
    fn surface_cwd(&self, sid: u64) -> Option<String> {
        let s = self.surfaces.get(&sid)?;
        let from_osc7 = s
            .terminal()
            .and_then(|t| t.current_directory_uri())
            .and_then(|uri| gio::File::for_uri(&uri).path())
            .map(|p| p.to_string_lossy().into_owned());
        from_osc7.or_else(|| {
            let link = std::fs::read_link(format!("/proc/{}/cwd", s.shell_pid?)).ok()?;
            Some(link.to_string_lossy().into_owned())
        })
    }

    /// Working directory of a terminal tab.
    pub(crate) fn cwd_of(&self, sid: u64) -> Option<String> {
        self.surface_cwd(sid)
    }

    pub(crate) fn git_poll(&self, ws_id: u64) -> Option<&crate::git::GitPoll> {
        self.ws(ws_id).map(|w| &w.git)
    }

    pub(crate) fn git_poll_mut(&mut self, ws_id: u64) -> Option<&mut crate::git::GitPoll> {
        self.ws_mut(ws_id).map(|w| &mut w.git)
    }

    /// Best cwd for a new terminal opened "next to" `sid` in workspace `ws_id`.
    fn cwd_near(&self, sid: Option<u64>, ws_id: u64) -> String {
        sid.and_then(|s| self.surface_cwd(s))
            .or_else(|| self.ws(ws_id).map(|w| w.cwd.clone()))
            .unwrap_or_else(home_dir)
    }

    pub fn list(&self) -> Vec<WorkspaceInfo> {
        let surface_info = |id: u64| {
            let s = &self.surfaces[&id];
            SurfaceInfo {
                id,
                kind: s.kind_name().into(),
                title: s.title.clone(),
                task: s.task.as_ref().map(|t| TaskInfo {
                    command: t.command.clone(),
                    seconds: t.started.elapsed().as_secs(),
                }),
            }
        };
        self.workspaces
            .iter()
            .map(|w| WorkspaceInfo {
                id: w.id,
                title: w.title.clone(),
                cwd: w.cwd.clone(),
                active: self.active == Some(w.id),
                unread: w.unread_count > 0,
                unread_count: w.unread_count,
                notification: w.notification.clone(),
                status: w.status.clone(),
                progress: w.progress.as_ref().map(|p| p.0),
                git_branch: w.git_branch.clone(),
                git_dirty: w.git.info.dirty,
                git_ahead: w.git.info.ahead,
                git_behind: w.git.info.behind,
                pull_request: w.git.info.pr.as_ref().map(|p| p.1.clone()),
                focused_surface: w.focused,
                panes: self
                    .panes_in(w.id)
                    .into_iter()
                    .map(|id| PaneInfo {
                        id,
                        floating: self.floating_window(id).is_some(),
                        surfaces: self.tabs_in(id).into_iter().map(surface_info).collect(),
                        current: self.current_tab(id),
                    })
                    .collect(),
            })
            .collect()
    }
}

/// Run `f` on the next main-loop iteration, if the app is still alive.
pub fn defer(weak: &Weak<RefCell<AppState>>, f: impl FnOnce(&Shared) + 'static) {
    let weak = weak.clone();
    glib::idle_add_local_once(move || {
        if let Some(shared) = weak.upgrade() {
            f(&shared);
        }
    });
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| "/".into())
}

fn check_supported(content: &Content) -> Result<(), String> {
    match content {
        Content::Browser(_) if !cfg!(feature = "browser") => {
            Err("this build has no browser (rebuild with the `browser` feature)".into())
        }
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------- workspaces

pub fn new_workspace(shared: &Shared, cwd: Option<String>, title: Option<String>) -> u64 {
    let cwd = cwd.unwrap_or_else(home_dir);
    let (ws_id, pane_id) = workspace_shell(shared, &cwd, title);
    let sid = new_terminal_surface(shared, pane_id, &cwd);
    shared.borrow_mut().ws_mut(ws_id).unwrap().focused = Some(sid);
    select_workspace(shared, ws_id);
    ws_id
}

/// A workspace (sidebar row + stack page) with one empty pane. Returns (workspace, pane).
pub(crate) fn workspace_shell(shared: &Shared, cwd: &str, title: Option<String>) -> (u64, u64) {
    let cwd = cwd.to_string();
    let weak = Rc::downgrade(shared);
    let ws_id = {
        let mut st = shared.borrow_mut();
        let id = st.alloc_id();
        let named = title.is_some();
        let title = title.unwrap_or_else(|| format!("workspace {}", st.workspaces.len() + 1));
        let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
        let row = Row::new(id);
        row.set_title(&title);
        row.set_meta(None, &Default::default(), &cwd);
        row.close.connect_clicked({
            let weak = weak.clone();
            move |_| defer(&weak, move |s| close_workspace(s, id))
        });
        // "PR #123" opens the pull request in a browser tab.
        row.pr.connect_clicked({
            let weak = weak.clone();
            move |_| {
                defer(&weak, move |s| {
                    let target = s.borrow().ws(id).and_then(|w| Some((w.git.info.pr.clone()?.1, w.focused)));
                    if let Some((url, focused)) = target {
                        let _ = open_browser(s, focused, &url, Some(Direction::Right));
                    }
                })
            }
        });
        st.sidebar.append(&row.row);
        root.set_hexpand(true);
        let page = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        page.append(&root);
        st.stack.add_named(&page, Some(&id.to_string()));
        st.workspaces.push(Workspace {
            id,
            title,
            named,
            cwd: cwd.clone(),
            git_branch: None,
            notification: None,
            status: None,
            progress: None,
            unread_count: 0,
            root,
            focused: None,
            row,
            git: Default::default(),
            page,
            git_panel: None,
            sandbox: None,
        });
        id
    };

    // Every workspace gets its own network namespace by default, so identical
    // localhost ports coexist across workspaces. Best-effort: if unprivileged
    // user namespaces are unavailable the workspace still opens, unisolated.
    // Turn it off per workspace with `cmuxpctl sandbox off`.
    if let Err(e) = set_workspace_sandbox(shared, ws_id, Some(true)) {
        eprintln!("cmuxp: workspace {ws_id} has no network sandbox: {e}");
    }

    let pane_id = new_pane(shared, ws_id);
    {
        let st = shared.borrow();
        st.ws(ws_id).unwrap().root.append(&st.panes[&pane_id].leaf);
    }
    (ws_id, pane_id)
}

/// Enable, disable, or toggle (with `enable = None`) workspace `ws_id`'s network
/// sandbox. Returns the resulting enabled state. When enabled, new terminals in
/// the workspace run inside its own network namespace; disabling stops the
/// keeper (and any sandboxed shells).
pub fn set_workspace_sandbox(shared: &Shared, ws_id: u64, enable: Option<bool>) -> Result<bool, String> {
    let mut st = shared.borrow_mut();
    let current = st.ws(ws_id).ok_or("no such workspace")?.sandbox.is_some();
    let want = enable.unwrap_or(!current);
    if want == current {
        return Ok(current);
    }
    if want {
        let sandbox =
            crate::sandbox::NetworkSandbox::create(ws_id).map_err(|e| format!("network sandbox: {e}"))?;
        st.ws_mut(ws_id).unwrap().sandbox = Some(sandbox);
    } else {
        // Dropping the sandbox stops the keeper and its sandboxed shells.
        st.ws_mut(ws_id).unwrap().sandbox = None;
    }
    refresh_row(&mut st, ws_id);
    Ok(want)
}

/// The same, addressed by the workspace that owns `surface`.
pub fn set_network_sandbox(shared: &Shared, surface: Option<u64>, enable: Option<bool>) -> Result<bool, String> {
    let ws_id = {
        let st = shared.borrow();
        let sid = st.resolve_surface(surface).ok_or("no such surface")?;
        st.surface_ws(sid).ok_or("no such workspace")?
    };
    let on = set_workspace_sandbox(shared, ws_id, enable)?;
    reconcile_forwards(shared);
    Ok(on)
}

/// Make a workspace visible and focus its focused tab.
pub fn select_workspace(shared: &Shared, ws_id: u64) -> bool {
    let (sidebar, row, focused) = {
        let mut st = shared.borrow_mut();
        let Some(ws) = st.ws_mut(ws_id) else { return false };
        ws.unread_count = 0;
        ws.row.set_unread(0);
        let row = ws.row.row.clone();
        let focused = ws.focused;
        let title = ws.title.clone();
        st.active = Some(ws_id);
        st.stack.set_visible_child_name(&ws_id.to_string());
        set_header_title(&st, &title);
        (st.sidebar.clone(), row, focused)
    };
    // Both of these can re-enter our handlers, so they run without a borrow held.
    if sidebar.selected_row().as_ref() != Some(&row) {
        sidebar.select_row(Some(&row));
    }
    if let Some(sid) = focused {
        focus_surface(shared, sid);
    }
    // Re-point browser port forwards at this workspace right away, so reloading
    // `localhost:<port>` after switching back works without a retry.
    reconcile_forwards(shared);
    true
}

pub fn cycle_workspace(shared: &Shared, delta: isize) {
    let next = {
        let st = shared.borrow();
        let n = st.workspaces.len() as isize;
        if n == 0 {
            return;
        }
        let cur = st.active.and_then(|a| st.workspaces.iter().position(|w| w.id == a)).unwrap_or(0) as isize;
        st.workspaces[(cur + delta).rem_euclid(n) as usize].id
    };
    select_workspace(shared, next);
}

pub fn select_workspace_index(shared: &Shared, index: usize) {
    let id = shared.borrow().workspaces.get(index).map(|w| w.id);
    if let Some(id) = id {
        select_workspace(shared, id);
    }
}

pub fn close_workspace(shared: &Shared, ws_id: u64) {
    let (next, window, floating) = {
        let mut st = shared.borrow_mut();
        let Some(idx) = st.workspaces.iter().position(|w| w.id == ws_id) else { return };
        let gone: HashSet<u64> = st.panes.iter().filter(|(_, p)| p.workspace == ws_id).map(|(id, _)| *id).collect();
        let floating: Vec<gtk::Window> = gone.iter().filter_map(|p| st.floating_window(*p)).collect();
        let ws = st.workspaces.remove(idx);
        st.panes.retain(|id, _| !gone.contains(id));
        st.surfaces.retain(|_, s| !gone.contains(&s.pane));
        st.stack.remove(&ws.page);
        st.sidebar.remove(&ws.row.row);
        if st.active == Some(ws_id) {
            st.active = None;
        }
        let next = st.workspaces.get(idx.min(st.workspaces.len().saturating_sub(1))).map(|w| w.id);
        (next, st.window.clone(), floating)
    };
    for w in floating {
        w.close();
    }
    match next {
        Some(id) => {
            select_workspace(shared, id);
        }
        None => window.close(),
    }
}

/// `cmuxpctl status`: the description line under the workspace title.
pub fn set_status(shared: &Shared, surface: Option<u64>, text: Option<String>) -> Result<(), String> {
    let mut st = shared.borrow_mut();
    let ws_id = st.resolve_surface(surface).and_then(|s| st.surface_ws(s)).ok_or("no such surface")?;
    st.ws_mut(ws_id).unwrap().status = text.filter(|t| !t.is_empty());
    refresh_row(&mut st, ws_id);
    Ok(())
}

/// `cmuxpctl progress`: a progress bar in the workspace's sidebar row.
pub fn set_progress(shared: &Shared, surface: Option<u64>, value: Option<f64>, label: Option<String>) -> Result<(), String> {
    let mut st = shared.borrow_mut();
    let ws_id = st.resolve_surface(surface).and_then(|s| st.surface_ws(s)).ok_or("no such surface")?;
    st.ws_mut(ws_id).unwrap().progress = value.map(|v| (v.clamp(0.0, 1.0), label));
    refresh_row(&mut st, ws_id);
    Ok(())
}

pub(crate) fn refresh_row_pub(st: &mut AppState, ws_id: u64) {
    refresh_row(st, ws_id);
}

/// Redraw a sidebar row from state: description, running tasks, progress, branch/cwd, badge.
fn refresh_row(st: &mut AppState, ws_id: u64) {
    let mut tasks: Vec<(u64, TaskLine)> = st
        .surfaces
        .iter()
        .filter(|(_, s)| st.panes.get(&s.pane).is_some_and(|p| p.workspace == ws_id))
        .filter_map(|(id, s)| {
            let t = s.task.as_ref()?;
            Some((*id, TaskLine { command: t.command.clone(), seconds: t.started.elapsed().as_secs() }))
        })
        .collect();
    tasks.sort_by_key(|(id, _)| *id);
    let tasks: Vec<TaskLine> = tasks.into_iter().map(|(_, t)| t).collect();
    let cwd = st.ws(ws_id).and_then(|w| w.focused).and_then(|sid| st.surface_cwd(sid));

    let Some(ws) = st.ws_mut(ws_id) else { return };
    if let Some(cwd) = cwd {
        ws.git_branch = git_branch(Path::new(&cwd));
        ws.cwd = cwd;
    }
    if ws.git_branch.is_none() {
        ws.git.info = Default::default();
    }
    ws.row.set_meta(ws.git_branch.as_deref(), &ws.git.info, &ws.cwd);
    ws.row.set_note(ws.status.as_deref().or(ws.notification.as_deref()));
    ws.row.set_tasks(&tasks);
    ws.row.set_progress(ws.progress.as_ref().map(|p| p.0), ws.progress.as_ref().and_then(|p| p.1.as_deref()));
    ws.row.set_unread(ws.unread_count);
}

// --------------------------------------------------------------------- panes

pub(crate) fn new_pane(shared: &Shared, ws_id: u64) -> u64 {
    let pane_id = shared.borrow_mut().alloc_id();
    let w = panes::new_pane();
    let weak = Rc::downgrade(shared);

    let on_click = |button: &gtk::Button, f: fn(&Shared, u64)| {
        let weak = weak.clone();
        button.connect_clicked(move |_| defer(&weak, move |s| f(s, pane_id)));
    };
    crate::search::wire(shared, pane_id, &w.search);
    on_click(&w.new_terminal, |s, p| {
        let _ = add_to_pane(s, p, Content::Terminal);
    });
    on_click(&w.new_browser, |s, p| {
        let _ = add_to_pane(s, p, Content::Browser(DEFAULT_URL.into()));
    });
    on_click(&w.git, |s, p| toggle_git_panel(s, Some(p)));
    on_click(&w.split_right, |s, p| {
        let _ = split_pane(s, p, Direction::Right, Content::Terminal);
    });
    on_click(&w.split_down, |s, p| {
        let _ = split_pane(s, p, Direction::Down, Content::Terminal);
    });
    #[cfg(not(feature = "browser"))]
    w.new_browser.set_visible(false);

    w.notebook.connect_switch_page({
        let weak = weak.clone();
        move |_, page, _| {
            let page = page.clone();
            defer(&weak, move |s| on_page_switched(s, pane_id, page))
        }
    });
    // Fired for our own tabs and for tabs dragged in from another pane.
    w.notebook.connect_page_added({
        let weak = weak.clone();
        move |_, page, _| {
            let page = page.clone();
            defer(&weak, move |s| on_page_added(s, pane_id, page))
        }
    });
    // Closing or dragging away the last tab closes the pane.
    w.notebook.connect_page_removed({
        let weak = weak.clone();
        move |_, _, _| defer(&weak, move |s| on_page_removed(s, pane_id))
    });
    // A tab dropped outside every cmuxp window becomes a floating window.
    w.notebook.connect_create_window({
        let weak = weak.clone();
        move |_, page| {
            let s = weak.upgrade()?;
            let ws_id = {
                let st = s.borrow();
                let sid = st.surface_for_page(page)?;
                if st.is_last_main_tab(sid) {
                    return None;
                }
                st.panes.get(&pane_id)?.workspace
            };
            Some(new_floating_pane(&s, ws_id))
        }
    });

    // Dropping a tab on a pane body: edges split, the middle joins.
    let drop = gtk::DropTarget::new(gtk::NotebookPage::static_type(), gdk::DragAction::MOVE);
    let motion = {
        let (overlay, indicator) = (w.overlay.clone(), w.drop_indicator.clone());
        move |_: &gtk::DropTarget, x: f64, y: f64| {
            let (width, height) = (overlay.width(), overlay.height());
            let zone = panes::zone_at(width as f64, height as f64, x, y);
            panes::show_drop_zone(&indicator, Some(zone), width, height);
            gdk::DragAction::MOVE
        }
    };
    // The preview must disappear however the drag ends: dropped here, dropped
    // on a tab bar, dropped outside, or cancelled. `leave` alone misses some
    // of these, so also listen to the drag itself.
    drop.connect_enter({
        let (motion, weak) = (motion.clone(), weak.clone());
        move |target, x, y| {
            if let Some(drag) = target.current_drop().and_then(|d| d.drag()) {
                let hide = {
                    let weak = weak.clone();
                    move || {
                        if let Some(s) = weak.upgrade() {
                            hide_drop_zones(&s.borrow());
                        }
                    }
                };
                let h = hide.clone();
                drag.connect_drop_performed(move |_| h());
                let h = hide.clone();
                drag.connect_dnd_finished(move |_| h());
                drag.connect_cancel(move |_, _| hide());
            }
            motion(target, x, y)
        }
    });
    drop.connect_motion(motion);
    drop.connect_leave({
        let indicator = w.drop_indicator.clone();
        move |_| panes::show_drop_zone(&indicator, None, 0, 0)
    });
    drop.connect_drop({
        let (overlay, indicator, weak) = (w.overlay.clone(), w.drop_indicator.clone(), weak.clone());
        move |_, value, x, y| {
            panes::show_drop_zone(&indicator, None, 0, 0);
            let (Ok(page), Some(s)) = (value.get::<gtk::NotebookPage>(), weak.upgrade()) else { return false };
            let zone = panes::zone_at(overlay.width() as f64, overlay.height() as f64, x, y);
            on_tab_dropped(&s, pane_id, &page.child(), zone);
            true
        }
    });
    w.overlay.add_controller(drop);

    shared
        .borrow_mut()
        .panes
        .insert(pane_id, Pane { workspace: ws_id, leaf: w.leaf, notebook: w.notebook, drop_indicator: w.drop_indicator, search: w.search });
    pane_id
}

/// A window holding one pane (which can be split further). Returns its tab bar.
fn new_floating_pane(shared: &Shared, ws_id: u64) -> gtk::Notebook {
    let pane_id = new_pane(shared, ws_id);
    let (leaf, notebook, app) = {
        let st = shared.borrow();
        let p = &st.panes[&pane_id];
        (p.leaf.clone(), p.notebook.clone(), st.app.clone())
    };
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&leaf);
    let window = gtk::Window::builder()
        .application(&app)
        .title("cmuxp")
        .icon_name(APP_ICON)
        .default_width(900)
        .default_height(560)
        .child(&root)
        .build();
    window.add_css_class("floating");
    let weak = Rc::downgrade(shared);
    window.connect_close_request(move |w| {
        if let Some(s) = weak.upgrade() {
            forget_window(&s, w);
        }
        glib::Propagation::Proceed
    });
    window.present();
    notebook
}

/// A floating window is closing: drop its panes and tabs from the state.
fn forget_window(shared: &Shared, window: &gtk::Window) {
    let mut st = shared.borrow_mut();
    let gone: HashSet<u64> =
        st.panes.keys().copied().filter(|p| st.floating_window(*p).as_ref() == Some(window)).collect();
    st.panes.retain(|id, _| !gone.contains(id));
    st.surfaces.retain(|_, s| !gone.contains(&s.pane));
}

/// Split `pane_id` and put `content` in the new pane.
fn split_pane(shared: &Shared, pane_id: u64, direction: Direction, content: Content) -> Result<u64, String> {
    check_supported(&content)?;
    let (ws_id, old_leaf, cwd) = {
        let st = shared.borrow();
        let pane = st.panes.get(&pane_id).ok_or("no such pane")?;
        (pane.workspace, pane.leaf.clone(), st.cwd_near(st.current_tab(pane_id), pane.workspace))
    };
    let new_pane_id = new_pane(shared, ws_id);
    let new_leaf = shared.borrow().panes[&new_pane_id].leaf.clone();
    let orientation = match direction {
        Direction::Right => gtk::Orientation::Horizontal,
        Direction::Down => gtk::Orientation::Vertical,
    };
    panes::split(old_leaf.upcast_ref(), new_leaf.upcast_ref(), orientation, false);
    let sid = create_surface(shared, new_pane_id, content, &cwd)?;
    focus_surface(shared, sid);
    Ok(sid)
}

fn close_pane(shared: &Shared, pane_id: u64) {
    let floating = shared.borrow().floating_window(pane_id);
    let Some(pane) = shared.borrow_mut().panes.remove(&pane_id) else { return };
    let ws_id = pane.workspace;
    match panes::remove(pane.leaf.upcast_ref()) {
        panes::Removed::Empty => match floating {
            Some(window) => window.close(),
            None => close_workspace(shared, ws_id),
        },
        panes::Removed::Sibling(sibling) => {
            let next = {
                let st = shared.borrow();
                let lost_focus = st.ws(ws_id).and_then(|w| w.focused).is_none_or(|f| !st.surfaces.contains_key(&f));
                let next = panes::leaves(&sibling).first().and_then(|l| st.pane_for_leaf(l)).and_then(|p| st.current_tab(p));
                next.filter(|_| lost_focus)
            };
            if let Some(next) = next {
                focus_surface(shared, next);
            }
        }
    }
}

fn on_page_added(shared: &Shared, pane_id: u64, page: gtk::Widget) {
    let mut st = shared.borrow_mut();
    hide_drop_zones(&st);
    if !st.panes.contains_key(&pane_id) {
        return;
    }
    if let Some(sid) = st.surface_for_page(&page) {
        st.surfaces.get_mut(&sid).unwrap().pane = pane_id;
    }
}

fn on_page_removed(shared: &Shared, pane_id: u64) {
    hide_drop_zones(&shared.borrow());
    let empty = shared.borrow().panes.get(&pane_id).map(|p| p.notebook.n_pages() == 0);
    if empty == Some(true) {
        close_pane(shared, pane_id);
    }
}

/// A different tab became visible in a pane. Give it keyboard focus if the
/// user was working in this pane (not when a background pane loses a tab).
fn on_page_switched(shared: &Shared, pane_id: u64, page: gtk::Widget) {
    let target = {
        let st = shared.borrow();
        let Some(sid) = st.surface_for_page(&page) else { return };
        let Some(pane) = st.panes.get(&pane_id) else { return };
        let Some(ws) = st.ws(pane.workspace) else { return };
        let focus_inside = pane
            .notebook
            .root()
            .and_then(|r| gtk::prelude::RootExt::focus(&r))
            .is_some_and(|f| f.is_ancestor(&pane.notebook));
        let pane_was_focused = ws.focused.and_then(|f| st.surfaces.get(&f)).is_none_or(|f| f.pane == pane_id);
        (focus_inside || pane_was_focused).then_some(sid)
    };
    if let Some(sid) = target {
        focus_surface(shared, sid);
    }
}

/// `cmuxpctl move`: the same as dragging a tab onto a pane.
pub fn move_tab_to(shared: &Shared, surface: Option<u64>, pane: u64, zone: Zone) -> Result<(), String> {
    let page = {
        let st = shared.borrow();
        let sid = st.resolve_surface(surface).ok_or("no such surface")?;
        if !st.panes.contains_key(&pane) {
            return Err("no such pane".into());
        }
        st.surfaces[&sid].page.clone()
    };
    let zone = match zone {
        Zone::Center => DropZone::Center,
        Zone::Left => DropZone::Left,
        Zone::Right => DropZone::Right,
        Zone::Top => DropZone::Top,
        Zone::Bottom => DropZone::Bottom,
    };
    on_tab_dropped(shared, pane, &page, zone);
    Ok(())
}

/// `cmuxpctl float`: the same as dragging a tab out of the window.
pub fn float_tab(shared: &Shared, surface: Option<u64>) -> Result<(), String> {
    let (sid, ws_id) = {
        let st = shared.borrow();
        let sid = st.resolve_surface(surface).ok_or("no such surface")?;
        if st.is_last_main_tab(sid) {
            return Err("it is the workspace's only tab; open another tab first".into());
        }
        (sid, st.surface_ws(sid).ok_or("no such workspace")?)
    };
    let notebook = new_floating_pane(shared, ws_id);
    let pane = shared.borrow().panes.iter().find(|(_, p)| p.notebook == notebook).map(|(id, _)| *id);
    if let Some(pane) = pane {
        move_tab(shared, sid, pane);
    }
    Ok(())
}

/// A tab was dropped on the body of `target_pane`.
fn on_tab_dropped(shared: &Shared, target_pane: u64, page: &gtk::Widget, zone: DropZone) {
    hide_drop_zones(&shared.borrow());
    let Some((sid, src_pane, src_tabs, ws_id, target_leaf)) = ({
        let st = shared.borrow();
        st.surface_for_page(page).and_then(|sid| {
            let src = st.surfaces[&sid].pane;
            let src_tabs = st.panes.get(&src).map_or(0, |p| p.notebook.n_pages());
            let target = st.panes.get(&target_pane)?;
            Some((sid, src, src_tabs, target.workspace, target.leaf.clone()))
        })
    }) else {
        return;
    };

    let (orientation, new_first) = match zone {
        DropZone::Center => {
            if src_pane != target_pane {
                move_tab(shared, sid, target_pane);
            }
            return;
        }
        DropZone::Left => (gtk::Orientation::Horizontal, true),
        DropZone::Right => (gtk::Orientation::Horizontal, false),
        DropZone::Top => (gtk::Orientation::Vertical, true),
        DropZone::Bottom => (gtk::Orientation::Vertical, false),
    };
    // Splitting a pane off its own only tab would leave an empty pane behind.
    if src_pane == target_pane && src_tabs <= 1 {
        return;
    }
    let new_pane_id = new_pane(shared, ws_id);
    let new_leaf = shared.borrow().panes[&new_pane_id].leaf.clone();
    panes::split(target_leaf.upcast_ref(), new_leaf.upcast_ref(), orientation, new_first);
    move_tab(shared, sid, new_pane_id);
}

/// Move a tab to another pane. The old label may still be held by the drag
/// icon, so the tab gets a fresh label that copies its look.
fn move_tab(shared: &Shared, sid: u64, to_pane: u64) {
    let Some((from_nb, to_nb, page, old_tab, icon, title)) = ({
        let st = shared.borrow();
        st.surfaces.get(&sid).and_then(|s| {
            Some((
                st.panes.get(&s.pane)?.notebook.clone(),
                st.panes.get(&to_pane)?.notebook.clone(),
                s.page.clone(),
                s.tab.clone(),
                s.icon_name(),
                s.title.clone(),
            ))
        })
    }) else {
        return;
    };
    // `detach_tab` (not `remove_page`) tells GTK this removal is part of a drag.
    from_nb.detach_tab(&page);
    let tab = make_tab_label(shared, sid, icon, &title);
    tab.copy_from(&old_tab);
    if let Some(s) = shared.borrow_mut().surfaces.get_mut(&sid) {
        s.tab = tab.clone();
        s.pane = to_pane;
    }
    panes::add_tab(&to_nb, &page, &tab);
    focus_surface(shared, sid);
}

// ------------------------------------------------------------------ surfaces

fn create_surface(shared: &Shared, pane_id: u64, content: Content, cwd: &str) -> Result<u64, String> {
    check_supported(&content)?;
    match content {
        Content::Terminal => Ok(new_terminal_surface(shared, pane_id, cwd)),
        #[cfg(feature = "browser")]
        Content::Browser(url) => Ok(new_browser_surface(shared, pane_id, &url)),
        #[cfg(not(feature = "browser"))]
        Content::Browser(_) => unreachable!("rejected by check_supported"),
    }
}

/// Add a tab to an existing pane.
fn add_to_pane(shared: &Shared, pane_id: u64, content: Content) -> Result<u64, String> {
    let cwd = {
        let st = shared.borrow();
        let pane = st.panes.get(&pane_id).ok_or("no such pane")?;
        st.cwd_near(st.current_tab(pane_id), pane.workspace)
    };
    let sid = create_surface(shared, pane_id, content, &cwd)?;
    focus_surface(shared, sid);
    Ok(sid)
}

fn pane_of(shared: &Shared, surface: Option<u64>) -> Result<u64, String> {
    let st = shared.borrow();
    let sid = st.resolve_surface(surface).ok_or("no such surface")?;
    Ok(st.surfaces[&sid].pane)
}

/// New tab in the same pane as `surface`.
pub fn new_tab(shared: &Shared, surface: Option<u64>, content: Content) -> Result<u64, String> {
    add_to_pane(shared, pane_of(shared, surface)?, content)
}

/// Split the pane holding `surface`; the new pane gets a terminal.
pub fn split(shared: &Shared, surface: Option<u64>, direction: Direction) -> Result<u64, String> {
    split_pane(shared, pane_of(shared, surface)?, direction, Content::Terminal)
}

/// Browser as a new tab, or in a new split when `split` is set.
pub fn open_browser(shared: &Shared, surface: Option<u64>, url: &str, split: Option<Direction>) -> Result<u64, String> {
    let pane = pane_of(shared, surface)?;
    let content = Content::Browser(url.to_string());
    match split {
        Some(direction) => split_pane(shared, pane, direction, content),
        None => add_to_pane(shared, pane, content),
    }
}

fn make_tab_label(shared: &Shared, sid: u64, icon: &str, title: &str) -> TabLabel {
    let tab = TabLabel::new(icon, title);
    let weak = Rc::downgrade(shared);
    tab.close.connect_clicked(move |_| defer(&weak, move |s| close_surface(s, sid)));
    tab
}

/// Wire up what every tab needs: label, focus tracking, and registration.
fn register_surface(shared: &Shared, sid: u64, pane_id: u64, page: gtk::Widget, title: &str, kind: SurfaceKind) {
    let tab = make_tab_label(shared, sid, icon_for(&kind), title);
    let surface = Surface {
        pane: pane_id,
        page: page.clone(),
        tab: tab.clone(),
        title: title.into(),
        kind,
        shell_pid: None,
        task: None,
        pending_input: Vec::new(),
    };

    let weak = Rc::downgrade(shared);
    let focus = gtk::EventControllerFocus::new();
    focus.connect_enter(move |_| defer(&weak, move |s| on_surface_focused(s, sid)));
    surface.focus_target().add_controller(focus);

    let notebook = {
        let mut st = shared.borrow_mut();
        st.surfaces.insert(sid, surface);
        st.panes[&pane_id].notebook.clone()
    };
    // Emits switch-page / page-added; their handlers are deferred.
    panes::add_tab(&notebook, &page, &tab);
}

pub(crate) fn new_terminal_surface(shared: &Shared, pane_id: u64, cwd: &str) -> u64 {
    let (sid, socket, ws_id) = {
        let mut st = shared.borrow_mut();
        let ws_id = st.panes[&pane_id].workspace;
        (st.alloc_id(), st.socket_path.clone(), ws_id)
    };
    let term = panes::new_terminal();
    let weak = Rc::downgrade(shared);

    // Signal handlers hold a Weak, not an Rc: the state owns the terminal,
    // so a strong reference here would be a cycle that never gets freed.
    term.connect_child_exited({
        let weak = weak.clone();
        move |_, _status| defer(&weak, move |s| close_surface(s, sid))
    });
    term.connect_bell({
        let weak = weak.clone();
        move |_| {
            defer(&weak, move |s| {
                let _ = notify(s, Some(sid), "Bell", "");
            })
        }
    });
    term.connect_window_title_changed({
        let weak = weak.clone();
        move |t| {
            let title = t.window_title().map(|s| s.to_string()).unwrap_or_default();
            defer(&weak, move |s| on_title_changed(s, sid, title))
        }
    });
    term.connect_current_directory_uri_changed({
        let weak = weak.clone();
        move |_| {
            defer(&weak, move |s| {
                let mut st = s.borrow_mut();
                if let Some(ws_id) = st.surface_ws(sid) {
                    refresh_row(&mut st, ws_id);
                }
            })
        }
    });

    crate::links::add_matchers(&term);
    crate::links::wire(shared, sid, &term);
    let page = panes::terminal_page(&term);
    register_surface(shared, sid, pane_id, page, "shell", SurfaceKind::Terminal(term.clone()));

    let env = [
        format!("{ENV_SOCKET}={socket}"),
        format!("{ENV_WORKSPACE}={ws_id}"),
        format!("{ENV_SURFACE}={sid}"),
        "TERM_PROGRAM=cmuxp".into(),
    ];
    // Spawn the shell inside the workspace's network namespace when it has one.
    // The borrow is held only for the synchronous spawn call.
    {
        let st = shared.borrow();
        let sandbox = st.ws(ws_id).and_then(|w| w.sandbox.as_ref());
        panes::spawn_shell(
            &term,
            cwd,
            &env,
            sandbox,
            move |pid| {
                defer(&weak, move |s| {
                    if let Some(surface) = s.borrow_mut().surfaces.get_mut(&sid) {
                        surface.shell_pid = Some(pid);
                        let pending = std::mem::take(&mut surface.pending_input);
                        if let (Some(term), false) = (surface.terminal(), pending.is_empty()) {
                            term.feed_child(&pending);
                        }
                    }
                })
            },
        );
    }
    sid
}

#[cfg(feature = "browser")]
pub(crate) fn new_browser_surface(shared: &Shared, pane_id: u64, url: &str) -> u64 {
    use webkit6::prelude::WebViewExt;

    let sid = shared.borrow_mut().alloc_id();
    let weak = Rc::downgrade(shared);
    // Links that want a new window open as a new tab next to this one.
    let (page, view) = crate::browser::new_browser(url, {
        let weak = weak.clone();
        move |uri| {
            defer(&weak, move |s| {
                let _ = open_browser(s, Some(sid), &uri, None);
            })
        }
    });
    WebViewExt::connect_title_notify(&view, {
        let weak = weak.clone();
        move |v| {
            let title = WebViewExt::title(v).map(|t| t.to_string()).unwrap_or_default();
            defer(&weak, move |s| on_title_changed(s, sid, title))
        }
    });
    view.connect_favicon_notify(move |v| {
        let favicon = v.favicon();
        defer(&weak, move |s| {
            if let Some(surface) = s.borrow().surfaces.get(&sid) {
                surface.tab.set_favicon(favicon.as_ref());
            }
        })
    });
    register_surface(shared, sid, pane_id, page, "New tab", SurfaceKind::Browser(view));
    sid
}

pub fn close_surface(shared: &Shared, sid: u64) {
    let target = {
        let mut st = shared.borrow_mut();
        let Some(surface) = st.surfaces.remove(&sid) else { return };
        st.panes.get(&surface.pane).map(|p| (p.notebook.clone(), surface.page.clone()))
    };
    if let Some((notebook, page)) = target {
        if let Some(n) = notebook.page_num(&page) {
            // If this was the last tab, `on_page_removed` closes the pane.
            notebook.remove_page(Some(n));
        }
    }
    // The page was the last reference to the terminal; VTE now hangs up the
    // pty and the shell receives SIGHUP.
}

pub fn close_focused(shared: &Shared) {
    let sid = shared.borrow().focused_surface();
    if let Some(sid) = sid {
        close_surface(shared, sid);
    }
}

/// Show a tab (switching the pane's notebook to it) and give it keyboard focus.
pub fn focus_surface(shared: &Shared, sid: u64) {
    let found = {
        let st = shared.borrow();
        st.surfaces.get(&sid).and_then(|s| {
            let nb = st.panes.get(&s.pane)?.notebook.clone();
            Some((nb, s.page.clone(), s.focus_target()))
        })
    };
    let Some((notebook, page, target)) = found else { return };
    if let Some(n) = notebook.page_num(&page) {
        if notebook.current_page() != Some(n) {
            notebook.set_current_page(Some(n));
        }
    }
    // Update state now rather than waiting for the focus event, which GTK
    // only delivers while the window is active.
    on_surface_focused(shared, sid);
    glib::idle_add_local_once(move || {
        target.grab_focus();
    });
}

/// Focus the next pane of the active workspace.
pub fn focus_next_pane(shared: &Shared) {
    let next = {
        let st = shared.borrow();
        let Some(ws) = st.active else { return };
        let order = st.panes_in(ws);
        let cur_pane = st.focused_surface().map(|f| st.surfaces[&f].pane);
        let cur = cur_pane.and_then(|p| order.iter().position(|x| *x == p)).unwrap_or(0);
        order.get((cur + 1) % order.len().max(1)).and_then(|p| st.current_tab(*p))
    };
    if let Some(sid) = next {
        focus_surface(shared, sid);
    }
}

/// Next/previous tab inside the focused pane.
pub fn cycle_tab(shared: &Shared, delta: isize) {
    let next = {
        let st = shared.borrow();
        let Some(cur) = st.focused_surface() else { return };
        let tabs = st.tabs_in(st.surfaces[&cur].pane);
        let i = tabs.iter().position(|t| *t == cur).unwrap_or(0) as isize;
        tabs[(i + delta).rem_euclid(tabs.len() as isize) as usize]
    };
    focus_surface(shared, next);
}

pub fn with_focused_terminal(shared: &Shared, f: impl FnOnce(&vte::Terminal)) {
    let term = {
        let st = shared.borrow();
        st.focused_surface().and_then(|sid| st.surfaces.get(&sid)).and_then(|s| s.terminal().cloned())
    };
    if let Some(t) = term {
        f(&t);
    }
}

pub fn send_text(shared: &Shared, surface: Option<u64>, text: &str) -> Result<(), String> {
    let mut st = shared.borrow_mut();
    let sid = st.resolve_surface(surface).ok_or("no such surface")?;
    let s = st.surfaces.get_mut(&sid).unwrap();
    let term = s.terminal().ok_or("surface is not a terminal")?.clone();
    if s.shell_pid.is_none() {
        // The pty does not exist yet; keystrokes would be lost. Replay them once the shell is up.
        s.pending_input.extend_from_slice(text.as_bytes());
    } else {
        term.feed_child(text.as_bytes());
    }
    Ok(())
}

/// Render the main window into a PNG. GTK only keeps the last drawn frame
/// of a widget, so ask for a redraw and wait (up to 2s) for it to happen.
pub async fn screenshot(window: &gtk::Window, path: &str) -> Result<(), String> {
    let window = window.clone();
    let paintable = gtk::WidgetPaintable::new(Some(&window));
    let (tx, rx) = async_channel::bounded::<()>(1);
    let handler = paintable.connect_invalidate_contents(move |_| {
        let _ = tx.try_send(());
    });
    window.queue_draw();
    let drawn = futures_timeout(rx.recv(), 2000).await;
    paintable.disconnect(handler);
    if !drawn {
        return Err("the window did not redraw (is it minimized?)".into());
    }

    let (w, h) = (window.width(), window.height());
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, w as f64, h as f64);
    let node = snapshot.to_node().ok_or("window has not been drawn yet")?;
    let renderer = window.renderer().ok_or("window has no renderer")?;
    let texture = renderer.render_texture(&node, None);
    texture.save_to_png(path).map_err(|e| e.to_string())
}

/// Await `fut`, giving up after `ms` milliseconds. True if it completed in time.
async fn futures_timeout<T>(fut: impl std::future::Future<Output = T>, ms: u64) -> bool {
    let timeout = glib::timeout_future(std::time::Duration::from_millis(ms));
    futures_select(fut, timeout).await
}

/// Minimal select over two futures without pulling in the `futures` crate.
async fn futures_select<A, B>(a: impl std::future::Future<Output = A>, b: impl std::future::Future<Output = B>) -> bool {
    use std::pin::pin;
    use std::task::Poll;
    let (mut a, mut b) = (pin!(a), pin!(b));
    std::future::poll_fn(|cx| {
        if a.as_mut().poll(cx).is_ready() {
            return Poll::Ready(true);
        }
        if b.as_mut().poll(cx).is_ready() {
            return Poll::Ready(false);
        }
        Poll::Pending
    })
    .await
}

// ------------------------------------------------------------ background tasks

/// Runs every second: track foreground commands, refresh sidebar rows, and
/// announce commands that finished while you were looking elsewhere.
pub fn tick(shared: &Shared) {
    let mut finished = Vec::new();
    {
        let mut st = shared.borrow_mut();
        let now = Instant::now();
        for (sid, s) in st.surfaces.iter_mut() {
            let (Some(term), Some(shell)) = (s.terminal(), s.shell_pid) else { continue };
            match foreground_pgid(term) {
                // Something other than the shell owns the terminal: a command is running.
                Some(pgid) if pgid != shell => {
                    if s.task.as_ref().map(|t| t.pgid) != Some(pgid) {
                        s.task = Some(Task { pgid, command: command_line(pgid), started: now });
                        s.tab.set_busy(true);
                    }
                }
                _ => {
                    if let Some(task) = s.task.take() {
                        s.tab.set_busy(false);
                        let secs = now.duration_since(task.started).as_secs();
                        if secs >= FINISH_NOTIFY_SECS {
                            finished.push((*sid, task.command, secs));
                        }
                    }
                }
            }
        }
        let ids: Vec<u64> = st.workspaces.iter().map(|w| w.id).collect();
        for id in ids {
            refresh_row(&mut st, id);
        }
        st.ticks += 1;
    }
    // Every few seconds: git status (and PR) for each workspace that's in a repo.
    let ticks = shared.borrow().ticks;
    if ticks % GIT_EVERY_TICKS == 1 {
        let repos: Vec<(u64, String, String)> = shared
            .borrow()
            .workspaces
            .iter()
            .filter_map(|w| Some((w.id, w.cwd.clone(), w.git_branch.clone()?)))
            .collect();
        for (ws_id, cwd, branch) in repos {
            crate::git::refresh(shared, ws_id, cwd, branch);
        }
    }
    // Keep open git panels current.
    if ticks % GIT_EVERY_TICKS == 3 {
        let open: Vec<_> =
            shared.borrow().workspaces.iter().filter_map(|w| w.git_panel.clone()).filter(|p| p.is_open()).collect();
        for panel in open {
            panel.refresh();
        }
    }
    // And save the session.
    if ticks.is_multiple_of(SAVE_EVERY_TICKS) {
        crate::session::save(shared);
    }
    for (sid, command, secs) in finished {
        let _ = notify(shared, Some(sid), "Finished", &format!("{command} · {}", format_duration(secs)));
    }
    reconcile_forwards(shared);
}

/// Mirror the focused workspace's listening ports onto the host so the in-app
/// browser can reach them (see `sandbox::forward`). Runs every tick; cheap.
fn reconcile_forwards(shared: &Shared) {
    // Read the target under a short borrow; the query is a quick round-trip.
    let target = {
        let st = shared.borrow();
        match st.active.and_then(|a| st.ws(a)).and_then(|w| w.sandbox.as_ref()) {
            Some(sb) => Some((st.active.unwrap_or(0), sb.fwd_path().to_string(), sb.listening_ports())),
            None => None,
        }
    };
    let mut st = shared.borrow_mut();
    match target {
        Some((ws_id, path, ports)) => st.forwards.reconcile(ws_id, &path, ports),
        None => st.forwards.clear(),
    }
}

/// The process group in the foreground of a terminal (the shell when idle).
fn foreground_pgid(term: &vte::Terminal) -> Option<i32> {
    let pty = term.pty()?;
    // SAFETY: tcgetpgrp only reads from a valid, open file descriptor.
    let pgid = unsafe { libc::tcgetpgrp(pty.fd().as_raw_fd()) };
    (pgid > 0).then_some(pgid)
}

/// "cargo build --release" for a pid, shortened for the sidebar.
fn command_line(pid: i32) -> String {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let mut args = raw.split(|b| *b == 0).filter(|a| !a.is_empty()).map(|a| String::from_utf8_lossy(a).into_owned());
    let mut out = match args.next() {
        Some(prog) => prog.rsplit('/').next().unwrap_or(&prog).to_string(),
        None => std::fs::read_to_string(format!("/proc/{pid}/comm")).unwrap_or_default().trim().to_string(),
    };
    for a in args {
        out.push(' ');
        out.push_str(&a);
    }
    if out.chars().count() > 48 {
        out = out.chars().take(47).collect::<String>() + "…";
    }
    out
}

/// Current git branch by reading `.git/HEAD` (no subprocess).
fn git_branch(cwd: &Path) -> Option<String> {
    for dir in cwd.ancestors() {
        let dotgit = dir.join(".git");
        let gitdir = if dotgit.is_dir() {
            dotgit
        } else if dotgit.is_file() {
            // Worktrees and submodules: ".git" is a file containing "gitdir: <path>".
            let text = std::fs::read_to_string(&dotgit).ok()?;
            dir.join(text.trim().strip_prefix("gitdir:")?.trim())
        } else {
            continue;
        };
        let head = std::fs::read_to_string(gitdir.join("HEAD")).ok()?;
        let head = head.trim();
        return Some(match head.strip_prefix("ref: refs/heads/") {
            Some(branch) => branch.to_string(),
            None => head.chars().take(7).collect(),
        });
    }
    None
}

// ------------------------------------------------------------- notifications

/// Blue dot on the tab, blue ring on the pane, text + unread badge in the
/// sidebar, and a desktop notification when the window is not focused.
pub fn notify(shared: &Shared, surface: Option<u64>, title: &str, body: &str) -> Result<(), String> {
    let (app, ws_id, window_active) = {
        let mut st = shared.borrow_mut();
        let sid = st.resolve_surface(surface).ok_or("no such surface")?;
        let ws_id = st.surface_ws(sid).ok_or("no such workspace")?;
        let window_active = st.window.is_active();
        let active = st.active;
        let surface = &st.surfaces[&sid];
        let leaf = st.panes[&surface.pane].leaf.clone();
        let tab = surface.tab.clone();
        let ws = st.ws_mut(ws_id).ok_or("no such workspace")?;
        let looking_at_it = window_active && active == Some(ws_id) && ws.focused == Some(sid);
        if looking_at_it {
            return Ok(());
        }
        tab.set_notified(true);
        leaf.add_css_class("notify-ring");
        ws.notification = Some(if body.is_empty() { title.to_string() } else { format!("{title}: {body}") });
        if active != Some(ws_id) || !window_active {
            ws.unread_count += 1;
        }
        let id = st.alloc_id();
        st.notes.insert(
            0,
            Note { id, workspace: ws_id, surface: sid, title: title.into(), body: body.into(), at: Instant::now(), read: false },
        );
        st.notes.truncate(MAX_NOTES);
        refresh_notification_center(&st);
        refresh_row(&mut st, ws_id);
        (st.app.clone(), ws_id, window_active)
    };
    if !window_active || shared.borrow().active != Some(ws_id) {
        let n = gio::Notification::new(title);
        if !body.is_empty() {
            n.set_body(Some(body));
        }
        app.send_notification(Some(&format!("ws-{ws_id}")), &n);
    }
    Ok(())
}

/// Jump to the first tab with something waiting.
pub fn jump_to_unread(shared: &Shared) {
    let target = {
        let st = shared.borrow();
        st.surfaces
            .iter()
            .filter(|(_, s)| s.tab.is_notified())
            .filter_map(|(id, _)| Some((st.surface_ws(*id)?, *id)))
            .min()
    };
    if let Some((ws_id, sid)) = target {
        select_workspace(shared, ws_id);
        focus_surface(shared, sid);
    }
}

/// Ring the pane while any of its tabs still has an unseen notification.
fn refresh_pane_ring(st: &AppState, pane_id: u64) {
    let Some(pane) = st.panes.get(&pane_id) else { return };
    if st.surfaces.values().any(|s| s.pane == pane_id && s.tab.is_notified()) {
        pane.leaf.add_css_class("notify-ring");
    } else {
        pane.leaf.remove_css_class("notify-ring");
    }
}

fn on_surface_focused(shared: &Shared, sid: u64) {
    let mut st = shared.borrow_mut();
    let Some(ws_id) = st.surface_ws(sid) else { return };
    let pane_id = st.surfaces[&sid].pane;
    for p in st.panes.values().filter(|p| p.workspace == ws_id) {
        p.leaf.remove_css_class("focused");
    }
    st.panes[&pane_id].leaf.add_css_class("focused");
    st.surfaces[&sid].tab.set_notified(false);
    refresh_pane_ring(&st, pane_id);

    let tab_title = st.surfaces[&sid].title.clone();
    let is_active = st.active == Some(ws_id);
    let ws = st.ws_mut(ws_id).unwrap();
    ws.focused = Some(sid);
    if !ws.named {
        ws.row.set_title(&tab_title);
        ws.title = tab_title;
    }
    let title = ws.title.clone();
    if is_active {
        set_header_title(&st, &title);
    }
    // Looking at a tab reads its notifications.
    let mut changed = false;
    for n in st.notes.iter_mut().filter(|n| n.surface == sid && !n.read) {
        n.read = true;
        changed = true;
    }
    if changed {
        refresh_notification_center(&st);
    }
    refresh_row(&mut st, ws_id);
}

fn on_title_changed(shared: &Shared, sid: u64, title: String) {
    if title.is_empty() {
        return;
    }
    let mut st = shared.borrow_mut();
    let Some(ws_id) = st.surface_ws(sid) else { return };
    let surface = st.surfaces.get_mut(&sid).unwrap();
    surface.tab.set_title(&title);
    surface.title = title.clone();
    let is_active = st.active == Some(ws_id);
    let mut renamed = false;
    if let Some(ws) = st.ws_mut(ws_id) {
        if ws.focused == Some(sid) && !ws.named {
            ws.row.set_title(&title);
            ws.title = title.clone();
            renamed = true;
        }
    }
    if renamed && is_active {
        set_header_title(&st, &title);
    }
}

pub fn rename_workspace(shared: &Shared, surface: Option<u64>, title: &str) -> Result<(), String> {
    let mut st = shared.borrow_mut();
    let ws_id = st.resolve_surface(surface).and_then(|s| st.surface_ws(s)).ok_or("no such surface")?;
    let is_active = st.active == Some(ws_id);
    let ws = st.ws_mut(ws_id).unwrap();
    ws.named = !title.is_empty();
    ws.title = title.to_string();
    ws.row.set_title(title);
    if is_active {
        set_header_title(&st, title);
    }
    Ok(())
}

// ------------------------------------------------------------- window chrome

fn set_header_title(st: &AppState, title: &str) {
    st.chrome.title.set_text(title);
    st.window.set_title(Some(&format!("{title} — cmuxp")));
}

fn hide_drop_zones(st: &AppState) {
    for p in st.panes.values() {
        panes::show_drop_zone(&p.drop_indicator, None, 0, 0);
    }
}

pub fn toggle_sidebar(shared: &Shared) -> bool {
    let st = shared.borrow();
    let visible = !st.sidebar_panel.is_visible();
    st.sidebar_panel.set_visible(visible);
    // Highlight the toggle while the sidebar is hidden, so it's easy to find again.
    if visible {
        st.chrome.sidebar_toggle.remove_css_class("on");
    } else {
        st.chrome.sidebar_toggle.add_css_class("on");
    }
    visible
}

// -------------------------------------------------------- notification center

/// Rebuild the bell badge and the notification list.
fn refresh_notification_center(st: &AppState) {
    let unread = st.notes.iter().filter(|n| !n.read).count();
    let badge = &st.chrome.bell_badge;
    badge.set_visible(unread > 0);
    badge.set_text(&if unread > 99 { "99+".to_string() } else { unread.to_string() });

    let list = &st.chrome.notifications;
    while let Some(row) = list.first_child() {
        list.remove(&row);
    }
    for n in &st.notes {
        let ws_name = st.ws(n.workspace).map(|w| w.title.as_str()).unwrap_or("closed workspace");
        list.append(&chrome::note_row(&NoteRow {
            id: n.id,
            title: &n.title,
            body: &n.body,
            workspace: ws_name,
            ago: chrome::ago(n.at.elapsed().as_secs()),
            read: n.read,
        }));
    }
    st.chrome.notifications_empty.set_visible(st.notes.is_empty());
    st.chrome.mark_all_read.set_sensitive(unread > 0);
    st.chrome.clear_all.set_sensitive(!st.notes.is_empty());
}

pub fn refresh_notifications(shared: &Shared) {
    refresh_notification_center(&shared.borrow());
}

pub fn mark_all_notifications_read(shared: &Shared) {
    let mut st = shared.borrow_mut();
    for n in st.notes.iter_mut() {
        n.read = true;
    }
    refresh_notification_center(&st);
}

pub fn clear_notifications(shared: &Shared) {
    let mut st = shared.borrow_mut();
    st.notes.clear();
    refresh_notification_center(&st);
}

/// Clicking a notification: go to its workspace and tab.
pub fn open_notification(shared: &Shared, id: u64) {
    let target = {
        let mut st = shared.borrow_mut();
        let Some(n) = st.notes.iter_mut().find(|n| n.id == id) else { return };
        n.read = true;
        let target = (n.workspace, n.surface);
        refresh_notification_center(&st);
        st.chrome.bell.popdown();
        target
    };
    let (ws_id, sid) = target;
    if select_workspace(shared, ws_id) {
        focus_surface(shared, sid);
    }
}

pub fn list_notifications(shared: &Shared) -> Vec<cmuxp_proto::NotificationInfo> {
    shared
        .borrow()
        .notes
        .iter()
        .map(|n| cmuxp_proto::NotificationInfo {
            id: n.id,
            workspace: n.workspace,
            surface: n.surface,
            title: n.title.clone(),
            body: n.body.clone(),
            seconds_ago: n.at.elapsed().as_secs(),
            read: n.read,
        })
        .collect()
}

// ---------------------------------------------------------------- git panel

/// Open/close the git history panel of a workspace. From a pane's git button
/// (`pane` = Some) it always shows the history of that pane's folder.
pub fn toggle_git_panel(shared: &Shared, pane: Option<u64>) {
    let (ws_id, dir, existing) = {
        let st = shared.borrow();
        let ws_id = match pane {
            Some(p) => st.panes.get(&p).map(|p| p.workspace),
            None => st.active,
        };
        let Some(ws_id) = ws_id else { return };
        let ws = st.ws(ws_id).unwrap();
        let sid = pane.and_then(|p| st.current_tab(p)).or(ws.focused);
        let dir = sid.and_then(|s| st.surface_cwd(s)).unwrap_or_else(|| ws.cwd.clone());
        (ws_id, dir, ws.git_panel.clone())
    };
    if let Some(panel) = &existing {
        if panel.is_open() && pane.is_none() {
            panel.hide();
            return;
        }
    }
    let panel = existing.unwrap_or_else(|| {
        let panel = crate::gitpanel::build(shared);
        let mut st = shared.borrow_mut();
        let ws = st.ws_mut(ws_id).unwrap();
        ws.page.append(&panel.root);
        ws.git_panel = Some(panel.clone());
        panel
    });
    panel.show(&dir);
}

/// Open the git panel at a commit (from the search popup).
pub fn show_commit(shared: &Shared, dir: &str, hash: &str) {
    toggle_git_panel_at(shared, dir);
    let panel = {
        let st = shared.borrow();
        st.active.and_then(|a| st.ws(a)).and_then(|w| w.git_panel.clone())
    };
    if let Some(p) = panel {
        p.reveal_commit(hash);
    }
}

fn toggle_git_panel_at(shared: &Shared, dir: &str) {
    let existing = {
        let st = shared.borrow();
        st.active.and_then(|a| st.ws(a)).map(|w| (w.id, w.git_panel.clone()))
    };
    let Some((ws_id, existing)) = existing else { return };
    let panel = existing.unwrap_or_else(|| {
        let panel = crate::gitpanel::build(shared);
        let mut st = shared.borrow_mut();
        let ws = st.ws_mut(ws_id).unwrap();
        ws.page.append(&panel.root);
        ws.git_panel = Some(panel.clone());
        panel
    });
    panel.show(dir);
}
