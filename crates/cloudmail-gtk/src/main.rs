mod recall_client;
mod ui;
mod util;

use cloudmail_api as api;
use cloudmail_api::config;

use gtk::{gdk, gio, glib, prelude::*};
use std::cell::OnceCell;
use std::rc::Rc;

pub const APP_ID: &str = "com.ferdousbhai.Cloudmail";
/// Set to start Cloudmail again once it quits (after the linked accounts change).
pub static RESTART: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn main() -> glib::ExitCode {
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
        }
    });

    let main_ui: Rc<OnceCell<Rc<ui::Ui>>> = Rc::default();
    let ensure = move |app: &gtk::Application| {
        main_ui
            .get_or_init(|| ui::Ui::new(app, config::load().map_err(|e| setup_hint(&e))))
            .clone()
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
        eprintln!("cloudmail-gtk: couldn't restart: {e}");
    }
    code
}

fn setup_hint(e: &cloudmail_api::Error) -> String {
    if e.kind != cloudmail_api::ErrorKind::Config {
        return e.to_string();
    }
    format!(
        "Cloudmail isn't configured yet.\n\n{e}\n\nIf someone invited you, run `cloudmail connect <the invite link>` in a terminal. To run your own, `cloudmail setup you@yourdomain.com` deploys it to your Cloudflare account. Or just link Gmail, iCloud Mail or HEY with the accounts button in the sidebar to read those here. Then restart the app.",
    )
}
