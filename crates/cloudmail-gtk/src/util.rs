pub use cloudmail_api::text::{
    format_addresses, html_to_text, human_size, long_time, quote, reply_subject, short_time, split_addresses,
    unused_path,
};
use gtk::{gio, glib};

/// Runs `work` on a worker thread and hands its result to `done` on the GTK main loop.
pub fn run<T, E, W, D>(work: W, done: D)
where
    T: Send + 'static,
    E: Into<String> + Send + 'static,
    W: FnOnce() -> Result<T, E> + Send + 'static,
    D: FnOnce(Result<T, String>) + 'static,
{
    glib::spawn_future_local(async move {
        let result = gio::spawn_blocking(move || work().map_err(Into::into))
            .await
            .unwrap_or_else(|_| Err("background task panicked".into()));
        done(result);
    });
}

#[derive(Debug, Default, Clone)]
pub struct Draft {
    pub to: String,
    pub cc: String,
    pub subject: String,
    pub body: String,
    pub reply_to_message_id: Option<String>,
    pub from: Option<String>,
}

pub fn parse_mailto(uri: &str) -> Draft {
    // GIO hands us "mailto:///a@b.com" for URIs passed on the command line.
    let rest = uri.strip_prefix("mailto:").unwrap_or(uri).trim_start_matches('/');
    let (addr, query) = rest.split_once('?').unwrap_or((rest, ""));
    let decode = |s: &str| urlencoding::decode(s).map(|c| c.into_owned()).unwrap_or_else(|_| s.to_string());
    let mut draft = Draft { to: decode(addr), ..Default::default() };
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        let v = decode(v);
        match k.to_ascii_lowercase().as_str() {
            "to" => {
                if draft.to.is_empty() {
                    draft.to = v;
                } else {
                    draft.to = format!("{}, {v}", draft.to);
                }
            }
            "cc" => draft.cc = v,
            "subject" => draft.subject = v,
            "body" => draft.body = v,
            _ => {}
        }
    }
    draft
}

pub fn notify(title: &str, body: &str) {
    // Notification daemons read the body as Pango markup; `--` keeps a subject like "-20% off"
    // from being taken for an option.
    let title = title.to_string();
    let body = escape_html(body);
    std::thread::spawn(move || {
        let _ = std::process::Command::new("notify-send")
            .args(["-a", "Cloudmail", "-i", crate::APP_ID, "--", &title, &body])
            .status();
    });
}

pub fn open_uri(uri: &str) {
    if let Err(e) = gio::AppInfo::launch_default_for_uri(uri, gio::AppLaunchContext::NONE) {
        eprintln!("could not open {uri}: {e}");
    }
}

pub fn subject_or_placeholder(subject: &str) -> &str {
    if subject.trim().is_empty() { "(no subject)" } else { subject }
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mailto() {
        let d = parse_mailto("mailto:foo@bar.com?subject=Hello%20there&cc=x@y.z&body=Hi%20you");
        assert_eq!(parse_mailto("mailto:a+tag@b.com").to, "a+tag@b.com");
        assert_eq!(parse_mailto("mailto:///a@b.com?subject=x").to, "a@b.com");
        assert_eq!(d.to, "foo@bar.com");
        assert_eq!(d.subject, "Hello there");
        assert_eq!(d.cc, "x@y.z");
        assert_eq!(d.body, "Hi you");
    }
}
