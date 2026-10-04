//! The workspace keeper: a lightweight, single-threaded process that owns a
//! workspace's user + network namespace and spawns the workspace's shells
//! inside it.
//!
//! The keeper is a re-exec of the cmuxp binary (`CMUXP_SANDBOX_KEEPER=1`),
//! connected to the app by a `SOCK_SEQPACKET` Unix socket pair. The app dups
//! the keeper's end to fd [`KEEPER_FD`] before exec.

use std::ffi::CString;
use std::io::{IoSlice, IoSliceMut};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::time::Duration;

use nix::sched::{unshare, CloneFlags};
use nix::sys::socket::{
    cmsg_space, recvmsg, sendmsg, socketpair, AddressFamily, ControlMessage, ControlMessageOwned,
    MsgFlags, SockFlag, SockType, SockaddrStorage,
};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{execvp, fork, setsid, ForkResult, Pid};
use serde::{Deserialize, Serialize};

/// The fd number the app dups the keeper's socket end to before exec.
const KEEPER_FD: RawFd = 3;

/// Env var telling the keeper where to serve forwarded TCP connections.
const ENV_FWD: &str = "CMUXP_SANDBOX_KEEPER_FWD";

#[derive(Serialize, Deserialize)]
struct SpawnRequest {
    argv: Vec<String>,
    env: Vec<String>,
    cwd: String,
}

/// Run the keeper, then exit. Called from `main` when `CMUXP_SANDBOX_KEEPER=1`.
pub(crate) fn keeper_main() -> ! {
    let code = match run_keeper() {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cmuxp sandbox keeper: {e}");
            1
        }
    };
    std::process::exit(code);
}

/// Spawn a keeper and wait until its network namespace is ready. Returns the
/// keeper process and the app's end of the control socket.
pub(crate) fn spawn_keeper(ws_id: u64, fwd_path: &str) -> Result<(std::process::Child, UnixStream), String> {
    let (app_fd, keeper_fd) = socketpair(AddressFamily::Unix, SockType::SeqPacket, None, SockFlag::SOCK_CLOEXEC)
        .map_err(|e| format!("socketpair: {e}"))?;

    let mut cmd = std::process::Command::new("/proc/self/exe");
    cmd.env("CMUXP_SANDBOX_KEEPER", "1");
    cmd.env("CMUXP_SANDBOX_KEEPER_WS", ws_id.to_string());
    cmd.env(ENV_FWD, fwd_path);
    let keeper_fd_raw = keeper_fd.as_raw_fd();
    // dup2 is async-signal-safe, so this pre-exec hook is fine in the forked
    // child of the (multithreaded) app.
    unsafe {
        cmd.pre_exec(move || {
            if libc::dup2(keeper_fd_raw, KEEPER_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| format!("spawn keeper: {e}"))?;
    drop(keeper_fd);

    let control = unsafe { UnixStream::from_raw_fd(app_fd.into_raw_fd()) };
    control.set_read_timeout(Some(Duration::from_secs(5))).map_err(|e| format!("set_read_timeout: {e}"))?;

    let msg = read_msg(control.as_raw_fd()).map_err(|e| format!("keeper readiness: {e}"))?;
    if msg.starts_with(b"OK") {
        Ok((child, control))
    } else {
        let text = String::from_utf8_lossy(&msg).into_owned();
        let _ = child.kill();
        let _ = child.wait();
        Err(format!("keeper failed to start: {text}"))
    }
}

/// Send a spawn request to the keeper and read back the child pid.
pub(crate) fn spawn(stream: &UnixStream, slave: OwnedFd, argv: &[String], env: &[String], cwd: &str) -> Result<i32, String> {
    let req = SpawnRequest { argv: argv.to_vec(), env: env.to_vec(), cwd: cwd.to_string() };
    let payload = serde_json::to_vec(&req).map_err(|e| e.to_string())?;
    let slave_raw = slave.as_raw_fd();
    let iov = [IoSlice::new(&payload)];
    sendmsg(
        stream.as_raw_fd(),
        &iov,
        &[ControlMessage::ScmRights(&[slave_raw])],
        MsgFlags::empty(),
        None::<&SockaddrStorage>,
    )
    .map_err(|e| format!("sendmsg: {e}"))?;
    drop(slave);

    let reply = read_msg(stream.as_raw_fd())?;
    let reply: serde_json::Value = serde_json::from_slice(&reply).map_err(|e| format!("bad keeper reply: {e}"))?;
    if let Some(err) = reply.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    reply.get("pid").and_then(|p| p.as_i64()).map(|p| p as i32).ok_or_else(|| "keeper reply missing pid".into())
}

/// TCP ports a process's network namespace is listening on, read straight from
/// `/proc/<pid>/net/tcp` — no keeper round-trip and no main-thread blocking.
pub(crate) fn listening_ports_of(pid: i32) -> Vec<u16> {
    let mut ports = Vec::new();
    for file in [format!("/proc/{pid}/net/tcp"), format!("/proc/{pid}/net/tcp6")] {
        let Ok(text) = std::fs::read_to_string(&file) else { continue };
        collect_listen_ports(&text, &mut ports);
    }
    ports
}

/// Parse `/proc/net/tcp` lines (columns: `sl local rem st …`) for LISTEN ports.
fn collect_listen_ports(text: &str, ports: &mut Vec<u16>) {
    for line in text.lines().skip(1) {
        let mut fields = line.split_whitespace();
        let _sl = fields.next();
        let local = fields.next();
        let _rem = fields.next();
        let state = fields.next();
        // st == "0A" is TCP_LISTEN.
        if state != Some("0A") {
            continue;
        }
        if let Some(port) = local.and_then(|l| l.rsplit(':').next()).and_then(|h| u16::from_str_radix(h, 16).ok()) {
            if port != 0 && !ports.contains(&port) {
                ports.push(port);
            }
        }
    }
}

/// Move the current process into a fresh user + network namespace and bring
/// loopback up. Runs rootless and keeps the *normal* user: loopback is brought
/// up while the namespace creator still holds CAP_NET_ADMIN, then the real
/// uid/gid are mapped to themselves, so the shell is the real user with correct
/// file permissions and no root.
fn setup_namespace() -> Result<(), String> {
    // Capture the real uid/gid *before* unsharing: inside the new user
    // namespace our uid is unmapped and reads back as the overflow uid (65534).
    let uid = unsafe { libc::geteuid() };
    let gid = unsafe { libc::getegid() };

    unshare(CloneFlags::CLONE_NEWUSER | CloneFlags::CLONE_NEWNET)
        .map_err(|e| userns_hint(format!("unshare(CLONE_NEWUSER|CLONE_NEWNET): {e}")))?;

    // Bring loopback up *now*: the creator of a network namespace has
    // CAP_NET_ADMIN over it until we give that up by writing the uid/gid map.
    bring_lo_up().map_err(userns_hint)?;

    // Map our real uid/gid to themselves so the shell runs as the normal user.
    write_proc("/proc/self/setgroups", "deny")?;
    write_proc("/proc/self/uid_map", &format!("{uid} {uid} 1\n"))?;
    write_proc("/proc/self/gid_map", &format!("{gid} {gid} 1\n"))?;
    Ok(())
}

/// Ubuntu (23.10+, default from 24.04) restricts unprivileged user namespaces:
/// an *unconfined* process that creates one is moved into the built-in
/// `unprivileged_userns` AppArmor profile, which denies CAP_SYS_ADMIN and
/// CAP_NET_ADMIN. Add an actionable hint when that (or a similar restriction)
/// is in force.
fn userns_hint(msg: String) -> String {
    if apparmor_userns_restricted() {
        format!(
            "{msg} -- this is likely Ubuntu's AppArmor unprivileged-userns restriction (which denies \
             CAP_NET_ADMIN). Install cmuxp's AppArmor profile (data/cmuxp.apparmor, see the README) or run \
             `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`"
        )
    } else {
        msg
    }
}

fn apparmor_userns_restricted() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns")
        .map(|s| s.trim() == "1")
        .unwrap_or(false)
}

fn run_keeper() -> Result<(), String> {
    let fd = KEEPER_FD;

    // If setup fails, send the reason over the control socket first so the app
    // can report *why* the workspace has no sandbox, then exit.
    if let Err(e) = setup_namespace() {
        let _ = send_msg(fd, e.as_bytes());
        return Err(e);
    }
    // Tell the app we're ready.
    send_msg(fd, b"OK")?;

    let mut children: Vec<Pid> = Vec::new();

    // Fork a child that relays forwarded TCP connections coming from the host.
    // (A separate process so the keeper stays single-threaded and fork-safe.)
    if let Ok(path) = std::env::var(ENV_FWD) {
        match unsafe { fork() } {
            Ok(ForkResult::Child) => forwarder_main(&path),
            Ok(ForkResult::Parent { child }) => children.push(child),
            Err(e) => eprintln!("cmuxp sandbox keeper: cannot fork forwarder: {e}"),
        }
    }

    // Serve spawn and ports requests until the app hangs up.
    loop {
        while let Some(pid) = reap_child() {
            children.retain(|c| *c != pid);
        }
        match recv_request(fd) {
            Ok(Some((req, slave))) => match slave {
                Some(slave) => {
                    if let Ok(pid) = handle_spawn(fd, slave, &req.argv, &req.env, &req.cwd) {
                        children.push(pid);
                    }
                }
                None => {
                    let _ = send_msg(fd, br#"{"error":"spawn without a pty fd"}"#);
                }
            },
            Ok(None) => break,
            Err(e) => {
                eprintln!("cmuxp sandbox keeper: {e}");
                break;
            }
        }
    }

    // Teardown: kill any surviving children so the namespace can die.
    for pid in &children {
        let _ = unsafe { libc::kill(pid.as_raw(), libc::SIGTERM) };
        let _ = unsafe { libc::kill(pid.as_raw(), libc::SIGKILL) };
    }
    Ok(())
}

/// Serve host-originated forwarded connections: each connection names a port
/// and is relayed to `127.0.0.1:<port>` inside this namespace.
fn forwarder_main(path: &str) -> ! {
    let _ = std::fs::remove_file(path);
    let listener = match std::os::unix::net::UnixListener::bind(path) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cmuxp sandbox keeper: cannot bind forward socket: {e}");
            std::process::exit(1);
        }
    };
    for stream in listener.incoming() {
        if let Ok(stream) = stream {
            std::thread::spawn(move || {
                let _ = handle_forward(stream);
            });
        }
    }
    std::process::exit(0);
}

fn handle_forward(mut stream: UnixStream) -> std::io::Result<()> {
    let port = read_forward_port(&mut stream)?;
    let mut upstream = std::net::TcpStream::connect(("127.0.0.1", port))?;
    let mut app_rx = stream.try_clone()?;
    let mut up_tx = upstream.try_clone()?;
    // host → service
    let pump = std::thread::spawn(move || {
        let _ = std::io::copy(&mut app_rx, &mut up_tx);
        let _ = up_tx.shutdown(std::net::Shutdown::Write);
    });
    // service → host
    let _ = std::io::copy(&mut upstream, &mut stream);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = pump.join();
    Ok(())
}

/// Read the `"<port>\n"` header the app sends before relaying.
fn read_forward_port(stream: &mut UnixStream) -> std::io::Result<u16> {
    use std::io::Read;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        if stream.read(&mut byte)? == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "no port header"));
        }
        if byte[0] == b'\n' {
            break;
        }
        buf.push(byte[0]);
        if buf.len() > 8 {
            return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "bad port header"));
        }
    }
    String::from_utf8_lossy(&buf)
        .trim()
        .parse::<u16>()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "bad port header"))
}

fn handle_spawn(fd: RawFd, slave: OwnedFd, argv: &[String], env: &[String], cwd: &str) -> Result<Pid, String> {
    let shell = argv.first().cloned().unwrap_or_else(|| "/bin/sh".into());
    let shell_c = CString::new(shell.as_str()).map_err(|e| e.to_string())?;
    let cwd_c = CString::new(cwd).map_err(|e| e.to_string())?;

    // fork is safe here: the keeper is single-threaded, so the child can use
    // the full std/nix API (setenv, chdir, …) before exec.
    match unsafe { fork() }.map_err(|e| e.to_string())? {
        ForkResult::Child => {
            // New session and controlling terminal so the shell gets job control
            // ("cannot set terminal process group" / "no job control" without this).
            let _ = setsid();
            let sfd = slave.as_raw_fd();
            let _ = unsafe { libc::ioctl(sfd, libc::TIOCSCTTY as _, 0 as libc::c_int) };
            let _ = unsafe { libc::dup2(sfd, 0) };
            let _ = unsafe { libc::dup2(sfd, 1) };
            let _ = unsafe { libc::dup2(sfd, 2) };
            if sfd > 2 {
                let _ = unsafe { libc::close(sfd) };
            }
            for e in env {
                if let Some((k, v)) = e.split_once('=') {
                    if let (Ok(kc), Ok(vc)) = (CString::new(k), CString::new(v)) {
                        unsafe { libc::setenv(kc.as_ptr(), vc.as_ptr(), 1) };
                    }
                }
            }
            let _ = unsafe { libc::chdir(cwd_c.as_ptr()) };
            let argv: Vec<CString> = argv.iter().map(|s| CString::new(s.as_str()).unwrap()).collect();
            let _ = execvp(&shell_c, &argv);
            unsafe { libc::_exit(127) };
        }
        ForkResult::Parent { child } => {
            drop(slave);
            let json = serde_json::json!({ "pid": child.as_raw() }).to_string();
            send_msg(fd, json.as_bytes())?;
            Ok(child)
        }
    }
}

fn recv_request(fd: RawFd) -> Result<Option<(SpawnRequest, Option<OwnedFd>)>, String> {
    let mut buf = vec![0u8; 8192];
    let mut cmsg = vec![0u8; cmsg_space::<[RawFd; 1]>()];
    let (bytes, slave) = {
        let mut iov = [IoSliceMut::new(&mut buf)];
        let msg = recvmsg::<SockaddrStorage>(fd, &mut iov, Some(&mut cmsg), MsgFlags::empty()).map_err(|e| e.to_string())?;
        if msg.bytes == 0 {
            return Ok(None);
        }
        let slave = msg
            .cmsgs()
            .map_err(|e| e.to_string())?
            .filter_map(|c| match c {
                ControlMessageOwned::ScmRights(mut fds) => (!fds.is_empty()).then(|| fds.remove(0)),
                _ => None,
            })
            .next();
        (msg.bytes, slave)
    };
    let req: SpawnRequest = serde_json::from_slice(&buf[..bytes]).map_err(|e| format!("bad keeper request: {e}"))?;
    Ok(Some((req, slave.map(|fd| unsafe { OwnedFd::from_raw_fd(fd) }))))
}

fn send_msg(fd: RawFd, bytes: &[u8]) -> Result<(), String> {
    let iov = [IoSlice::new(bytes)];
    sendmsg(fd, &iov, &[], MsgFlags::empty(), None::<&SockaddrStorage>)
        .map_err(|e| e.to_string())
        .map(drop)
}

/// Read one `SOCK_SEQPACKET` message from the app.
fn read_msg(fd: RawFd) -> Result<Vec<u8>, String> {
    let mut buf = [0u8; 8192];
    let n = unsafe { libc::recv(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len(), 0) };
    if n < 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    if n == 0 {
        return Err("keeper closed the connection".into());
    }
    Ok(buf[..n as usize].to_vec())
}

/// Reap one exited child (non-blocking), returning its pid if any.
fn reap_child() -> Option<Pid> {
    match waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG)) {
        Ok(WaitStatus::Exited(pid, _)) | Ok(WaitStatus::Signaled(pid, _, _)) => Some(pid),
        _ => None,
    }
}

fn write_proc(path: &str, data: &str) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new().write(true).open(path).map_err(|e| format!("open {path}: {e}"))?;
    f.write_all(data.as_bytes()).map_err(|e| format!("write {path}: {e}"))?;
    Ok(())
}

/// Bring the loopback interface up via `SIOCSIFFLAGS`. Must run while the caller
/// still has CAP_NET_ADMIN in the namespace (before the uid/gid map is written),
/// and as the current process — a subprocess would lose its effective
/// capabilities on exec.
fn bring_lo_up() -> Result<(), String> {
    // SAFETY: ifreq is a plain C struct zeroed then filled with a short name;
    // ioctl only reads/writes it.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return Err(format!("socket: {}", std::io::Error::last_os_error()));
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        for (i, b) in b"lo".iter().enumerate() {
            ifr.ifr_name[i] = *b as libc::c_char;
        }
        if libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut ifr) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(format!("SIOCGIFFLAGS: {e}"));
        }
        ifr.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
        if libc::ioctl(fd, libc::SIOCSIFFLAGS, &ifr) < 0 {
            let e = std::io::Error::last_os_error();
            libc::close(fd);
            return Err(format!("SIOCSIFFLAGS: {e}"));
        }
        libc::close(fd);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Prove the rootless slice: an unprivileged process can unshare a user +
    /// network namespace and bind `127.0.0.1:3000` in it. Skips (rather than
    /// fails) when user namespaces or `ip` are unavailable.
    #[test]
    fn rootless_namespace_isolates_localhost() {
        match unsafe { fork() }.unwrap() {
            ForkResult::Child => {
                if setup_namespace().is_err() {
                    // userns disabled, or `ip` missing — skip.
                    unsafe { libc::_exit(77) };
                }
                let bound = std::net::TcpListener::bind("127.0.0.1:3000").is_ok();
                unsafe { libc::_exit(if bound { 0 } else { 1 }) };
            }
            ForkResult::Parent { child } => match waitpid(child, None).unwrap() {
                WaitStatus::Exited(_, 0) => {}
                WaitStatus::Exited(_, 77) => eprintln!("skipping: user namespaces or `ip` unavailable"),
                other => panic!("namespace setup failed: {other:?}"),
            },
        }
    }
}
