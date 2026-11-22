mod drafts;
mod files;
mod format;
mod recall_client;
mod ui;
mod util;

use cloudmail_api as api;
use cloudmail_api::config;

use gtk::{gdk, gio, glib, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;

/// The application's name on the session bus, which keeps it to one process: a second start, or a `mailto:` link,
/// reaches the window already open. GApplication only takes a reverse-DNS name with a dot in it, so this one stays
/// what it has always been; what the desktop knows the app by is [`NAME`].
pub const APP_ID: &str = "com.ferdousbhai.Cloudmail";
/// Omarchy's one name for the app: the command, the package, the `.desktop` file, the icon and the Wayland app id.
pub const NAME: &str = "cloudmail";
/// Set to start Cloudmail again once it quits (after the linked accounts change).
pub static RESTART: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn main() -> glib::ExitCode {
    register_catalogs();
    // A launcher entry or a `mailto:` default from before 0.8 names the old `.desktop` file; the app started some
    // other way puts that right too (the CLI does it as well).
    cloudmail_api::desktop::migrate_mimeapps();
    // The command line's name, which X11 and GTK's logs use, and which `omui::name_window` makes each window's Wayland
    // app id below.
    glib::set_prgname(Some(NAME));
    glib::set_application_name("Cloudmail");
    // omui's look is its own whatever GTK theme the desktop names, and GTK_THEME would load one under it. This is
    // what `omui::application` does first; the app builds its own application because it also opens mailto: links.
    // SAFETY: nothing else is running yet that could be reading the environment.
    unsafe { std::env::remove_var("GTK_THEME") };
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();
    // With OMUI_QA=1, omui drives the window the first activate or open shows (dev/qa.sh); before GTK starts, so what
    // it logs from the start is counted.
    #[cfg(feature = "qa")]
    omui::qa::attach(&app);
    app.connect_startup(|_| {
        if let Some(display) = gdk::Display::default() {
            omui::Theme::install(&display, include_str!("style.css"));
            // As `omui::application` does: a selectable label is often the first focus stop, and GTK would select
            // all of it as the window opens.
            gtk::Settings::for_display(&display).set_gtk_label_select_on_focus(false);
        }
        // After GTK has started, as `omui::application` does: the language decides which way the windows read.
        omui::Translator::install();
    });
    // Every window, the compose windows and dialogs too, tells the compositor it is `cloudmail` (the prgname above),
    // so that the launcher, Hyprland's window rules and the icon all match it by the one name; GTK alone would give
    // it the D-Bus id. `omui::application` does this for an app it builds, and this one builds its own for mailto:.
    app.connect_window_added(|_, window| omui::name_window(window));

    // The main window, made at the first activate or open. The app can outlive it for a moment (a send still under
    // way holds the application), so being started again then makes a new window rather than showing a closed one.
    let main_ui: Rc<RefCell<Option<Rc<ui::Ui>>>> = Rc::default();
    let ensure = move |app: &gtk::Application| {
        let open = main_ui.borrow().clone().filter(|ui| ui.is_open());
        open.unwrap_or_else(|| {
            let ui = ui::Ui::new(app, config::load());
            main_ui.replace(Some(ui.clone()));
            ui
        })
    };

    app.connect_activate({
        let ensure = ensure.clone();
        move |app| ensure(app).present()
    });

    app.connect_open(move |app, files, _| {
        let ui = ensure(app);
        ui.present();
        for file in files {
            let uri = file.uri();
            if uri.starts_with("mailto:") {
                ui::compose::open(&ui, util::parse_mailto(&uri));
            }
        }
    });

    let code = app.run();
    if RESTART.load(std::sync::atomic::Ordering::SeqCst)
        && let Ok(exe) = std::env::current_exe()
    {
        use std::os::unix::process::CommandExt;
        // Only returns when the new process couldn't start.
        let e = std::process::Command::new(exe).exec();
        eprintln!("{NAME}: couldn't restart: {e}");
    }
    code
}

/// Before anything is translated; omui's own strings are in its catalog. English alone for now: its counted strings
/// have their singular forms here.
fn register_catalogs() {
    omui::i18n::register_all(&[("en", include_str!("i18n/en.json"))]);
    // Scripted QA switches to a language with a few strings of its own (omui's `language` command in
    // dev/qa/language.jsonl), to see the window follow; no build that ships has it.
    #[cfg(feature = "qa")]
    omui::i18n::register("da", include_str!("../dev/qa/da.json"));
}

/// The catalogs, registered once for the tests that read translated text (the app registers them in `main`).
#[cfg(test)]
pub fn test_catalogs() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(register_catalogs);
}

/// What the window says when there is no configuration to read, in the language of the moment.
pub fn setup_hint(e: &cloudmail_api::Error) -> String {
    if e.kind != cloudmail_api::ErrorKind::Config {
        return e.to_string();
    }
    omui::tr!(
        "Cloudmail isn't configured yet.\n\n{}\n\nIf someone invited you, run `cloudmail connect <the invite link>` in a terminal. To run your own, `cloudmail setup you@yourdomain.com` deploys it to your Cloudflare account. Or just link Gmail, iCloud Mail or HEY with the accounts button in the sidebar to read those here. Then restart the app.",
        e
    )
}

#[cfg(test)]
mod tests {
    /// `en.json` lists exactly the strings the window translates, counting the app's wrappers that take a literal and
    /// translate it where it is shown.
    #[test]
    fn the_catalogs_match_the_sources() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        omui::i18n::CatalogCheck::new(root.join("src"), root.join("src/i18n"))
            .function("tip_button", 1, false)
            .function("bind_title", 1, false)
            .function("caption", 0, false)
            .arguments("confirm", &[1, 2, 3], false)
            .assert_ok();
    }
}
