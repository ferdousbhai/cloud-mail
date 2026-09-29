use gtk::{gdk, glib, prelude::*};

const KEYS: &[(&str, &str)] = &[
    ("j / k", "Next / previous"),
    ("Enter, o", "Focus the message (scroll with arrows/space)"),
    ("e", "Archive"),
    ("i", "Move to Inbox"),
    ("u", "Toggle unread"),
    ("r", "Reply"),
    ("a", "Reply all"),
    ("c", "Compose"),
    ("y / n", "Screener: let in / block"),
    ("1 2 3 4", "Screener · Inbox · Archive · Sent"),
    ("/", "Search"),
    ("Esc", "Clear search"),
    ("L", "Load remote images"),
    ("R", "Refresh"),
    ("Ctrl+Enter", "Send (in compose)"),
    ("Ctrl+Q", "Quit"),
    ("?", "This help"),
];

pub fn show(parent: &gtk::Window) {
    let grid = gtk::Grid::builder().row_spacing(6).column_spacing(24).build();
    grid.add_css_class("shortcuts");
    for (i, (key, what)) in KEYS.iter().enumerate() {
        let k = gtk::Label::builder().label(*key).xalign(1.0).build();
        k.add_css_class("key");
        grid.attach(&k, 0, i as i32, 1, 1);
        grid.attach(&gtk::Label::builder().label(*what).xalign(0.0).build(), 1, i as i32, 1, 1);
    }
    let window = gtk::Window::builder()
        .title("Keyboard shortcuts")
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .child(&grid)
        .build();
    window.add_css_class("cloudmail");
    let keys = gtk::EventControllerKey::new();
    let w = window.clone();
    keys.connect_key_pressed(move |_, key, _, _| {
        if matches!(key, gdk::Key::Escape | gdk::Key::question | gdk::Key::q) {
            w.close();
            return glib::Propagation::Stop;
        }
        glib::Propagation::Proceed
    });
    window.add_controller(keys);
    window.present();
}
