use gtk::{gdk, gio, glib, glib::clone, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;

use super::Ui;
use crate::api::{Address, SendRequest};
use crate::util::{self, Draft};

struct Compose {
    window: gtk::Window,
    from: gtk::DropDown,
    from_emails: RefCell<Vec<String>>,
    to: gtk::Entry,
    cc: gtk::Entry,
    subject: gtk::Entry,
    body: gtk::TextView,
    error: gtk::Label,
    send: gtk::Button,
    /// To, Cc, Subject and body as the window opened, to tell whether anything was edited.
    initial: RefCell<[String; 4]>,
    reply_to_message_id: Option<String>,
}

fn fields(c: &Compose) -> [String; 4] {
    [c.to.text().to_string(), c.cc.text().to_string(), c.subject.text().to_string(), body_text(c)]
}

pub fn open(ui: &Rc<Ui>, draft: Draft) {
    if ui.client.is_none() {
        return;
    }
    let window = gtk::Window::builder()
        .title(if draft.reply_to_message_id.is_some() { "Reply" } else { "New message" })
        .transient_for(&ui.window)
        .default_width(760)
        .default_height(600)
        .build();
    window.add_css_class("compose");

    let grid = gtk::Grid::builder().row_spacing(8).column_spacing(10).margin_top(14).margin_bottom(14).margin_start(14).margin_end(14).build();
    let field = |row: i32, name: &str, widget: &gtk::Widget| {
        let label = gtk::Label::builder().label(name).xalign(1.0).build();
        label.add_css_class("field-label");
        grid.attach(&label, 0, row, 1, 1);
        widget.set_hexpand(true);
        grid.attach(widget, 1, row, 1, 1);
    };

    let from = gtk::DropDown::from_strings(&[]);
    let to = gtk::Entry::builder().text(&draft.to).placeholder_text("name@example.com, …").build();
    let cc = gtk::Entry::builder().text(&draft.cc).build();
    let subject = gtk::Entry::builder().text(&draft.subject).build();
    field(0, "From", from.upcast_ref());
    field(1, "To", to.upcast_ref());
    field(2, "Cc", cc.upcast_ref());
    field(3, "Subject", subject.upcast_ref());

    let body = gtk::TextView::builder().wrap_mode(gtk::WrapMode::WordChar).accepts_tab(false).vexpand(true).build();
    body.buffer().set_text(&draft.body);
    body.buffer().place_cursor(&body.buffer().start_iter());
    let scroller = gtk::ScrolledWindow::builder().child(&body).vexpand(true).hexpand(true).build();
    grid.attach(&scroller, 0, 4, 2, 1);

    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let error = gtk::Label::builder().xalign(0.0).hexpand(true).wrap(true).build();
    error.add_css_class("error");
    let hint = gtk::Label::new(Some("Ctrl+Enter to send · Esc to close"));
    hint.add_css_class("dim");
    let send = gtk::Button::with_label("Send");
    send.add_css_class("suggested");
    bottom.append(&error);
    bottom.append(&hint);
    bottom.append(&send);
    grid.attach(&bottom, 0, 5, 2, 1);
    window.set_child(Some(&grid));
    window.set_default_widget(Some(&send));

    let c = Rc::new(Compose {
        window,
        from,
        from_emails: Default::default(),
        to,
        cc,
        subject,
        body,
        error,
        send,
        initial: Default::default(),
        reply_to_message_id: draft.reply_to_message_id,
    });

    *c.initial.borrow_mut() = fields(&c);
    // The title-bar close button and the compositor's close (Super+W) ask first, like Esc.
    c.window.connect_close_request(clone!(#[weak] c, #[upgrade_or] glib::Propagation::Proceed, move |_| {
        if is_dirty(&c) {
            close(&c);
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    }));

    // Signal handlers only hold weak refs; the window owns the compose state.
    let keep = RefCell::new(Some(c.clone()));
    c.window.connect_destroy(move |_| drop(keep.take()));

    fill_from(&c, ui, draft.from.as_deref());
    if ui.identities.borrow().is_none() {
        let client = ui.client.clone().unwrap();
        let wanted = draft.from;
        util::run(
            move || client.identities(),
            clone!(#[weak] ui, #[strong] c, move |result| {
                if let Ok(ids) = result {
                    *ui.identities.borrow_mut() = Some(ids);
                    if c.window.is_visible() {
                        fill_from(&c, &ui, wanted.as_deref());
                    }
                }
            }),
        );
    }

    c.send.connect_clicked(clone!(#[weak] ui, #[weak] c, move |_| send_message(&c, &ui)));

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(clone!(
        #[weak] ui,
        #[weak] c,
        #[upgrade_or] glib::Propagation::Proceed,
        move |_, key, _, mods| {
            if matches!(key, gdk::Key::Return | gdk::Key::KP_Enter) && mods.contains(gdk::ModifierType::CONTROL_MASK) {
                send_message(&c, &ui);
                return glib::Propagation::Stop;
            }
            if key == gdk::Key::Escape {
                // Esc in an open dropdown (From) closes the dropdown, not the draft.
                let in_popover = gtk::prelude::GtkWindowExt::focus(&c.window)
                    .is_some_and(|w| w.ancestor(gtk::Popover::static_type()).is_some());
                if in_popover {
                    return glib::Propagation::Proceed;
                }
                close(&c);
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        }
    ));
    c.window.add_controller(keys);

    ui.composes.borrow_mut().retain(|w| w.upgrade().is_some());
    ui.composes.borrow_mut().push(c.window.downgrade());
    c.window.present();
    if draft.to.is_empty() {
        c.to.grab_focus();
    } else if draft.subject.is_empty() {
        c.subject.grab_focus();
    } else {
        c.body.grab_focus();
    }
}

fn fill_from(c: &Compose, ui: &Ui, wanted: Option<&str>) {
    let ids = ui.identities.borrow();
    let mut options: Vec<Address> = Vec::new();
    if let Some(ids) = ids.as_ref() {
        for a in ids.default.iter().chain(&ids.identities) {
            if !options.iter().any(|o| o.email.eq_ignore_ascii_case(&a.email)) {
                options.push(a.clone());
            }
        }
    }
    if let Some(w) = wanted
        && !options.iter().any(|o| o.email.eq_ignore_ascii_case(w)) {
            let name = ids.as_ref().and_then(|i| i.default.as_ref()).and_then(|d| d.name.clone());
            options.push(Address { name, email: w.to_string() });
        }
    let labels: Vec<String> = options.iter().map(Address::formatted).collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    c.from.set_model(Some(&gtk::StringList::new(&refs)));
    let selected = wanted
        .and_then(|w| options.iter().position(|o| o.email.eq_ignore_ascii_case(w)))
        .unwrap_or(0);
    c.from.set_selected(selected as u32);
    *c.from_emails.borrow_mut() = options.into_iter().map(|a| a.email).collect();
}

fn body_text(c: &Compose) -> String {
    let buffer = c.body.buffer();
    buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string()
}

fn send_message(c: &Rc<Compose>, ui: &Rc<Ui>) {
    if !c.send.is_sensitive() {
        return;
    }
    let Some(client) = ui.client.clone() else { return };
    let to = util::split_addresses(&c.to.text());
    if to.is_empty() {
        c.error.set_label("Add at least one recipient.");
        c.to.grab_focus();
        return;
    }
    let text = body_text(c);
    if text.trim().is_empty() {
        c.error.set_label("The message is empty.");
        return;
    }
    let from = c.from_emails.borrow().get(c.from.selected() as usize).cloned();
    let req = SendRequest {
        from,
        to,
        cc: util::split_addresses(&c.cc.text()),
        bcc: Vec::new(),
        subject: c.subject.text().to_string(),
        text,
        reply_to_message_id: c.reply_to_message_id.clone(),
    };
    c.error.set_label("");
    c.send.set_sensitive(false);
    c.send.set_label("Sending…");
    util::run(
        move || client.send(&req),
        clone!(#[weak] ui, #[strong] c, move |result: Result<crate::api::SendResponse, String>| match result {
            Ok(resp) => {
                c.window.destroy();
                match &resp.warning {
                    Some(w) => ui.toast(&format!("Sent, but {w}")),
                    None => ui.toast("Sent"),
                }
                ui.after_send(resp.thread_id.as_deref());
            }
            Err(e) => {
                c.send.set_sensitive(true);
                c.send.set_label("Send");
                c.error.set_label(&format!("Couldn't send: {e}"));
            }
        }),
    );
}

fn is_dirty(c: &Compose) -> bool {
    let now = fields(c);
    let initial = c.initial.borrow();
    now.iter().zip(initial.iter()).any(|(a, b)| a.trim() != b.trim()) && now.iter().any(|f| !f.trim().is_empty())
}

fn close(c: &Rc<Compose>) {
    if !is_dirty(c) {
        c.window.destroy();
        return;
    }
    let dialog = gtk::AlertDialog::builder()
        .message("Discard this draft?")
        .detail("Drafts aren't saved.")
        .buttons(["Keep editing", "Discard"])
        .cancel_button(0)
        .default_button(0)
        .modal(true)
        .build();
    let window = c.window.clone();
    dialog.choose(Some(&c.window), gio::Cancellable::NONE, move |res| {
        if res == Ok(1) {
            window.destroy();
        }
    });
}
