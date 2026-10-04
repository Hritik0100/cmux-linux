//! Host-side port forwarding for the in-app browser.
//!
//! The browser runs in the host network namespace, so it cannot reach a service
//! listening inside a sandboxed workspace (which only has loopback). For the
//! focused workspace we mirror its listening TCP ports onto host
//! `127.0.0.1:<port>`: each accepted host connection is relayed through the
//! keeper's Unix socket (Unix sockets are not namespaced), which connects to
//! `127.0.0.1:<port>` inside the workspace. No root required.

use std::collections::HashMap;
use std::io::Write;
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Which host ports currently mirror the focused workspace.
#[derive(Default)]
pub struct ForwardManager {
    active: HashMap<u16, Forward>,
    workspace: Option<u64>,
}

struct Forward {
    port: u16,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ForwardManager {
    /// Mirror `ports` (listening inside workspace `ws_id`'s sandbox) on the host,
    /// dropping anything no longer needed. A change of workspace drops everything;
    /// teardown is synchronous so re-binding the same port never races.
    pub fn reconcile(&mut self, ws_id: u64, fwd_path: &str, ports: Vec<u16>) {
        if self.workspace != Some(ws_id) {
            self.clear();
            self.workspace = Some(ws_id);
        }
        self.active.retain(|port, _| ports.contains(port));
        for port in ports {
            if !self.active.contains_key(&port) {
                if let Some(f) = Forward::start(port, fwd_path) {
                    self.active.insert(port, f);
                }
            }
        }
    }

    /// Stop every forward (e.g. the focused workspace has no sandbox).
    pub fn clear(&mut self) {
        self.active.clear();
        self.workspace = None;
    }
}

impl Forward {
    fn start(port: u16, fwd_path: &str) -> Option<Forward> {
        // Loopback only, and never steal a port the host is already using.
        let listener = TcpListener::bind(("127.0.0.1", port)).ok()?;
        let stop = Arc::new(AtomicBool::new(false));
        let stop_thread = stop.clone();
        let path = fwd_path.to_string();
        let handle = std::thread::spawn(move || {
            loop {
                match listener.accept() {
                    Ok((client, _)) => {
                        if stop_thread.load(Ordering::Relaxed) {
                            break; // the "wake" connection that ends the loop
                        }
                        let path = path.clone();
                        std::thread::spawn(move || relay(client, &path, port));
                    }
                    Err(_) => break,
                }
            }
        });
        Some(Forward { port, stop, handle: Some(handle) })
    }
}

impl Drop for Forward {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // Wake the blocking accept so the listener is closed before we return;
        // otherwise a rebind of the same port on the next workspace would fail.
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// One host connection: reach the keeper's relay, name the port, pump bytes.
fn relay(mut client: TcpStream, fwd_path: &str, port: u16) {
    let Ok(mut keeper) = UnixStream::connect(fwd_path) else { return };
    if writeln!(keeper, "{port}").is_err() {
        return;
    }
    let (Ok(mut client_rx), Ok(mut keeper_tx)) = (client.try_clone(), keeper.try_clone()) else { return };
    // host → workspace
    let pump = std::thread::spawn(move || {
        let _ = std::io::copy(&mut client_rx, &mut keeper_tx);
        let _ = keeper_tx.shutdown(std::net::Shutdown::Write);
    });
    // workspace → host
    let _ = std::io::copy(&mut keeper, &mut client);
    let _ = client.shutdown(std::net::Shutdown::Both);
    let _ = pump.join();
}
