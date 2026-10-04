//! Find in scrollback (terminal) or on the page (browser): Ctrl+Shift+F.
//!
//! Each pane owns a small search bar (`panes::SearchBar`) that floats over its
//! top-right corner and searches whichever tab is visible in that pane.
//! Terminal search uses VTE's PCRE2 regex search; browser search uses
//! WebKit's FindController.

use gtk::glib;
use gtk::prelude::*;
use vte::prelude::*;

use crate::panes::SearchBar;
use crate::state::{self, Shared, SurfaceKind};

// PCRE2 compile flags (from pcre2.h). VTE requires MULTILINE for search regexes.
const PCRE2_CASELESS: u32 = 0x0000_0008;
const PCRE2_MULTILINE: u32 = 0x0000_0400;

/// Connect a pane's search bar to the search logic.
pub fn wire(shared: &Shared, pane_id: u64, bar: &SearchBar) {
    let weak = std::rc::Rc::downgrade(shared);
    let run = move |backwards: bool, refresh: bool| {
        let weak = weak.clone();
        move || state::defer(&weak, move |s| run(s, pane_id, backwards, refresh))
    };

    bar.entry.connect_search_changed({
        let go = run(true, true);
        move |_| go()
    });
    bar.regex.connect_toggled({
        let go = run(true, true);
        move |_| go()
    });
    bar.older.connect_clicked({
        let go = run(true, false);
        move |_| go()
    });
    bar.newer.connect_clicked({
        let go = run(false, false);
        move |_| go()
    });

    // Enter = older match (up), Shift+Enter = newer (down), Esc = close.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let (older, newer) = (run(true, false), run(false, false));
    let weak = std::rc::Rc::downgrade(shared);
    keys.connect_key_pressed(move |_, key, _, mods| match key {
        gtk::gdk::Key::Return | gtk::gdk::Key::KP_Enter => {
            if mods.contains(gtk::gdk::ModifierType::SHIFT_MASK) {
                newer();
            } else {
                older();
            }
            glib::Propagation::Stop
        }
        gtk::gdk::Key::Escape => {
            state::defer(&weak, move |s| close(s, pane_id));
            glib::Propagation::Stop
        }
        _ => glib::Propagation::Proceed,
    });
    bar.entry.add_controller(keys);

    let weak = std::rc::Rc::downgrade(shared);
    bar.close.connect_clicked(move |_| state::defer(&weak, move |s| close(s, pane_id)));
}

/// Show the search bar of the focused pane and put the cursor in it.
pub fn open(shared: &Shared) {
    let bar = {
        let st = shared.borrow();
        let Some(sid) = st.focused_surface() else { return };
        let Some(pane) = st.panes.get(&st.surfaces[&sid].pane) else { return };
        pane.search.clone()
    };
    bar.root.set_visible(true);
    bar.entry.grab_focus();
    bar.entry.select_region(0, -1);
}

/// `cmuxpctl find TEXT`: open the bar in the pane of `surface` and search.
pub fn find(shared: &Shared, surface: Option<u64>, text: &str) -> Result<(), String> {
    let (bar, sid) = {
        let st = shared.borrow();
        let sid = st.resolve_surface(surface).ok_or("no such surface")?;
        let pane_id = st.surfaces[&sid].pane;
        (st.panes.get(&pane_id).ok_or("no such pane")?.search.clone(), sid)
    };
    state::focus_surface(shared, sid);
    bar.root.set_visible(true);
    // Setting the text triggers the search (search-changed), deferred.
    bar.entry.set_text(text);
    Ok(())
}

pub fn close(shared: &Shared, pane_id: u64) {
    let found = {
        let st = shared.borrow();
        let Some(pane) = st.panes.get(&pane_id) else { return };
        Some((pane.search.clone(), st.current_tab(pane_id)))
    };
    let Some((bar, sid)) = found else { return };
    bar.root.set_visible(false);
    bar.status.set_text("");
    if let Some(sid) = sid {
        clear(shared, sid);
        state::focus_surface(shared, sid);
    }
}

/// Remove highlights in a tab.
fn clear(shared: &Shared, sid: u64) {
    let st = shared.borrow();
    let Some(surface) = st.surfaces.get(&sid) else { return };
    match &surface.kind {
        SurfaceKind::Terminal(t) => t.search_set_regex(None, 0),
        #[cfg(feature = "browser")]
        SurfaceKind::Browser(view) => {
            if let Some(fc) = webkit6::prelude::WebViewExt::find_controller(view) {
                fc.search_finish();
            }
        }
    }
}

/// Search the visible tab of `pane_id`. `refresh` = the query changed.
fn run(shared: &Shared, pane_id: u64, backwards: bool, refresh: bool) {
    let found = {
        let st = shared.borrow();
        let Some(pane) = st.panes.get(&pane_id) else { return };
        let Some(sid) = st.current_tab(pane_id) else { return };
        Some((pane.search.clone(), sid))
    };
    let Some((bar, sid)) = found else { return };
    let text = bar.entry.text().to_string();
    bar.status.remove_css_class("error");
    if text.is_empty() {
        bar.status.set_text("");
        clear(shared, sid);
        return;
    }
    // Smart case: all-lowercase queries ignore case.
    let caseless = !text.chars().any(char::is_uppercase);

    let st = shared.borrow();
    let Some(surface) = st.surfaces.get(&sid) else { return };
    match &surface.kind {
        SurfaceKind::Terminal(term) => {
            if refresh {
                let pattern = if bar.regex.is_active() { text.clone() } else { literal(&text) };
                let flags = PCRE2_MULTILINE | if caseless { PCRE2_CASELESS } else { 0 };
                match vte::Regex::for_search(&pattern, flags) {
                    Ok(regex) => {
                        term.search_set_regex(Some(&regex), 0);
                        term.search_set_wrap_around(true);
                    }
                    Err(_) => {
                        bar.status.set_text("Invalid regex");
                        bar.status.add_css_class("error");
                        return;
                    }
                }
            }
            let hit = if backwards { term.search_find_previous() } else { term.search_find_next() };
            bar.status.set_text(if hit { "" } else { "No matches" });
        }
        #[cfg(feature = "browser")]
        SurfaceKind::Browser(view) => {
            let Some(fc) = webkit6::prelude::WebViewExt::find_controller(view) else { return };
            if refresh {
                let mut options = webkit6::FindOptions::WRAP_AROUND;
                if caseless {
                    options |= webkit6::FindOptions::CASE_INSENSITIVE;
                }
                fc.search(&text, options.bits(), u32::MAX);
            } else if backwards {
                fc.search_previous();
            } else {
                fc.search_next();
            }
            bar.status.set_text("");
        }
    }
}

/// Match `text` literally inside a PCRE2 pattern.
fn literal(text: &str) -> String {
    // \Q…\E quotes everything; a literal "\E" inside has to be split out.
    format!("\\Q{}\\E", text.replace("\\E", "\\E\\\\E\\Q"))
}

#[cfg(test)]
mod tests {
    use super::literal;

    #[test]
    fn literal_quotes_everything() {
        assert_eq!(literal("a.b"), "\\Qa.b\\E");
        assert_eq!(literal("x\\Ey"), "\\Qx\\E\\\\E\\Qy\\E");
    }
}
