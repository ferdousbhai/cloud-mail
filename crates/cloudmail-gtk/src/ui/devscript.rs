//! Debug-build-only automation that puts the app in a scene for dev/preview.sh's pictures: CLOUDMAIL_SCRIPT="j;#"
//! Steps run 1.5s apart. A step is a single key, `enter`, `esc`, `people` (opens the People window), `drop:<file>`
//! (drops the file on the open compose window, as dragging it there would), `unattach:<n>` (clicks the nth chip's
//! remove button), or `quit`. The flows themselves, and pictures of the main window, are omui's scripted QA's
//! (dev/qa); it cannot drop a file, so dropping stays here.

use gtk::{gdk, glib, prelude::*};
use std::rc::Rc;
use std::time::Duration;

use super::Ui;

pub fn start(ui: &Rc<Ui>) {
    let Ok(script) = std::env::var("CLOUDMAIL_SCRIPT") else {
        return;
    };
    let steps: Vec<String> = script
        .split(';')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    run(Rc::downgrade(ui), steps, 0);
}

fn run(ui: std::rc::Weak<Ui>, steps: Vec<String>, i: usize) {
    glib::timeout_add_local_once(Duration::from_millis(1500), move || {
        let Some(ui_rc) = ui.upgrade() else { return };
        let Some(step) = steps.get(i) else { return };
        eprintln!("script: {step}");
        match step.as_str() {
            "quit" => {
                if let Some(a) = ui_rc.window.application() {
                    a.quit()
                }
                return;
            }
            "enter" => {
                ui_rc.on_key(gdk::Key::Return, gdk::ModifierType::empty());
            }
            "esc" => {
                ui_rc.on_key(gdk::Key::Escape, gdk::ModifierType::empty());
            }
            s if let Some(path) = s.strip_prefix("drop:") => {
                eprintln!("script: dropped {path}: {}", super::compose::drop_on_open(path));
            }
            s if let Some(i) = s.strip_prefix("unattach:").and_then(|i| i.parse().ok()) => {
                eprintln!(
                    "script: removed chip {i}: {}; now {:?}",
                    super::compose::unattach_on_open(i),
                    super::compose::open_state()
                );
            }
            "people" => super::people::show(&ui_rc),
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
