//! Git details for the sidebar: uncommitted changes (`main*`), commits ahead
//! and behind upstream (`↑1 ↓2`), and the branch's pull request (`PR #123`).
//!
//! Everything runs as async subprocesses on the GLib main loop, so a slow
//! repository never freezes the UI:
//!   - `git status --porcelain=v2 --branch`   every few seconds per workspace
//!   - `gh pr view --json number,url,state`    at most once a minute per branch,
//!     only if the GitHub CLI (`gh`) is installed

use std::ffi::OsStr;
use std::time::{Duration, Instant};

use gtk::{gio, glib};

use crate::state::{self, Shared};

const PR_REFRESH: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, Default, PartialEq)]
pub struct GitInfo {
    pub dirty: bool,
    pub ahead: u32,
    pub behind: u32,
    /// (number, url) of the pull request for the current branch.
    pub pr: Option<(u64, String)>,
}

/// Per-workspace bookkeeping so we never run two checks at once.
#[derive(Debug, Default)]
pub struct GitPoll {
    pub busy: bool,
    pub info: GitInfo,
    /// (cwd, branch) the PR was last looked up for, and when.
    pub pr_checked: Option<(String, String, Instant)>,
}

/// Parse `git status --porcelain=v2 --branch`.
pub fn parse_status(out: &str) -> (bool, u32, u32) {
    let (mut dirty, mut ahead, mut behind) = (false, 0, 0);
    for line in out.lines() {
        if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    behind = n.parse().unwrap_or(0);
                }
            }
        } else if !line.starts_with('#') && !line.is_empty() {
            dirty = true;
        }
    }
    (dirty, ahead, behind)
}

pub(crate) async fn run(cwd: &str, argv: &[&str]) -> Option<String> {
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::STDOUT_PIPE | gio::SubprocessFlags::STDERR_SILENCE);
    launcher.set_cwd(cwd);
    // Never prompt (e.g. for credentials) from a background check.
    launcher.setenv("GIT_TERMINAL_PROMPT", "0", true);
    launcher.setenv("GH_PROMPT_DISABLED", "1", true);
    let args: Vec<&OsStr> = argv.iter().map(OsStr::new).collect();
    let proc = launcher.spawn(&args).ok()?;
    let (stdout, _) = proc.communicate_utf8_future(None).await.ok()?;
    if !proc.is_successful() {
        return None;
    }
    stdout.map(|s| s.to_string())
}

/// Start a background refresh for workspace `ws_id` (in `cwd`, on `branch`).
pub fn refresh(shared: &Shared, ws_id: u64, cwd: String, branch: String) {
    {
        let mut st = shared.borrow_mut();
        let Some(poll) = st.git_poll_mut(ws_id) else { return };
        if poll.busy {
            return;
        }
        poll.busy = true;
    }
    let weak = std::rc::Rc::downgrade(shared);
    glib::spawn_future_local(async move {
        let status = run(&cwd, &["git", "status", "--porcelain=v2", "--branch"]).await;

        // Look up the PR only when the branch changed or a minute passed.
        let want_pr = {
            let Some(shared) = weak.upgrade() else { return };
            let st = shared.borrow();
            st.git_poll(ws_id).is_some_and(|p| match &p.pr_checked {
                Some((c, b, at)) => c != &cwd || b != &branch || at.elapsed() > PR_REFRESH,
                None => true,
            })
        };
        let pr = if want_pr && crate::links::which("gh").is_some() {
            Some(
                run(&cwd, &["gh", "pr", "view", "--json", "number,url,state"])
                    .await
                    .and_then(|json| serde_json::from_str::<serde_json::Value>(&json).ok())
                    .filter(|v| v["state"] == "OPEN")
                    .and_then(|v| Some((v["number"].as_u64()?, v["url"].as_str()?.to_string()))),
            )
        } else {
            None
        };

        let Some(shared) = weak.upgrade() else { return };
        let mut st = shared.borrow_mut();
        if let Some(poll) = st.git_poll_mut(ws_id) {
            poll.busy = false;
            if let Some(out) = status {
                let (dirty, ahead, behind) = parse_status(&out);
                poll.info.dirty = dirty;
                poll.info.ahead = ahead;
                poll.info.behind = behind;
            }
            if let Some(pr) = pr {
                poll.info.pr = pr;
                poll.pr_checked = Some((cwd, branch, Instant::now()));
            } else if !want_pr {
                // keep the previous PR
            } else {
                poll.info.pr = None;
                poll.pr_checked = Some((cwd, branch, Instant::now()));
            }
        }
        state::refresh_row_pub(&mut st, ws_id);
    });
}

#[cfg(test)]
mod tests {
    use super::parse_status;

    #[test]
    fn porcelain_v2() {
        let clean = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +0 -0\n";
        assert_eq!(parse_status(clean), (false, 0, 0));
        let dirty = "# branch.head main\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 a b src/x.rs\n? new.txt\n";
        assert_eq!(parse_status(dirty), (true, 2, 1));
        let no_upstream = "# branch.head feature\n";
        assert_eq!(parse_status(no_upstream), (false, 0, 0));
    }
}
