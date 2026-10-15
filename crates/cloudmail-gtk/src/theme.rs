use gtk::{gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;

#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    pub dark: bool,
    pub accent: String,
    pub selection: String,
    pub muted: String,
    pub background: String,
    pub dark_background: String,
    pub lighter_background: String,
    pub foreground: String,
    pub dark_foreground: String,
    pub bright_foreground: String,
    pub red: String,
    pub green: String,
}

impl Default for Palette {
    fn default() -> Self {
        // tokyo-night
        Self {
            dark: true,
            accent: "#7aa2f7".into(),
            selection: "#292e42".into(),
            muted: "#414868".into(),
            background: "#1a1b26".into(),
            dark_background: "#13141c".into(),
            lighter_background: "#24283b".into(),
            foreground: "#a9b1d6".into(),
            dark_foreground: "#565f89".into(),
            bright_foreground: "#c0caf5".into(),
            red: "#f7768e".into(),
            green: "#9ece6a".into(),
        }
    }
}

fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".local/state"))
        .join("omarchy/current")
}

fn is_color(v: &str) -> bool {
    v.starts_with('#') && gdk::RGBA::parse(v).is_ok()
}

pub fn load() -> Palette {
    let mut p = Palette::default();
    let Ok(text) = std::fs::read_to_string(state_dir().join("theme/colors.toml")) else {
        return p;
    };
    let Ok(map) = toml::from_str::<HashMap<String, toml::Value>>(&text) else {
        return p;
    };
    let get = |k: &str| map.get(k).and_then(|v| v.as_str()).map(str::to_string);
    if let Some(mode) = get("mode") {
        p.dark = mode != "light";
    }
    let slots: [(&str, &mut String); 11] = [
        ("accent", &mut p.accent),
        ("selection", &mut p.selection),
        ("muted", &mut p.muted),
        ("background", &mut p.background),
        ("dark_background", &mut p.dark_background),
        ("lighter_background", &mut p.lighter_background),
        ("foreground", &mut p.foreground),
        ("dark_foreground", &mut p.dark_foreground),
        ("bright_foreground", &mut p.bright_foreground),
        ("red", &mut p.red),
        ("green", &mut p.green),
    ];
    for (key, slot) in slots {
        if let Some(v) = get(key).filter(|v| is_color(v)) {
            *slot = v;
        }
    }
    p
}

pub fn css(p: &Palette) -> String {
    format!(
        r#"
window.cloudmail, window.compose {{
  background: {bg};
  color: {fg};
  font-family: monospace;
  font-size: 10pt;
}}
.sidebar {{ background: {bg}; border-right: 1px solid alpha({fg}, 0.10); padding: 10px 0; }}
.sidebar .brand {{ color: {fg}; font-weight: 600; padding: 4px 18px 14px 18px; }}
.sidebar list {{ background: transparent; }}
.sidebar row {{ padding: 9px 12px; margin: 1px 8px; border-radius: 6px; color: {fg}; }}
.sidebar row:hover {{ background: {lbg}; }}
.sidebar row:selected {{ background: alpha({accent}, 0.12); color: {accent}; }}
.sidebar button.compose {{ margin: 0 12px 12px 12px; padding: 6px 10px; }}
.sidebar .hint {{ color: alpha({fg}, 0.65); font-size: 9pt; padding: 10px 18px; }}
.badge {{ background: {accent}; color: {dbg}; border-radius: 9px; padding: 0 7px; font-weight: bold; font-size: 9pt; }}
.badge.dim {{ background: {muted}; color: {bfg}; }}
.account-badge {{ color: alpha({fg}, 0.65); font-size: 7.5pt; border: 1px solid alpha({fg}, 0.16); border-radius: 4px; padding: 0 4px; }}
.nav-heading {{ color: alpha({fg}, 0.65); font-size: 8pt; font-weight: bold; padding: 12px 18px 2px 18px; }}
.account-warning {{ color: {red}; font-size: 9pt; padding: 0 14px 8px 14px; }}
.account-sign-in {{ padding: 0 14px 8px 14px; }}
.column-title {{ font-weight: bold; color: {bfg}; padding: 12px 14px 4px 14px; }}
.column-sub {{ color: alpha({fg}, 0.65); font-size: 9pt; padding: 0 14px 8px 14px; }}
.threadlist, .threadlist list {{ background: {bg}; }}
.threadlist row {{ padding: 12px 16px; border-left: 2px solid transparent; border-bottom: 1px solid alpha({fg}, 0.10); }}
.threadlist row:hover {{ background: alpha({fg}, 0.04); }}
.threadlist row:selected {{ background: alpha({accent}, 0.10); border-left-color: {accent}; }}
.threadlist row:focus-visible {{ outline: 1px solid alpha({accent}, 0.7); outline-offset: -1px; }}
.sender {{ color: {fg}; }}
.unread .sender {{ color: {bfg}; font-weight: bold; }}
.unread-dot {{ color: {accent}; }}
.subject {{ color: {fg}; }}
.unread .subject {{ color: {bfg}; }}
.snippet, .time, .dim {{ color: alpha({fg}, 0.65); }}
.time {{ font-size: 9pt; }}
.empty {{ color: alpha({fg}, 0.65); padding: 40px; }}
.error {{ color: {red}; }}
.reader-bar {{ padding: 8px 12px; border-bottom: 1px solid alpha({fg}, 0.10); background: {bg}; }}
.placeholder {{ color: alpha({fg}, 0.65); }}
.setup {{ color: {fg}; padding: 30px; }}
.setup-title {{ color: {accent}; font-weight: bold; font-size: 14pt; }}
entry, searchentry, textview, textview text, dropdown > button {{
  background: alpha({fg}, 0.025); color: {bfg}; caret-color: {accent};
}}
entry, searchentry {{ min-height: 32px; border: 1px solid alpha({fg}, 0.16); border-radius: 6px; box-shadow: none; }}
entry:focus-within, searchentry:focus-within {{ border-color: {accent}; outline: none; }}
.search {{ margin: 10px 12px 4px 12px; }}
.compose textview {{ padding: 10px; border-radius: 6px; font-family: monospace; }}
.compose .field-label {{ color: alpha({fg}, 0.65); }}
.compose .attachment-chip {{ background: {lbg}; border: 1px solid alpha({fg}, 0.16); border-radius: 6px; padding: 0 0 0 8px; }}
.compose .attachment-chip button {{ padding: 0 7px; min-height: 0; }}
.compose flowboxchild {{ padding: 0; }}
window.compose:drop(active) {{ box-shadow: inset 0 0 0 2px {accent}; }}
button {{ background: transparent; color: {fg}; border: 1px solid alpha({fg}, 0.16); border-radius: 6px; box-shadow: none; padding: 5px 10px; min-height: 24px; font-weight: 500; }}
button:hover {{ background: alpha({fg}, 0.07); color: {bfg}; }}
button:focus-visible {{ outline: 1px solid {accent}; outline-offset: 2px; }}
button.suggested {{ background: alpha({accent}, 0.14); color: {accent}; border-color: alpha({accent}, 0.35); font-weight: 600; }}
button.approve {{ color: {green}; border-color: {green}; background: transparent; }}
button.block {{ color: {red}; border-color: {red}; background: transparent; }}
button.approve:hover {{ background: {green}; color: {dbg}; }}
button.block:hover {{ background: {red}; color: {dbg}; }}
button.flat {{ border-color: transparent; background: transparent; }}
button.flat:hover {{ background: {lbg}; }}
paned > separator {{ background: {lbg}; min-width: 1px; }}
scrollbar {{ background: transparent; }}
scrollbar slider {{ background: {muted}; border-radius: 6px; min-width: 6px; }}
.shortcuts {{ background: {bg}; color: {fg}; padding: 16px; border-radius: 8px; }}
.shortcuts .key {{ color: {accent}; font-weight: bold; }}
popover > contents {{ background: {bg}; color: {fg}; }}
"#,
        bg = p.background,
        dbg = p.dark_background,
        lbg = p.lighter_background,
        fg = p.foreground,
        bfg = p.bright_foreground,
        muted = p.muted,
        accent = p.accent,
        red = p.red,
        green = p.green,
    )
}

thread_local! {
    static PROVIDER: RefCell<Option<gtk::CssProvider>> = const { RefCell::new(None) };
    static MONITORS: RefCell<Vec<gio::FileMonitor>> = const { RefCell::new(Vec::new()) };
    static PENDING: RefCell<Option<glib::SourceId>> = const { RefCell::new(None) };
}

pub fn apply(p: &Palette) {
    let Some(display) = gdk::Display::default() else { return };
    PROVIDER.with(|cell| {
        let mut cell = cell.borrow_mut();
        let provider = cell.get_or_insert_with(|| {
            let provider = gtk::CssProvider::new();
            gtk::style_context_add_provider_for_display(&display, &provider, gtk::STYLE_PROVIDER_PRIORITY_USER);
            provider
        });
        provider.load_from_string(&css(p));
    });
    if let Some(settings) = gtk::Settings::default() {
        settings.set_gtk_application_prefer_dark_theme(p.dark);
    }
}

/// Watches Omarchy's current-theme directory and calls `on_change` (debounced)
/// whenever the palette changes. Omarchy swaps `current/theme` by `mv`, so we
/// watch `current/` and re-arm the watch on `current/theme` after each change.
pub fn watch(on_change: impl Fn(Palette) + 'static) {
    let on_change = Rc::new(on_change);
    let last = Rc::new(RefCell::new(load()));
    arm(on_change, last);
}

fn arm(on_change: Rc<dyn Fn(Palette)>, last: Rc<RefCell<Palette>>) {
    let dir = state_dir();
    let mut monitors = Vec::new();
    for path in [dir.clone(), dir.join("theme")] {
        let Ok(monitor) =
            gio::File::for_path(&path).monitor_directory(gio::FileMonitorFlags::WATCH_MOVES, gio::Cancellable::NONE)
        else {
            continue;
        };
        let on_change = on_change.clone();
        let last = last.clone();
        monitor.connect_changed(move |_, _, _, _| {
            let on_change = on_change.clone();
            let last = last.clone();
            PENDING.with(|p| {
                if let Some(id) = p.borrow_mut().take() {
                    id.remove();
                }
                let id = glib::timeout_add_local_once(std::time::Duration::from_millis(400), move || {
                    PENDING.with(|p| p.borrow_mut().take());
                    let palette = load();
                    if *last.borrow() != palette {
                        *last.borrow_mut() = palette.clone();
                        on_change(palette);
                    }
                    arm(on_change, last);
                });
                *p.borrow_mut() = Some(id);
            });
        });
        monitors.push(monitor);
    }
    MONITORS.with(|m| *m.borrow_mut() = monitors);
}
