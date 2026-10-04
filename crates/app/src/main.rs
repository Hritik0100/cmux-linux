//! cmuxp — a Linux terminal for running many coding agents side by side.
//!
//! Layout:  [ sidebar: one row per workspace ] | [ stack: one split tree per workspace ]
//!          each split-tree leaf is a pane with its own tab bar (terminals + browsers)

mod chrome;
mod git;
mod gitpanel;
mod icons;
mod ipc;
mod links;
mod palette;
mod panes;
mod sandbox;
mod search;
mod session;
mod shortcuts;
mod shortcuts_ui;
mod sidebar;
mod state;

#[cfg(feature = "browser")]
mod browser;

use std::cell::RefCell;
use std::rc::Rc;

use cmuxp_proto::Direction;
use gtk::prelude::*;
use gtk::{gdk, gio, glib};

use state::{AppState, Shared};

const APP_ID: &str = "dev.cmuxp.App";

fn main() -> glib::ExitCode {
    // The sandbox keeper is a hidden re-exec of this binary. Run it before any
    // GTK/glib initialization.
    if std::env::var_os("CMUXP_SANDBOX_KEEPER").is_some() {
        sandbox::namespace::keeper_main();
    }

    // GTK 4.16+ defaults to the Vulkan renderer. With VTE and WebKit that can
    // mean laggy terminals and blank web pages on some GPUs (seen on Intel
    // Iris + Mesa). The GL renderer is fast and reliable. Users can still
    // override it, e.g. GSK_RENDERER=vulkan cmuxp.
    if std::env::var_os("GSK_RENDERER").is_none() {
        std::env::set_var("GSK_RENDERER", "ngl");
    }
    // WebKit's sandboxed web process can stall forever while setting up
    // DMA-BUF sharing on hybrid-GPU laptops (blank page, no load events). The
    // shared-memory path keeps the sandbox on and always works.
    #[cfg(feature = "browser")]
    if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
        std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
    }
    // On hybrid laptops WebKit picks the first GPU (/dev/dri/renderD128), which
    // may be a discrete card that is powered off or driven by a broken driver.
    // Use the GPU that drives the display instead.
    #[cfg(feature = "browser")]
    if std::env::var_os("WEBKIT_WEB_RENDER_DEVICE_FILE").is_none() {
        if let Some(node) = display_render_node() {
            std::env::set_var("WEBKIT_WEB_RENDER_DEVICE_FILE", node);
        }
    }

    // CMUXP_NON_UNIQUE=1 runs an independent instance (e.g. a dev build next to
    // the installed app) instead of handing over to the one already running.
    let flags = if std::env::var_os("CMUXP_NON_UNIQUE").is_some() {
        gio::ApplicationFlags::NON_UNIQUE
    } else {
        gio::ApplicationFlags::default()
    };
    let app = gtk::Application::builder().application_id(APP_ID).flags(flags).build();
    app.connect_startup(|_| {
        let display = gdk::Display::default().expect("no display");
        icons::install(&display);
        gtk::Window::set_default_icon_name(icons::APP_ICON);
        load_css(&display);
        #[cfg(feature = "browser")]
        browser::init();
    });

    // GtkApplication is single-instance: launching `cmuxp` again re-activates
    // this process instead of starting a new one.
    let state: Rc<RefCell<Option<Shared>>> = Rc::default();
    app.connect_activate(move |app| {
        if let Some(shared) = state.borrow().as_ref() {
            shared.borrow().window.present();
            return;
        }
        *state.borrow_mut() = Some(build_ui(app));
    });

    app.run()
}

/// The /dev/dri/renderD* node of the GPU the firmware used for the display.
#[cfg(feature = "browser")]
fn display_render_node() -> Option<String> {
    std::fs::read_dir("/sys/class/drm").ok()?.flatten().find_map(|entry| {
        let name = entry.file_name().into_string().ok()?;
        let boot_vga = std::fs::read_to_string(entry.path().join("device/boot_vga")).ok()?;
        (name.starts_with("renderD") && boot_vga.trim() == "1").then(|| format!("/dev/dri/{name}"))
    })
}

fn load_css(display: &gdk::Display) {
    let provider = gtk::CssProvider::new();
    provider.load_from_string(include_str!("style.css"));
    // Above USER priority: themes (and ~/.config/gtk-4.0/gtk.css) otherwise
    // restyle our buttons, e.g. dimming icons until they vanish on a dark bar.
    gtk::style_context_add_provider_for_display(display, &provider, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);

    // Personal tweaks on top: ~/.config/cmuxp/style.css, or a file named by $CMUXP_CSS.
    let user_css = std::env::var_os("CMUXP_CSS")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| glib::user_config_dir().join("cmuxp").join("style.css"));
    if user_css.exists() {
        let user = gtk::CssProvider::new();
        user.load_from_path(&user_css);
        gtk::style_context_add_provider_for_display(display, &user, gtk::STYLE_PROVIDER_PRIORITY_USER + 2);
    }
}

fn build_ui(app: &gtk::Application) -> Shared {
    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("cmuxp")
        .default_width(1400)
        .default_height(900)
        .build();

    let sidebar = gtk::ListBox::new();
    sidebar.add_css_class("sidebar");
    sidebar.set_selection_mode(gtk::SelectionMode::Single);
    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&sidebar)
        .build();
    let sidebar_column = gtk::Box::new(gtk::Orientation::Vertical, 0);
    sidebar_column.add_css_class("sidebar-column");
    sidebar_column.append(&sidebar::header());
    sidebar_column.append(&sidebar_scroll);
    sidebar_column.append(&chrome::help_button());

    let chrome = chrome::build();
    window.set_titlebar(Some(&chrome.header));
    let (notifications, mark_all_read, clear_all, bell) =
        (chrome.notifications.clone(), chrome.mark_all_read.clone(), chrome.clear_all.clone(), chrome.bell.clone());

    let stack = gtk::Stack::new();
    stack.set_hexpand(true);

    let main = gtk::Paned::builder()
        .orientation(gtk::Orientation::Horizontal)
        .start_child(&sidebar_column)
        .end_child(&stack)
        .resize_start_child(false)
        .shrink_start_child(false)
        .position(280)
        .build();
    // The command palette floats over everything.
    let command_palette = palette::build();
    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&main));
    overlay.add_overlay(&command_palette.root);
    window.set_child(Some(&overlay));

    let socket_path = cmuxp_proto::default_socket_path();
    let shared: Shared = Rc::new(RefCell::new(AppState::new(
        app.clone(),
        window.clone(),
        sidebar.clone(),
        stack,
        socket_path.to_string_lossy().into_owned(),
        chrome,
        sidebar_column.clone().upcast(),
    )));

    let weak = Rc::downgrade(&shared);
    sidebar.connect_row_selected(move |sidebar, row| {
        let Some(id) = row.and_then(sidebar::Row::workspace_id) else { return };
        let sidebar = sidebar.clone();
        state::defer(&weak, move |s| {
            // By the time this runs another switch may have happened; acting on
            // a stale selection would make two switches undo each other forever.
            let still_selected = sidebar.selected_row().and_then(|r| sidebar::Row::workspace_id(&r)) == Some(id);
            let already_active = s.borrow().active == Some(id);
            if still_selected && !already_active {
                state::select_workspace(s, id);
            }
        });
    });

    // Notification center.
    let weak = Rc::downgrade(&shared);
    notifications.connect_row_activated(move |_, row| {
        let Some(id) = row.widget_name().strip_prefix("note-").and_then(|n| n.parse().ok()) else { return };
        state::defer(&weak, move |s| state::open_notification(s, id));
    });
    let weak = Rc::downgrade(&shared);
    mark_all_read.connect_clicked(move |_| state::defer(&weak, state::mark_all_notifications_read));
    let weak = Rc::downgrade(&shared);
    clear_all.connect_clicked(move |_| state::defer(&weak, state::clear_notifications));
    // Refresh "4m ago" labels each time the list opens.
    let weak = Rc::downgrade(&shared);
    bell.connect_active_notify(move |b| {
        if b.is_active() {
            state::defer(&weak, state::refresh_notifications);
        }
    });

    if let Err(e) = ipc::start(&socket_path, &shared) {
        eprintln!("cmuxp: control socket disabled ({}): {e}", socket_path.display());
    }
    window.connect_destroy(move |_| {
        let _ = std::fs::remove_file(&socket_path);
    });

    install_actions(app, &shared);

    // Keyboard shortcuts: defaults + ~/.config/cmuxp/shortcuts.toml (+ the user's custom ones).
    let keymap = shortcuts::Keymap::new(app, Rc::downgrade(&shared));
    shared.borrow_mut().keymap = Some(keymap.clone());
    let show = gio::SimpleAction::new("show-shortcuts", None);
    show.connect_activate({
        let (keymap, window) = (keymap.clone(), window.clone());
        move |_, _| shortcuts_ui::open(window.upcast_ref(), &keymap)
    });
    app.add_action(&show);
    keymap.apply();

    // Background-task tracking and sidebar refresh.
    let weak = Rc::downgrade(&shared);
    glib::timeout_add_seconds_local(1, move || match weak.upgrade() {
        Some(s) => {
            state::tick(&s);
            glib::ControlFlow::Continue
        }
        None => glib::ControlFlow::Break,
    });
    shared.borrow_mut().palette = Some(command_palette);
    // Reopen the last session, or start with one workspace.
    if !session::restore(&shared) {
        state::new_workspace(&shared, None, None);
    }
    // Save the layout when the window closes (and every few seconds in `tick`).
    window.connect_close_request({
        let weak = Rc::downgrade(&shared);
        move |_| {
            if let Some(s) = weak.upgrade() {
                session::save(&s);
            }
            glib::Propagation::Proceed
        }
    });
    window.present();
    shared
}

/// Every command is a `gio::SimpleAction` on the application, so buttons,
/// menus and keyboard shortcuts can all trigger it. Which keys trigger which
/// action is decided by `shortcuts::Keymap` (defaults + the user's file).
fn install_actions(app: &gtk::Application, shared: &Shared) {
    let add = |name: &str, f: fn(&Shared)| {
        let action = gio::SimpleAction::new(name, None);
        let weak = Rc::downgrade(shared);
        action.connect_activate(move |_, _| {
            if let Some(s) = weak.upgrade() {
                f(&s);
            }
        });
        app.add_action(&action);
    };

    add("new-workspace", |s| {
        state::new_workspace(s, None, None);
    });
    add("new-tab", |s| {
        let _ = state::new_tab(s, None, state::Content::Terminal);
    });
    add("close-tab", state::close_focused);
    add("next-tab", |s| state::cycle_tab(s, 1));
    add("prev-tab", |s| state::cycle_tab(s, -1));
    add("split-right", |s| {
        let _ = state::split(s, None, Direction::Right);
    });
    add("split-down", |s| {
        let _ = state::split(s, None, Direction::Down);
    });
    add("next-pane", state::focus_next_pane);
    add("next-workspace", |s| state::cycle_workspace(s, 1));
    add("prev-workspace", |s| state::cycle_workspace(s, -1));
    add("jump-unread", state::jump_to_unread);
    add("find", search::open);
    add("git-history", |s| state::toggle_git_panel(s, None));
    add("search", |s| {
        let palette = s.borrow().palette.clone();
        if let Some(p) = palette {
            p.open(s, None);
        }
    });
    add("command-palette", |s| {
        let palette = s.borrow().palette.clone();
        if let Some(p) = palette {
            p.open(s, Some(palette::Cat::Command));
        }
    });
    add("toggle-sidebar", |s| {
        state::toggle_sidebar(s);
    });
    add("toggle-network-sandbox", |s| {
        if let Some(ws) = s.borrow().active {
            let _ = state::set_workspace_sandbox(s, ws, None);
        }
    });
    add("notifications", |s| {
        let bell = s.borrow().chrome.bell.clone();
        bell.popup();
    });
    add("copy", |s| {
        state::with_focused_terminal(s, |t| {
            use vte::prelude::*;
            t.copy_clipboard_format(vte::Format::Text)
        })
    });
    add("paste", |s| {
        state::with_focused_terminal(s, |t| {
            use vte::prelude::*;
            t.paste_clipboard()
        })
    });
    #[cfg(feature = "browser")]
    {
        add("browser-tab", |s| {
            let _ = state::open_browser(s, None, state::DEFAULT_URL, None);
        });
        add("browser-split", |s| {
            let _ = state::open_browser(s, None, state::DEFAULT_URL, Some(Direction::Right));
        });
    }

    // Alt+1..9 jumps to workspace N.
    for n in 1..=9usize {
        let action = gio::SimpleAction::new(&format!("workspace-{n}"), None);
        let weak = Rc::downgrade(shared);
        action.connect_activate(move |_, _| {
            if let Some(s) = weak.upgrade() {
                state::select_workspace_index(&s, n - 1);
            }
        });
        app.add_action(&action);
    }
}
