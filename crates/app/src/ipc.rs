//! Control socket: lets `cmuxpctl` (and agent hooks, scripts, ...) drive the app.
//!
//! Socket I/O happens on plain background threads. GTK widgets may only be
//! touched from the main thread, so each parsed request is sent over a
//! channel to the main loop, handled there, and the reply comes back on a
//! one-shot channel.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::thread;

use cmuxp_proto::{Request, Response};
use gtk::glib;
use gtk::prelude::*;

use crate::state::{self, Shared};

type Job = (Request, async_channel::Sender<Response>);

/// Bind the socket and start serving. Requests are executed on the main thread.
pub fn start(path: &Path, shared: &Shared) -> std::io::Result<()> {
    // A socket file left by a crashed run would make bind() fail, so remove
    // it, but never one that a running cmuxp is still answering on.
    if UnixStream::connect(path).is_ok() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AddrInUse,
            "another cmuxp is using this socket (set CMUXP_SOCKET_PATH to run a second one)",
        ));
    }
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

    let (tx, rx) = async_channel::unbounded::<Job>();

    thread::spawn(move || {
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => {
                    let tx = tx.clone();
                    thread::spawn(move || serve(stream, tx));
                }
                Err(e) => eprintln!("cmuxp: socket accept failed: {e}"),
            }
        }
    });

    let weak = std::rc::Rc::downgrade(shared);
    glib::spawn_future_local(async move {
        while let Ok((request, reply)) = rx.recv().await {
            let Some(shared) = weak.upgrade() else { break };
            let response = dispatch(&shared, request).await;
            let _ = reply.send(response).await;
        }
    });
    Ok(())
}

/// One client connection: read JSON lines, answer each with one JSON line.
fn serve(stream: UnixStream, tx: async_channel::Sender<Job>) {
    let Ok(read_half) = stream.try_clone() else { return };
    let mut writer = stream;
    for line in BufReader::new(read_half).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Request>(&line) {
            Ok(request) => {
                let (reply_tx, reply_rx) = async_channel::bounded(1);
                if tx.send_blocking((request, reply_tx)).is_err() {
                    break;
                }
                reply_rx.recv_blocking().unwrap_or_else(|_| Response::error("app is shutting down"))
            }
            Err(e) => Response::error(format!("bad request: {e}")),
        };
        let mut out = serde_json::to_string(&response).unwrap_or_default();
        out.push('\n');
        if writer.write_all(out.as_bytes()).is_err() {
            break;
        }
    }
}

async fn dispatch(shared: &Shared, request: Request) -> Response {
    let result = match request {
        Request::Ping => return Response::data("pong"),
        Request::ListWorkspaces => return Response::data(shared.borrow().list()),
        Request::NewWorkspace { cwd, title } => {
            Ok(serde_json::json!({ "workspace": state::new_workspace(shared, cwd, title) }))
        }
        Request::SelectWorkspace { id } => {
            if state::select_workspace(shared, id) {
                Ok(serde_json::Value::Null)
            } else {
                Err("no such workspace".to_string())
            }
        }
        Request::Notify { surface, title, body } => {
            state::notify(shared, surface, &title, &body).map(|_| serde_json::Value::Null)
        }
        Request::Split { surface, direction } => {
            state::split(shared, surface, direction).map(|sid| serde_json::json!({ "surface": sid }))
        }
        Request::SendText { surface, text } => {
            state::send_text(shared, surface, &text).map(|_| serde_json::Value::Null)
        }
        Request::NewTab { surface } => state::new_tab(shared, surface, state::Content::Terminal)
            .map(|sid| serde_json::json!({ "surface": sid })),
        Request::BrowserOpen { surface, url, split } => {
            state::open_browser(shared, surface, &url, split).map(|sid| serde_json::json!({ "surface": sid }))
        }
        Request::SetStatus { surface, text } => {
            state::set_status(shared, surface, text).map(|_| serde_json::Value::Null)
        }
        Request::SetProgress { surface, value, label } => {
            state::set_progress(shared, surface, value, label).map(|_| serde_json::Value::Null)
        }
        Request::RenameWorkspace { surface, title } => {
            state::rename_workspace(shared, surface, &title).map(|_| serde_json::Value::Null)
        }
        Request::MoveTab { surface, pane, zone } => {
            state::move_tab_to(shared, surface, pane, zone).map(|_| serde_json::Value::Null)
        }
        Request::FloatTab { surface } => state::float_tab(shared, surface).map(|_| serde_json::Value::Null),
        Request::ToggleSidebar => Ok(serde_json::json!({ "visible": state::toggle_sidebar(shared) })),
        Request::NetworkSandbox { surface, enable } => {
            state::set_network_sandbox(shared, surface, enable).map(|on| serde_json::json!({ "sandbox": on }))
        }
        Request::ListNotifications => return Response::data(state::list_notifications(shared)),
        Request::ClearNotifications => {
            state::clear_notifications(shared);
            Ok(serde_json::Value::Null)
        }
        Request::Find { surface, text } => crate::search::find(shared, surface, &text).map(|_| serde_json::Value::Null),
        Request::Open { surface, target } => {
            let sid = shared.borrow().resolve_surface(surface);
            match sid {
                Some(sid) => {
                    crate::links::open(shared, sid, &target);
                    Ok(serde_json::Value::Null)
                }
                None => Err("no such surface".to_string()),
            }
        }
        Request::GitHistory { surface, commit } => {
            let found = {
                let st = shared.borrow();
                st.resolve_surface(surface).map(|sid| (sid, st.cwd_of(sid).unwrap_or_default()))
            };
            match found {
                Some((sid, dir)) => {
                    state::focus_surface(shared, sid);
                    match commit {
                        Some(hash) => state::show_commit(shared, &dir, &hash),
                        None => state::toggle_git_panel(shared, None),
                    }
                    Ok(serde_json::Value::Null)
                }
                None => Err("no such surface".to_string()),
            }
        }
        Request::Search { query, mode } => {
            let mode = match mode.as_deref() {
                None | Some("all") => Ok(None),
                Some("files") => Ok(Some(crate::palette::Cat::File)),
                Some("commands") => Ok(Some(crate::palette::Cat::Command)),
                Some("tabs") => Ok(Some(crate::palette::Cat::Tab)),
                Some("history") => Ok(Some(crate::palette::Cat::History)),
                Some("commits") => Ok(Some(crate::palette::Cat::Commit)),
                Some(other) => Err(format!("unknown mode {other:?}")),
            };
            let palette = shared.borrow().palette.clone();
            match (mode, palette) {
                (Ok(mode), Some(p)) => {
                    p.open_with(shared, mode, &query);
                    Ok(serde_json::Value::Null)
                }
                (Err(e), _) => Err(e),
                (_, None) => Err("search is not ready".to_string()),
            }
        }
        Request::ListShortcuts => {
            let keymap = shared.borrow().keymap.clone();
            return match keymap {
                Some(k) => Response::Ok { data: k.describe() },
                None => Response::error("shortcuts not loaded yet"),
            };
        }
        Request::ShowShortcuts { section, new, change } => {
            let (window, keymap) = {
                let st = shared.borrow();
                (st.window.clone(), st.keymap.clone())
            };
            match keymap {
                Some(k) => {
                    let target = crate::shortcuts_ui::Target { section, new, change };
                    crate::shortcuts_ui::open_at(window.upcast_ref(), &k, target);
                    Ok(serde_json::Value::Null)
                }
                None => Err("shortcuts not loaded yet".to_string()),
            }
        }
        Request::Activate { action } => {
            let app = shared.borrow().app.clone();
            if app.lookup_action(&action).is_some() {
                app.activate_action(&action, None);
                Ok(serde_json::Value::Null)
            } else {
                Err(format!("no action named {action:?} (see `cmuxpctl shortcuts`)"))
            }
        }
        Request::Screenshot { path, window } => {
            let target: Option<gtk::Window> = match window.as_deref() {
                None | Some("main") => Some(shared.borrow().window.clone().upcast()),
                Some("shortcuts") => crate::shortcuts_ui::current(),
                Some("dialog") => crate::shortcuts_ui::current_dialog(),
                Some(other) => return Response::error(format!("unknown window {other:?}")),
            };
            match target {
                Some(w) => state::screenshot(&w, &path).await.map(|_| serde_json::json!({ "path": path })),
                None => Err("that window is not open".to_string()),
            }
        }
        Request::CloseSurface { surface } => {
            let sid = shared.borrow().resolve_surface(surface);
            match sid {
                Some(sid) => {
                    state::close_surface(shared, sid);
                    Ok(serde_json::Value::Null)
                }
                None => Err("no such surface".to_string()),
            }
        }
    };
    match result {
        Ok(data) => Response::Ok { data },
        Err(message) => Response::Error { message },
    }
}
