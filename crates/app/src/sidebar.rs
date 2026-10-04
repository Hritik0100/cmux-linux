//! The vertical-tabs sidebar: brand header with a "new workspace" button, and one row per workspace.
//!
//! Row layout (top to bottom):
//!   [badge] title                                [×]
//!   status text, or the latest notification
//!   ⟳ running commands with elapsed time
//!   ▬▬▬▬▬▬▬▬░░░ progress (set by `cmuxpctl progress`)
//!   branch • ~/path

use gtk::prelude::*;

/// Logo + "cmuxp", then a small "WORKSPACES" caption.
pub fn header() -> gtk::Box {
    let logo = gtk::Image::from_icon_name(crate::icons::APP_ICON);
    logo.set_pixel_size(22);
    let name = gtk::Label::builder().label("cmuxp").xalign(0.0).hexpand(true).build();
    name.add_css_class("brand");

    let brand = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    brand.add_css_class("sidebar-brand");
    brand.append(&logo);
    brand.append(&name);

    let caption = gtk::Label::builder().label("WORKSPACES").xalign(0.0).build();
    caption.add_css_class("sidebar-heading");

    let header = gtk::Box::new(gtk::Orientation::Vertical, 0);
    header.append(&brand);
    header.append(&caption);
    header
}

/// One running command, for display.
pub struct TaskLine {
    pub command: String,
    pub seconds: u64,
}

#[derive(Clone)]
pub struct Row {
    pub row: gtk::ListBoxRow,
    pub close: gtk::Button,
    title: gtk::Label,
    badge: gtk::Label,
    note: gtk::Label,
    tasks_box: gtk::Box,
    tasks: gtk::Label,
    spinner: gtk::Spinner,
    progress: gtk::ProgressBar,
    meta: gtk::Label,
    /// "PR #123" link, shown when the branch has an open pull request.
    pub pr: gtk::Button,
}

impl Row {
    pub fn new(workspace_id: u64) -> Self {
        let badge = gtk::Label::new(None);
        badge.add_css_class("badge");
        badge.set_valign(gtk::Align::Center);
        badge.set_visible(false);

        let title = gtk::Label::builder()
            .xalign(0.0)
            .hexpand(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["ws-title"])
            .build();

        let close = crate::panes::icon_button("cmuxp-close-symbolic", "Close workspace");
        close.add_css_class("ws-close");

        let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        top.append(&badge);
        top.append(&title);
        top.append(&close);

        let note = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .wrap_mode(gtk::pango::WrapMode::WordChar)
            .lines(3)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["ws-note"])
            .visible(false)
            .build();

        let spinner = gtk::Spinner::new();
        spinner.set_valign(gtk::Align::Start);
        let tasks = gtk::Label::builder().xalign(0.0).hexpand(true).css_classes(["ws-tasks"]).build();
        tasks.set_ellipsize(gtk::pango::EllipsizeMode::End);
        let tasks_box = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        tasks_box.append(&spinner);
        tasks_box.append(&tasks);
        tasks_box.set_visible(false);

        let progress = gtk::ProgressBar::new();
        progress.add_css_class("ws-progress");
        progress.set_visible(false);

        let meta = gtk::Label::builder()
            .xalign(0.0)
            .ellipsize(gtk::pango::EllipsizeMode::Start)
            .css_classes(["ws-meta"])
            .build();

        let pr = gtk::Button::builder().halign(gtk::Align::Start).visible(false).build();
        pr.add_css_class("ws-pr");

        let body = gtk::Box::new(gtk::Orientation::Vertical, 4);
        body.append(&top);
        body.append(&note);
        body.append(&tasks_box);
        body.append(&progress);
        body.append(&meta);
        body.append(&pr);

        let row = gtk::ListBoxRow::builder().child(&body).build();
        row.add_css_class("ws-row");
        // The row remembers which workspace it belongs to.
        row.set_widget_name(&format!("ws-{workspace_id}"));

        Row { row, close, title, badge, note, tasks_box, tasks, spinner, progress, meta, pr }
    }

    pub fn workspace_id(row: &gtk::ListBoxRow) -> Option<u64> {
        row.widget_name().strip_prefix("ws-")?.parse().ok()
    }

    pub fn set_title(&self, text: &str) {
        self.title.set_text(text);
    }

    /// `main* ↑1 ↓2 • ~/path`, plus the PR link.
    pub fn set_meta(&self, branch: Option<&str>, git: &crate::git::GitInfo, path: &str) {
        let branch = branch.map(|b| {
            let mut s = b.to_string();
            if git.dirty {
                s.push('*');
            }
            if git.ahead > 0 {
                s.push_str(&format!(" ↑{}", git.ahead));
            }
            if git.behind > 0 {
                s.push_str(&format!(" ↓{}", git.behind));
            }
            s
        });
        let branch = branch.as_deref();
        match &git.pr {
            Some((number, url)) => {
                self.pr.set_label(&format!("PR #{number} ↗"));
                self.pr.set_tooltip_text(Some(url));
                self.pr.set_visible(true);
            }
            None => self.pr.set_visible(false),
        }
        let home = std::env::var("HOME").unwrap_or_default();
        let shown = match path.strip_prefix(&home) {
            Some(rest) if !home.is_empty() => format!("~{rest}"),
            _ => path.to_string(),
        };
        let text = match branch {
            Some(b) => format!("{b} • {shown}"),
            None => shown,
        };
        if self.meta.text() != text {
            self.meta.set_text(&text);
        }
    }

    /// The description line: an explicit status wins over the latest notification.
    pub fn set_note(&self, text: Option<&str>) {
        self.note.set_text(text.unwrap_or(""));
        self.note.set_visible(text.is_some_and(|t| !t.is_empty()));
    }

    pub fn set_tasks(&self, tasks: &[TaskLine]) {
        let busy = !tasks.is_empty();
        self.tasks_box.set_visible(busy);
        if self.spinner.is_spinning() != busy {
            self.spinner.set_spinning(busy);
        }
        if busy {
            let text: Vec<String> =
                tasks.iter().map(|t| format!("{} · {}", t.command, format_duration(t.seconds))).collect();
            self.tasks.set_text(&text.join("\n"));
        }
    }

    pub fn set_progress(&self, value: Option<f64>, label: Option<&str>) {
        self.progress.set_visible(value.is_some());
        if let Some(v) = value {
            self.progress.set_fraction(v);
            self.progress.set_show_text(true);
            let pct = format!("{:.0}%", v * 100.0);
            self.progress.set_text(Some(&match label {
                Some(l) => format!("{l} — {pct}"),
                None => pct,
            }));
        }
    }

    pub fn set_unread(&self, count: u32) {
        self.badge.set_visible(count > 0);
        self.badge.set_text(&count.to_string());
    }
}

/// 75 → "1m 15s", 3700 → "1h 1m".
pub fn format_duration(secs: u64) -> String {
    match secs {
        0..=59 => format!("{secs}s"),
        60..=3599 => format!("{}m {}s", secs / 60, secs % 60),
        _ => format!("{}h {}m", secs / 3600, (secs % 3600) / 60),
    }
}

#[cfg(test)]
mod tests {
    use super::format_duration;

    #[test]
    fn durations() {
        assert_eq!(format_duration(5), "5s");
        assert_eq!(format_duration(75), "1m 15s");
        assert_eq!(format_duration(3700), "1h 1m");
    }
}
