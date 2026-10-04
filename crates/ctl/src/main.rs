//! `cmuxpctl` — command-line client for a running `cmuxp`.
//!
//! Run it from inside a cmuxp terminal and it targets that pane automatically
//! (via `$CMUXP_SURFACE_ID`). Example Claude Code hook:
//!     cmuxpctl notify --title "Claude Code" --body "Waiting for your input"

use clap::{Parser, Subcommand, ValueEnum};
use cmuxp_proto::{Direction, Request, Response, Zone, ENV_SOCKET, ENV_SURFACE};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "cmuxpctl", version, about = "Control a running cmuxp")]
struct Cli {
    /// Socket path (defaults to the one the app uses).
    #[arg(long, global = true, env = ENV_SOCKET)]
    socket: Option<PathBuf>,

    /// Target pane. Defaults to the pane this command runs in.
    #[arg(long, global = true, env = ENV_SURFACE)]
    surface: Option<u64>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check that the app is running.
    Ping,
    /// List workspaces as JSON.
    List,
    /// Create a workspace.
    New {
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        title: Option<String>,
    },
    /// Switch to a workspace by id.
    Select { id: u64 },
    /// Light up the pane and post a notification.
    Notify {
        #[arg(long, short)]
        title: String,
        #[arg(long, short, default_value = "")]
        body: String,
    },
    /// Split the target pane.
    Split {
        #[arg(value_enum)]
        direction: Dir,
    },
    /// Type text into the target pane (use "\n" to press enter).
    Send { text: String },
    /// Open a new terminal tab in the target pane.
    Tab,
    /// Open a browser tab in the target pane (or a new split with --split).
    Browser {
        url: String,
        #[arg(long, value_enum)]
        split: Option<Dir>,
    },
    /// Close the target tab.
    Close,
    /// Set the workspace's sidebar status line (omit TEXT to clear it).
    Status { text: Option<String> },
    /// Show progress in the sidebar: a number 0-100, or "clear".
    Progress {
        value: String,
        #[arg(long, short)]
        label: Option<String>,
    },
    /// Rename the workspace the target tab belongs to.
    Rename { title: String },
    /// Move the target tab into pane PANE (ids from `list`), optionally splitting it.
    Move {
        pane: u64,
        #[arg(long, value_enum, default_value = "center")]
        zone: ZoneArg,
    },
    /// Pop the target tab out into its own window.
    Float,
    /// Show or hide the sidebar.
    Sidebar,
    /// Turn this workspace's network sandbox on/off (default: toggle).
    Sandbox {
        #[arg(value_enum)]
        state: Option<SandboxArg>,
    },
    /// List notification history (newest first), or clear it.
    Notifications {
        #[arg(long)]
        clear: bool,
    },
    /// Open the search popup, optionally pre-filled.
    Search {
        query: Option<String>,
        /// files, commands, tabs, history or commits
        #[arg(long)]
        mode: Option<String>,
    },
    /// Open the git history panel (optionally expanded at a commit).
    Git {
        #[arg(long)]
        commit: Option<String>,
    },
    /// Find TEXT in the target tab's scrollback (opens the find bar).
    Find { text: String },
    /// Open path[:line[:col]] in your editor (or a URL in the browser).
    Open { target: String },
    /// List keyboard shortcuts (edit them with --open or in ~/.config/cmuxp/shortcuts.toml).
    Shortcuts {
        /// Open the Keyboard Shortcuts window instead.
        #[arg(long)]
        open: bool,
        /// With --open: jump to a category (all, yours, workspaces, tabs, window, terminal).
        #[arg(long)]
        section: Option<String>,
        /// Open the "new shortcut" editor.
        #[arg(long)]
        new: bool,
        /// Open the key recorder for an action, e.g. --change split-right.
        #[arg(long)]
        change: Option<String>,
    },
    /// Run an action by name, as if its shortcut was pressed (e.g. split-right, custom-0).
    Action { name: String },
    /// Save a PNG of the cmuxp window.
    Screenshot {
        path: PathBuf,
        /// Which window: main, shortcuts, or dialog.
        #[arg(long)]
        window: Option<String>,
    },
    /// Send a raw JSON request line.
    Raw { json: String },
}

#[derive(Clone, Copy, ValueEnum)]
enum Dir {
    Right,
    Down,
}

#[derive(Clone, Copy, ValueEnum)]
enum ZoneArg {
    Center,
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy, ValueEnum)]
enum SandboxArg {
    On,
    Off,
    Toggle,
}

impl From<ZoneArg> for Zone {
    fn from(z: ZoneArg) -> Self {
        match z {
            ZoneArg::Center => Zone::Center,
            ZoneArg::Left => Zone::Left,
            ZoneArg::Right => Zone::Right,
            ZoneArg::Top => Zone::Top,
            ZoneArg::Bottom => Zone::Bottom,
        }
    }
}

impl From<Dir> for Direction {
    fn from(d: Dir) -> Self {
        match d {
            Dir::Right => Direction::Right,
            Dir::Down => Direction::Down,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let surface = cli.surface;

    let request = match cli.cmd {
        Cmd::Ping => Request::Ping,
        Cmd::List => Request::ListWorkspaces,
        Cmd::New { cwd, title } => Request::NewWorkspace { cwd, title },
        Cmd::Select { id } => Request::SelectWorkspace { id },
        Cmd::Notify { title, body } => Request::Notify { surface, title, body },
        Cmd::Split { direction } => Request::Split { surface, direction: direction.into() },
        Cmd::Send { text } => Request::SendText { surface, text: unescape(&text) },
        Cmd::Tab => Request::NewTab { surface },
        Cmd::Browser { url, split } => Request::BrowserOpen { surface, url, split: split.map(Into::into) },
        Cmd::Close => Request::CloseSurface { surface },
        Cmd::Status { text } => Request::SetStatus { surface, text },
        Cmd::Progress { value, label } => {
            let value = match value.as_str() {
                "clear" | "done" | "none" => None,
                v => match v.trim_end_matches('%').parse::<f64>() {
                    Ok(n) => Some((n / 100.0).clamp(0.0, 1.0)),
                    Err(_) => {
                        eprintln!("progress must be 0-100 or \"clear\"");
                        return ExitCode::from(2);
                    }
                },
            };
            Request::SetProgress { surface, value, label }
        }
        Cmd::Rename { title } => Request::RenameWorkspace { surface, title },
        Cmd::Move { pane, zone } => Request::MoveTab { surface, pane, zone: zone.into() },
        Cmd::Float => Request::FloatTab { surface },
        Cmd::Sidebar => Request::ToggleSidebar,
        Cmd::Sandbox { state } => Request::NetworkSandbox {
            surface,
            enable: match state {
                Some(SandboxArg::On) => Some(true),
                Some(SandboxArg::Off) => Some(false),
                Some(SandboxArg::Toggle) | None => None,
            },
        },
        Cmd::Notifications { clear: true } => Request::ClearNotifications,
        Cmd::Notifications { clear: false } => Request::ListNotifications,
        Cmd::Shortcuts { open, section, new, change } if open || new || change.is_some() || section.is_some() => {
            Request::ShowShortcuts { section, new, change }
        }
        Cmd::Shortcuts { .. } => Request::ListShortcuts,
        Cmd::Action { name } => Request::Activate { action: name },
        Cmd::Find { text } => Request::Find { surface, text },
        Cmd::Git { commit } => Request::GitHistory { surface, commit },
        Cmd::Search { query, mode } => Request::Search { query: query.unwrap_or_default(), mode },
        Cmd::Open { target } => Request::Open { surface, target },
        Cmd::Screenshot { path, window } => {
            // The app resolves paths relative to its own cwd, so send an absolute one.
            let path = std::path::absolute(&path).unwrap_or(path);
            Request::Screenshot { path: path.to_string_lossy().into_owned(), window }
        }
        Cmd::Raw { json } => match serde_json::from_str(&json) {
            Ok(req) => req,
            Err(e) => {
                eprintln!("invalid request: {e}");
                return ExitCode::from(2);
            }
        },
    };

    let socket = cli.socket.unwrap_or_else(cmuxp_proto::default_socket_path);
    match call(&socket, &request) {
        Ok(Response::Ok { data }) => {
            if !data.is_null() {
                println!("{}", serde_json::to_string_pretty(&data).unwrap_or_default());
            }
            ExitCode::SUCCESS
        }
        Ok(Response::Error { message }) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("cannot reach cmuxp at {}: {e}", socket.display());
            ExitCode::FAILURE
        }
    }
}

fn call(socket: &PathBuf, request: &Request) -> std::io::Result<Response> {
    let mut stream = UnixStream::connect(socket)?;
    let mut line = serde_json::to_string(request)?;
    line.push('\n');
    stream.write_all(line.as_bytes())?;

    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(serde_json::from_str(&reply)?)
}

/// Turn the two-character sequences `\n`, `\t`, `\\` into real characters.
fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}
