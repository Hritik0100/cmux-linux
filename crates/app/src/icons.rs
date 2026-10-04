//! Bundled icons. They are compiled into the binary and written to a cache
//! directory (as a `hicolor` theme folder) at startup, which is added to
//! GTK's icon search path. Icons named
//! `*-symbolic` are recoloured by GTK to match the CSS `color`, so hover and
//! active states just work.

use gtk::gdk;

pub const APP_ICON: &str = "dev.cmuxp.App";

const ICONS: &[(&str, &str)] = &[
    ("dev.cmuxp.App.svg", include_str!("../../../data/icons/dev.cmuxp.App.svg")),
    ("cmuxp-terminal-symbolic.svg", include_str!("../icons/cmuxp-terminal-symbolic.svg")),
    ("cmuxp-globe-symbolic.svg", include_str!("../icons/cmuxp-globe-symbolic.svg")),
    ("cmuxp-split-right-symbolic.svg", include_str!("../icons/cmuxp-split-right-symbolic.svg")),
    ("cmuxp-split-down-symbolic.svg", include_str!("../icons/cmuxp-split-down-symbolic.svg")),
    ("cmuxp-close-symbolic.svg", include_str!("../icons/cmuxp-close-symbolic.svg")),
    ("cmuxp-plus-symbolic.svg", include_str!("../icons/cmuxp-plus-symbolic.svg")),
    ("cmuxp-back-symbolic.svg", include_str!("../icons/cmuxp-back-symbolic.svg")),
    ("cmuxp-forward-symbolic.svg", include_str!("../icons/cmuxp-forward-symbolic.svg")),
    ("cmuxp-reload-symbolic.svg", include_str!("../icons/cmuxp-reload-symbolic.svg")),
    ("cmuxp-sidebar-symbolic.svg", include_str!("../icons/cmuxp-sidebar-symbolic.svg")),
    ("cmuxp-bell-symbolic.svg", include_str!("../icons/cmuxp-bell-symbolic.svg")),
    ("cmuxp-folder-symbolic.svg", include_str!("../icons/cmuxp-folder-symbolic.svg")),
    ("cmuxp-help-symbolic.svg", include_str!("../icons/cmuxp-help-symbolic.svg")),
    ("cmuxp-keyboard-symbolic.svg", include_str!("../icons/cmuxp-keyboard-symbolic.svg")),
    ("cmuxp-copy-symbolic.svg", include_str!("../icons/cmuxp-copy-symbolic.svg")),
    ("cmuxp-paste-symbolic.svg", include_str!("../icons/cmuxp-paste-symbolic.svg")),
    ("cmuxp-pencil-symbolic.svg", include_str!("../icons/cmuxp-pencil-symbolic.svg")),
    ("cmuxp-undo-symbolic.svg", include_str!("../icons/cmuxp-undo-symbolic.svg")),
    ("cmuxp-trash-symbolic.svg", include_str!("../icons/cmuxp-trash-symbolic.svg")),
    ("cmuxp-warning-symbolic.svg", include_str!("../icons/cmuxp-warning-symbolic.svg")),
    ("cmuxp-up-symbolic.svg", include_str!("../icons/cmuxp-up-symbolic.svg")),
    ("cmuxp-down-symbolic.svg", include_str!("../icons/cmuxp-down-symbolic.svg")),
    ("cmuxp-search-symbolic.svg", include_str!("../icons/cmuxp-search-symbolic.svg")),
    ("cmuxp-git-symbolic.svg", include_str!("../icons/cmuxp-git-symbolic.svg")),
    ("cmuxp-file-symbolic.svg", include_str!("../icons/cmuxp-file-symbolic.svg")),
    ("cmuxp-clock-symbolic.svg", include_str!("../icons/cmuxp-clock-symbolic.svg")),
    ("cmuxp-commit-symbolic.svg", include_str!("../icons/cmuxp-commit-symbolic.svg")),
    ("cmuxp-branch-symbolic.svg", include_str!("../icons/cmuxp-branch-symbolic.svg")),
    ("cmuxp-external-symbolic.svg", include_str!("../icons/cmuxp-external-symbolic.svg")),
];

pub fn install(display: &gdk::Display) {
    let base = gtk::glib::user_cache_dir().join("cmuxp").join("icons");
    // Icons must live inside a theme folder (hicolor is every theme's
    // fallback). GTK draws loose files in the search path as plain images and
    // would not recolour the symbolic ones.
    let actions = base.join("hicolor/scalable/actions");
    let apps = base.join("hicolor/scalable/apps");
    for dir in [&actions, &apps] {
        if let Err(e) = std::fs::create_dir_all(dir) {
            eprintln!("cmuxp: cannot create {}: {e}", dir.display());
            return;
        }
    }
    for (name, svg) in ICONS {
        let dir = if name.ends_with("-symbolic.svg") { &actions } else { &apps };
        // Drop copies left at the top level by older versions.
        let _ = std::fs::remove_file(base.join(name));
        let path = dir.join(name);
        if std::fs::read_to_string(&path).ok().as_deref() != Some(*svg) {
            let _ = std::fs::write(&path, svg);
        }
    }
    gtk::IconTheme::for_display(display).add_search_path(&base);
}
