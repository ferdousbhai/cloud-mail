//! Debug-build-only automation for smoke tests: CLOUDMAIL_SCRIPT="j;shot:/tmp/a.png;quit"
//! Steps run 1.5s apart. A step is a single key, `enter`, `esc`, `shot:<png>` (main window),
//! `shotcompose:<png>`, `send` (activates the open compose window's Send), or `quit`.

use gtk::{gdk, glib, prelude::*};
use std::rc::Rc;
use std::time::Duration;

use super::Ui;

pub fn start(ui: &Rc<Ui>) {
    let Ok(script) = std::env::var("CLOUDMAIL_SCRIPT") else { return };
    let steps: Vec<String> = script.split(';').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    run(Rc::downgrade(ui), steps, 0);
}

fn run(ui: std::rc::Weak<Ui>, steps: Vec<String>, i: usize) {
    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
        let Some(ui_rc) = ui.upgrade() else { return };
        let Some(step) = steps.get(i) else { return };
        eprintln!("script: {step}");
        match step.as_str() {
            "quit" => {
                if let Some(a) = ui_rc.window.application() { a.quit() }
                return;
            }
            "enter" => {
                ui_rc.on_key(gdk::Key::Return, gdk::ModifierType::empty());
            }
            "esc" => {
                ui_rc.on_key(gdk::Key::Escape, gdk::ModifierType::empty());
            }
            "send" => {
                if let Some(b) = compose_window().and_then(|w| w.default_widget()).and_downcast::<gtk::Button>() {
                    b.emit_clicked();
                }
            }
            s if s.starts_with("shot:") => shot(ui_rc.window.upcast_ref(), &s[5..]),
            s if s.starts_with("shotcompose:") => {
                if let Some(w) = compose_window() {
                    shot(w.upcast_ref(), &s[12..]);
                }
            }
            s => {
                if let Some(ch) = s.chars().next() {
                    let key = unsafe { glib::translate::from_glib(gdk::unicode_to_keyval(ch as u32)) };
                    ui_rc.on_key(key, gdk::ModifierType::empty());
                }
            }
        }
        run(ui, steps, i + 1);
    });
}

fn compose_window() -> Option<gtk::Window> {
    gtk::Window::list_toplevels()
        .into_iter()
        .filter_map(|w| w.downcast::<gtk::Window>().ok())
        .find(|w| w.has_css_class("compose") && w.is_visible())
}

fn shot(widget: &gtk::Widget, path: &str) {
    let paintable = gtk::WidgetPaintable::new(Some(widget));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, widget.width() as f64, widget.height() as f64);
    let Some(node) = snapshot.to_node() else {
        eprintln!("script: nothing to capture (mapped: {})", widget.is_mapped());
        return;
    };
    let Some(renderer) = widget.native().and_then(|n| n.renderer()) else { return };
    let texture = renderer.render_texture(&node, None);
    if let Err(e) = texture.save_to_png(path) {
        eprintln!("script: could not save {path}: {e}");
    }
}
