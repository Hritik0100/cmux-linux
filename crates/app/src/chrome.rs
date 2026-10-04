//! Window chrome: the title bar (sidebar toggle, notification
//! bell, new workspace, current workspace name), the notification center, and
//! the keyboard-shortcut help popover.

use gtk::prelude::*;

use crate::panes::icon_button;

pub struct Chrome {
    pub header: gtk::HeaderBar,
    pub sidebar_toggle: gtk::Button,
    pub bell: gtk::MenuButton,
    pub bell_badge: gtk::Label,
    pub title: gtk::Label,
    pub notifications: gtk::ListBox,
    pub notifications_empty: gtk::Label,
    pub mark_all_read: gtk::Button,
    pub clear_all: gtk::Button,
}

pub fn build() -> Chrome {
    let sidebar_toggle = icon_button("cmuxp-sidebar-symbolic", "Show/hide sidebar (Ctrl+Shift+S)");
    sidebar_toggle.set_action_name(Some("app.toggle-sidebar"));

    // Bell with an unread badge in its top-right corner.
    let bell_badge = gtk::Label::new(None);
    bell_badge.add_css_class("bell-badge");
    bell_badge.set_halign(gtk::Align::End);
    bell_badge.set_valign(gtk::Align::Start);
    bell_badge.set_can_target(false);
    bell_badge.set_visible(false);
    let bell_icon = gtk::Image::from_icon_name("cmuxp-bell-symbolic");
    let bell_content = gtk::Overlay::new();
    bell_content.set_child(Some(&bell_icon));
    bell_content.add_overlay(&bell_badge);

    let (popover, notifications, notifications_empty, mark_all_read, clear_all) = notification_center();
    let bell = gtk::MenuButton::builder()
        .child(&bell_content)
        .popover(&popover)
        .tooltip_text("Notifications (Ctrl+Shift+I)")
        .build();
    bell.add_css_class("flat");
    bell.add_css_class("icon-button");
    bell.add_css_class("bell");

    let search = icon_button("cmuxp-search-symbolic", "Search everything (Ctrl+Shift+Space)");
    search.set_action_name(Some("app.search"));

    let shortcuts = icon_button("cmuxp-keyboard-symbolic", "Keyboard shortcuts (Ctrl+,)");
    shortcuts.set_action_name(Some("app.show-shortcuts"));

    let new_workspace = icon_button("cmuxp-plus-symbolic", "New workspace (Ctrl+Shift+N)");
    new_workspace.set_action_name(Some("app.new-workspace"));

    let folder = gtk::Image::from_icon_name("cmuxp-folder-symbolic");
    folder.add_css_class("title-folder");
    let title = gtk::Label::builder().ellipsize(gtk::pango::EllipsizeMode::End).build();
    title.add_css_class("title-text");
    let title_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    title_box.add_css_class("title-box");
    title_box.append(&folder);
    title_box.append(&title);

    let header = gtk::HeaderBar::new();
    header.add_css_class("cmuxp-header");
    // The title goes on the left next to the buttons, like cmux, so the
    // centred title slot stays empty.
    header.set_title_widget(Some(&gtk::Box::new(gtk::Orientation::Horizontal, 0)));
    for w in [sidebar_toggle.upcast_ref::<gtk::Widget>(), search.upcast_ref(), bell.upcast_ref(), shortcuts.upcast_ref(), new_workspace.upcast_ref()] {
        header.pack_start(w);
    }
    header.pack_start(&title_box);

    Chrome {
        header,
        sidebar_toggle,
        bell,
        bell_badge,
        title,
        notifications,
        notifications_empty,
        mark_all_read,
        clear_all,
    }
}

fn notification_center() -> (gtk::Popover, gtk::ListBox, gtk::Label, gtk::Button, gtk::Button) {
    let heading = gtk::Label::builder().label("Notifications").xalign(0.0).hexpand(true).build();
    heading.add_css_class("popover-heading");
    let mark_all_read = gtk::Button::with_label("Mark all read");
    mark_all_read.add_css_class("flat");
    mark_all_read.add_css_class("text-button-small");
    let clear_all = gtk::Button::with_label("Clear");
    clear_all.add_css_class("flat");
    clear_all.add_css_class("text-button-small");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    top.add_css_class("popover-top");
    top.append(&heading);
    top.append(&mark_all_read);
    top.append(&clear_all);

    let list = gtk::ListBox::new();
    list.add_css_class("notification-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    let empty = gtk::Label::builder().label("You're all caught up").build();
    empty.add_css_class("empty-state");
    let scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .propagate_natural_height(true)
        .max_content_height(460)
        .child(&list)
        .build();

    let body = gtk::Box::new(gtk::Orientation::Vertical, 0);
    body.set_width_request(360);
    body.append(&top);
    body.append(&empty);
    body.append(&scroll);

    let popover = gtk::Popover::builder().child(&body).has_arrow(false).build();
    popover.add_css_class("cmuxp-popover");
    (popover, list, empty, mark_all_read, clear_all)
}

/// One entry in the notification center.
pub struct NoteRow<'a> {
    pub id: u64,
    pub title: &'a str,
    pub body: &'a str,
    pub workspace: &'a str,
    pub ago: String,
    pub read: bool,
}

pub fn note_row(n: &NoteRow) -> gtk::ListBoxRow {
    let dot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    dot.add_css_class("note-dot");
    dot.set_valign(gtk::Align::Center);
    dot.set_opacity(if n.read { 0.0 } else { 1.0 });
    let title = gtk::Label::builder().label(n.title).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).build();
    title.add_css_class("note-title");
    let ago = gtk::Label::new(Some(&n.ago));
    ago.add_css_class("note-meta");
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    top.append(&dot);
    top.append(&title);
    top.append(&ago);

    let body = gtk::Label::builder()
        .label(n.body)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .lines(2)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .visible(!n.body.is_empty())
        .build();
    body.add_css_class("note-body");
    let ws = gtk::Label::builder().label(n.workspace).xalign(0.0).ellipsize(gtk::pango::EllipsizeMode::End).build();
    ws.add_css_class("note-meta");

    let column = gtk::Box::new(gtk::Orientation::Vertical, 3);
    column.append(&top);
    column.append(&body);
    column.append(&ws);

    let row = gtk::ListBoxRow::builder().child(&column).activatable(true).build();
    row.add_css_class("note-row");
    if !n.read {
        row.add_css_class("unread");
    }
    row.set_widget_name(&format!("note-{}", n.id));
    row
}

/// The "?" button at the bottom of the sidebar: opens the Keyboard Shortcuts window.
pub fn help_button() -> gtk::Button {
    let button = icon_button("cmuxp-help-symbolic", "Keyboard shortcuts (Ctrl+,)");
    button.set_action_name(Some("app.show-shortcuts"));
    button.set_halign(gtk::Align::Start);
    button.add_css_class("help-button");
    button
}

/// "just now", "4m", "2h", "3d".
pub fn ago(secs: u64) -> String {
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m", secs / 60),
        3600..=86_399 => format!("{}h", secs / 3600),
        _ => format!("{}d", secs / 86_400),
    }
}
