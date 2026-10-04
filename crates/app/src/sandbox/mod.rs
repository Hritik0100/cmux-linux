//! Per-workspace rootless network sandbox.
//!
//! Each workspace owns a [`NetworkSandbox`]: a keeper process that unshares a
//! user + network namespace (so every terminal in the workspace shares one
//! isolated `localhost`) and spawns the workspace's shells inside it.
//!
//! This module is deliberately GTK-free so it can be unit-tested without a
//! display. The only GTK touchpoint — wrapping the pty master with
//! `vte::Pty::foreign_sync` — lives in `panes.rs`.

pub(crate) mod namespace;

mod forward;

// Future extension points. Unused for now; they exist to hold the design for
// the privileged (non-rootless) veth/bridge/address work described in the plan.
pub mod address;
pub mod dns;
pub mod route;
pub mod veth;

pub use forward::ForwardManager;

use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// A workspace's ephemeral network sandbox. Dropping it tears it down.
pub struct NetworkSandbox {
    keeper: std::process::Child,
    control: Option<UnixStream>,
    fwd_path: String,
    destroyed: bool,
}

impl NetworkSandbox {
    /// Spawn a keeper and wait until its network namespace is ready.
    pub fn create(ws_id: u64) -> Result<Self, String> {
        let fwd_path = fwd_socket_path(ws_id);
        if let Some(dir) = std::path::Path::new(&fwd_path).parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let (keeper, control) = namespace::spawn_keeper(ws_id, &fwd_path)?;
        Ok(NetworkSandbox { keeper, control: Some(control), fwd_path, destroyed: false })
    }

    /// Unix socket the keeper relays forwarded host connections on.
    pub fn fwd_path(&self) -> &str {
        &self.fwd_path
    }

    /// TCP ports currently listening inside this workspace's namespace.
    /// Read straight from the keeper's procfs — cheap, no main-thread blocking.
    pub fn listening_ports(&self) -> Vec<u16> {
        namespace::listening_ports_of(self.keeper.id() as i32)
    }

    /// Open a new pty pair. `master` is wrapped by VTE; `slave` is handed to
    /// the keeper to become the shell's stdin/stdout/stderr.
    pub fn open_pty() -> Result<(OwnedFd, OwnedFd), String> {
        let r = nix::pty::openpty(None, None).map_err(|e| e.to_string())?;
        Ok((r.master, r.slave))
    }

    /// Spawn `argv` in the sandbox with the given `slave` pty fd as its stdio.
    /// Returns the child pid.
    pub fn spawn(&self, slave: OwnedFd, argv: &[String], env: &[String], cwd: &str) -> Result<i32, String> {
        let control = self.control.as_ref().ok_or("sandbox is already destroyed")?;
        namespace::spawn(control, slave, argv, env, cwd)
    }

    /// Tear the sandbox down. Idempotent.
    pub fn destroy(&mut self) {
        if self.destroyed {
            return;
        }
        self.destroyed = true;

        // Close the control channel: the keeper sees EOF, kills its children
        // and exits, which destroys the network namespace.
        if let Some(control) = self.control.take() {
            let _ = control.shutdown(std::net::Shutdown::Both);
        }

        // Reap the keeper without blocking the main loop for long. It exits
        // promptly on EOF; SIGKILL is only a last-resort fallback.
        let deadline = Instant::now() + Duration::from_millis(1500);
        loop {
            match self.keeper.try_wait() {
                Ok(Some(_)) | Err(_) => break,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = self.keeper.kill();
                    let _ = self.keeper.wait();
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        let _ = std::fs::remove_file(&self.fwd_path);
    }
}

impl Drop for NetworkSandbox {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// Deterministic path of a workspace's forward relay socket.
fn fwd_socket_path(ws_id: u64) -> String {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("cmuxp");
    dir.join(format!("sandbox-{ws_id}.fwd")).to_string_lossy().into_owned()
}
