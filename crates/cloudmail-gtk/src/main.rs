mod theme;
mod ui;
mod util;

use cloudmail_api as api;
use cloudmail_api::config;

use gtk::{gio, glib, prelude::*};
use std::cell::OnceCell;
use std::rc::Rc;

pub const APP_ID: &str = "com.ferdousbhai.Cloudmail";

fn main() -> glib::ExitCode {
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::HANDLES_OPEN)
        .build();

    let main_ui: Rc<OnceCell<Rc<ui::Ui>>> = Rc::default();
    let ensure = move |app: &gtk::Application| main_ui.get_or_init(|| ui::Ui::new(app, config::load().map_err(|e| setup_hint(&e)))).clone();

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

    app.run()
}

fn setup_hint(e: &cloudmail_api::Error) -> String {
    if e.kind != cloudmail_api::ErrorKind::Config {
        return e.to_string();
    }
    format!(
        "Cloudmail isn't configured yet.\n\n{e}\n\nRun `cloudmail setup` to deploy a worker, or create {} with:\n\napi_url = \"https://cloudmail.<you>.workers.dev\"\napi_token = \"<the worker's API_TOKEN secret>\"\n\nthen restart the app.",
        config::path().display()
    )
}
