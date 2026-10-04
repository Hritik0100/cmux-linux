//! In-app browser (WebKitGTK), shown as a tab like a terminal. Built with the `browser` feature.

use gtk::prelude::*;
use webkit6::prelude::*;

/// Turn on favicon storage once, so tabs can show site icons.
pub fn init() {
    if let Some(manager) = webkit6::NetworkSession::default().and_then(|s| s.website_data_manager()) {
        manager.set_favicons_enabled(true);
    }
}

/// Build a browser page. `on_new_window` is called with the URL when a page
/// asks for a new window (target=_blank, window.open); we open it as a tab.
pub fn new_browser(url: &str, on_new_window: impl Fn(String) + 'static) -> (gtk::Widget, webkit6::WebView) {
    let view = webkit6::WebView::new();
    view.set_hexpand(true);
    view.set_vexpand(true);
    if let Some(settings) = WebViewExt::settings(&view) {
        // Right-click → "Inspect Element" opens the Web Inspector.
        settings.set_enable_developer_extras(true);
    }

    let address = gtk::Entry::builder()
        .text(url)
        .hexpand(true)
        .placeholder_text("Search Google or enter address")
        .input_purpose(gtk::InputPurpose::Url)
        .build();
    address.add_css_class("address-bar");

    let back = crate::panes::icon_button("cmuxp-back-symbolic", "Back");
    let forward = crate::panes::icon_button("cmuxp-forward-symbolic", "Forward");
    let reload = crate::panes::icon_button("cmuxp-reload-symbolic", "Reload");

    let bar = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    bar.add_css_class("browser-bar");
    bar.append(&back);
    bar.append(&forward);
    bar.append(&reload);
    bar.append(&address);

    let progress = gtk::ProgressBar::new();
    progress.add_css_class("load-progress");
    progress.set_visible(false);

    let page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    page.append(&bar);
    page.append(&progress);
    page.append(&view);

    address.connect_activate({
        let view = view.clone();
        move |entry| {
            view.load_uri(&normalize_url(&entry.text()));
            view.grab_focus();
        }
    });
    back.connect_clicked({
        let view = view.clone();
        move |_| view.go_back()
    });
    forward.connect_clicked({
        let view = view.clone();
        move |_| view.go_forward()
    });
    reload.connect_clicked({
        let view = view.clone();
        move |_| view.reload()
    });
    view.connect_uri_notify({
        let address = address.clone();
        move |v| {
            if !address.has_focus() {
                address.set_text(&v.uri().unwrap_or_default());
            }
        }
    });
    view.connect_estimated_load_progress_notify({
        let progress = progress.clone();
        move |v| {
            let p = v.estimated_load_progress();
            progress.set_fraction(p);
            progress.set_visible(p < 1.0);
        }
    });
    if std::env::var_os("CMUXP_DEBUG").is_some() {
        view.connect_load_changed(|v, ev| eprintln!("browser: {ev:?} {:?}", v.uri()));
        view.connect_web_process_terminated(|_, reason| eprintln!("browser: web process terminated: {reason:?}"));
    }
    view.connect_load_failed(|_, _, uri, err| {
        eprintln!("cmuxp: failed to load {uri}: {err}");
        false
    });
    view.connect_create(move |_, action| {
        if let Some(uri) = action.clone().request().and_then(|r| r.uri()) {
            on_new_window(uri.to_string());
        }
        None
    });

    view.load_uri(&normalize_url(url));
    (page.upcast(), view)
}

/// "localhost:3000" → http, "example.com" → https, anything else → Google search.
pub fn normalize_url(input: &str) -> String {
    let mut input = input.trim().to_string();
    // WebKit refuses the unspecified address 0.0.0.0 ("restricted network
    // port"); it is a bind address, not a destination, and means loopback, so
    // rewrite it to 127.0.0.1 — with or without a scheme.
    if let Some(rest) = input.strip_prefix("0.0.0.0") {
        input = format!("127.0.0.1{rest}");
    } else if let Some(i) = input.find("://") {
        let (scheme, rest) = input.split_at(i + 3);
        if let Some(after) = rest.strip_prefix("0.0.0.0") {
            input = format!("{scheme}127.0.0.1{after}");
        }
    }

    if input.contains("://") || input.starts_with("about:") {
        input
    } else if input.starts_with("localhost") || input.starts_with("127.") {
        format!("http://{input}")
    } else if input.contains(' ') || !input.contains('.') {
        format!("https://www.google.com/search?q={}", url_encode(&input))
    } else {
        format!("https://{input}")
    }
}

fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            b' ' => "+".into(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::normalize_url;

    #[test]
    fn urls() {
        assert_eq!(normalize_url("localhost:3000"), "http://localhost:3000");
        assert_eq!(normalize_url("github.com"), "https://github.com");
        assert_eq!(normalize_url("rust gtk4"), "https://www.google.com/search?q=rust+gtk4");
        assert_eq!(normalize_url("https://x.dev/a"), "https://x.dev/a");
        // WebKit rejects 0.0.0.0; it becomes loopback.
        assert_eq!(normalize_url("0.0.0.0:3000"), "http://127.0.0.1:3000");
        assert_eq!(normalize_url("http://0.0.0.0:3000/"), "http://127.0.0.1:3000/");
    }
}
