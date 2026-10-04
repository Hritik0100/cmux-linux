//! Clickable file paths and URLs in terminal output.
//!
//! VTE underlines text matching our regexes when the mouse is over it.
//! **Ctrl+click** opens it:
//!   - `src/main.rs:42:7`, `./lib/a.ts:10`, `~/x/y.py` → your editor, at that line
//!   - `https://…` → the built-in browser (or the system browser without it)
//!
//! Editor choice, first match wins:
//!   1. `$CMUXP_EDITOR`, a command template with `{file}`, `{line}`, `{col}`,
//!      e.g. `zed {file}:{line}:{col}` or `nvim +{line} {file}`.
//!      Terminal editors (vim, nvim, nano, hx, micro, emacs -nw…) open in a new tab.
//!   2. VS Code (`code -g file:line:col`) if it's on PATH
//!   3. `$VISUAL` / `$EDITOR` in a new tab (`+line file`)
//!   4. the desktop's default app for the file

use std::path::{Path, PathBuf};

use gtk::prelude::*;
use gtk::{gdk, gio};
use vte::prelude::*;

use crate::state::{self, Content, Shared};

const PCRE2_MULTILINE: u32 = 0x0000_0400;

const URL: &str = r#"\b(?:https?|file)://[^\s<>"'`()\[\]{}]*[^\s<>"'`()\[\]{}.,;:!?]"#;
/// Paths with a slash (`src/a.rs`, `./x`, `~/x`, `/abs/x`) or a bare `name.ext:line`.
const PATH: &str = r"(?:~|\.{1,2})?/?(?:[\w.@+-]+/)+[\w.@+-]+(?::\d+(?::\d+)?)?|[\w.@+-]+\.[A-Za-z][\w]{0,9}:\d+(?::\d+)?";

/// Teach a terminal to underline paths and URLs.
pub fn add_matchers(term: &vte::Terminal) {
    for pattern in [URL, PATH] {
        match vte::Regex::for_match(pattern, PCRE2_MULTILINE) {
            Ok(regex) => {
                let tag = term.match_add_regex(&regex, 0);
                term.match_set_cursor_name(tag, "pointer");
            }
            Err(e) => eprintln!("cmuxp: bad link regex: {e}"),
        }
    }
}

/// Ctrl+click handling for a terminal tab.
pub fn wire(shared: &Shared, sid: u64, term: &vte::Terminal) {
    let click = gtk::GestureClick::new();
    click.set_button(gdk::BUTTON_PRIMARY);
    let weak = std::rc::Rc::downgrade(shared);
    click.connect_pressed(move |gesture, _n, x, y| {
        if !gesture.current_event_state().contains(gdk::ModifierType::CONTROL_MASK) {
            return;
        }
        let Some(term) = gesture.widget().and_downcast::<vte::Terminal>() else { return };
        let (text, _tag) = term.check_match_at(x, y);
        let Some(text) = text.map(|t| t.to_string()) else { return };
        // Don't let VTE also start a selection.
        gesture.set_state(gtk::EventSequenceState::Claimed);
        state::defer(&weak, move |s| open(s, sid, &text));
    });
    term.add_controller(click);
}

/// Open a URL or `path[:line[:col]]` that was clicked in tab `sid`.
pub fn open(shared: &Shared, sid: u64, text: &str) {
    if text.contains("://") {
        open_url(shared, sid, text);
        return;
    }
    let target = parse_location(text);
    let base = shared.borrow().cwd_of(sid).unwrap_or_else(|| std::env::var("HOME").unwrap_or_default());
    let path = resolve(&base, &target.path);
    if !path.exists() {
        return; // Looked like a path but isn't one; do nothing rather than guess.
    }
    open_in_editor(shared, sid, &path, target.line, target.col);
}

fn open_url(shared: &Shared, sid: u64, url: &str) {
    if cfg!(feature = "browser") {
        let _ = state::open_browser(shared, Some(sid), url, None);
    } else if let Err(e) = gio::AppInfo::launch_default_for_uri(url, None::<&gio::AppLaunchContext>) {
        eprintln!("cmuxp: cannot open {url}: {e}");
    }
}

#[derive(Debug, PartialEq)]
pub struct Location {
    pub path: String,
    pub line: Option<u32>,
    pub col: Option<u32>,
}

/// "src/a.rs:42:7" → (src/a.rs, 42, 7)
pub fn parse_location(text: &str) -> Location {
    let mut parts: Vec<&str> = text.split(':').collect();
    let mut nums = Vec::new();
    while parts.len() > 1 {
        match parts.last().and_then(|p| p.parse::<u32>().ok()) {
            Some(n) => {
                nums.insert(0, n);
                parts.pop();
            }
            None => break,
        }
    }
    Location { path: parts.join(":"), line: nums.first().copied(), col: nums.get(1).copied() }
}

fn resolve(base: &str, path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        return Path::new(&std::env::var("HOME").unwrap_or_default()).join(rest);
    }
    let p = Path::new(path);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new(base).join(p)
    }
}

/// Programs that run inside a terminal (so they open in a new tab).
const TERMINAL_EDITORS: &[&str] = &["vi", "vim", "nvim", "nano", "hx", "helix", "micro", "kak", "emacs", "ne", "joe"];

pub(crate) fn open_in_editor(shared: &Shared, sid: u64, path: &Path, line: Option<u32>, col: Option<u32>) {
    let file = path.to_string_lossy().to_string();
    let (line_s, col_s) = (line.unwrap_or(1).to_string(), col.unwrap_or(1).to_string());

    // 1. $CMUXP_EDITOR template
    if let Ok(template) = std::env::var("CMUXP_EDITOR") {
        let argv: Vec<String> = split_command(&template)
            .into_iter()
            .map(|a| a.replace("{file}", &file).replace("{line}", &line_s).replace("{col}", &col_s))
            .collect();
        run_editor(shared, sid, argv);
        return;
    }
    // 2. VS Code
    if which("code").is_some() {
        run_editor(shared, sid, vec!["code".into(), "-g".into(), format!("{file}:{line_s}:{col_s}")]);
        return;
    }
    // 3. $VISUAL / $EDITOR
    if let Some(editor) = std::env::var("VISUAL").ok().or_else(|| std::env::var("EDITOR").ok()).filter(|e| !e.is_empty()) {
        let mut argv = split_command(&editor);
        if line.is_some() {
            argv.push(format!("+{line_s}"));
        }
        argv.push(file);
        run_editor(shared, sid, argv);
        return;
    }
    // 4. Desktop default
    let uri = gio::File::for_path(path).uri();
    if let Err(e) = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>) {
        eprintln!("cmuxp: cannot open {uri}: {e}");
    }
}

/// Terminal editors get a new tab next to `sid`; GUI editors are started in the background.
fn run_editor(shared: &Shared, sid: u64, argv: Vec<String>) {
    let Some(program) = argv.first() else { return };
    let name = Path::new(program).file_name().and_then(|n| n.to_str()).unwrap_or(program);
    let in_terminal = TERMINAL_EDITORS.contains(&name) && !argv.iter().any(|a| a == "--gui" || a == "-g" && name == "emacs");
    if in_terminal {
        let command: Vec<String> = argv.iter().map(|a| shell_quote(a)).collect();
        let result = state::new_tab(shared, Some(sid), Content::Terminal)
            .and_then(|tab| state::send_text(shared, Some(tab), &format!("{}\n", command.join(" "))));
        if let Err(e) = result {
            eprintln!("cmuxp: cannot open editor: {e}");
        }
        return;
    }
    let args: Vec<&std::ffi::OsStr> = argv.iter().map(std::ffi::OsStr::new).collect();
    let launcher = gio::SubprocessLauncher::new(gio::SubprocessFlags::STDOUT_SILENCE | gio::SubprocessFlags::STDERR_SILENCE);
    if let Err(e) = launcher.spawn(&args) {
        eprintln!("cmuxp: cannot start {program}: {e}");
    }
}

/// Split "code --wait -g" into words, honouring simple quotes.
fn split_command(cmd: &str) -> Vec<String> {
    match gtk::glib::shell_parse_argv(cmd) {
        Ok(words) => words.into_iter().map(|w| w.to_string_lossy().into_owned()).collect(),
        Err(_) => cmd.split_whitespace().map(String::from).collect(),
    }
}

fn shell_quote(s: &str) -> String {
    if s.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+:=@".contains(c)) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub fn which(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|dir| Path::new(dir).join(program)).find(|p| p.is_file())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations() {
        assert_eq!(parse_location("src/main.rs:42:7"), Location { path: "src/main.rs".into(), line: Some(42), col: Some(7) });
        assert_eq!(parse_location("a/b.ts:10"), Location { path: "a/b.ts".into(), line: Some(10), col: None });
        assert_eq!(parse_location("README.md"), Location { path: "README.md".into(), line: None, col: None });
    }

    #[test]
    fn quoting() {
        assert_eq!(shell_quote("src/a.rs"), "src/a.rs");
        assert_eq!(shell_quote("my file.rs"), "'my file.rs'");
        assert_eq!(shell_quote("it's"), r"'it'\''s'");
    }

    #[test]
    fn paths_resolve() {
        assert_eq!(resolve("/repo", "src/a.rs"), PathBuf::from("/repo/src/a.rs"));
        assert_eq!(resolve("/repo", "/etc/hosts"), PathBuf::from("/etc/hosts"));
    }
}
