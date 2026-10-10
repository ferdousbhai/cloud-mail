use gtk::{gdk, gio, glib, glib::clone, prelude::*};
use std::cell::RefCell;
use std::rc::Rc;

use super::Ui;
use crate::api::{Address, OutgoingAttachment, SendRequest};
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
    /// The files to send, each shown as a removable chip in `chips`.
    attachments: RefCell<Vec<OutgoingAttachment>>,
    chips: gtk::FlowBox,
    /// To, Cc, Subject and body as the window opened, to tell whether anything was edited.
    initial: RefCell<[String; 4]>,
    reply_to_message_id: Option<String>,
}

fn fields(c: &Compose) -> [String; 4] {
    [c.to.text().to_string(), c.cc.text().to_string(), c.subject.text().to_string(), body_text(c)]
}

#[cfg(debug_assertions)]
thread_local! {
    /// Open compose windows, for the smoke-test script (which can't drive a file chooser).
    static OPEN: RefCell<Vec<std::rc::Weak<Compose>>> = const { RefCell::new(Vec::new()) };
}

pub fn open(ui: &Rc<Ui>, draft: Draft) {
    if ui.client.is_none() {
        return;
    }
    let window = gtk::Window::builder()
        .title(if draft.reply_to_message_id.is_some() { "Reply" } else { "New message" })
        .transient_for(&ui.window)
        // Hyprland centres a dialog on its parent, so one wider than a half-screen tile hangs off the edge.
        .default_width((ui.window.width() - 40).clamp(480, 760))
        .default_height(600)
        .build();
    window.add_css_class("compose");

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(10)
        .margin_top(14)
        .margin_bottom(14)
        .margin_start(14)
        .margin_end(14)
        .build();
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
    let chips = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(6)
        .row_spacing(6)
        .max_children_per_line(20)
        .visible(false)
        .build();
    chips.add_css_class("attachments");
    grid.attach(&chips, 0, 4, 2, 1);
    let scroller = gtk::ScrolledWindow::builder().child(&body).vexpand(true).hexpand(true).build();
    grid.attach(&scroller, 0, 5, 2, 1);

    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let attach = gtk::Button::with_label("Attach…");
    attach.set_tooltip_text(Some("Attach files (or drop them on this window)"));
    let error = gtk::Label::builder().xalign(0.0).hexpand(true).wrap(true).build();
    error.add_css_class("error");
    let hint = gtk::Label::new(Some("Ctrl+Enter to send · Esc to close"));
    hint.add_css_class("dim");
    let send = gtk::Button::with_label("Send");
    send.add_css_class("suggested");
    bottom.append(&attach);
    bottom.append(&error);
    bottom.append(&hint);
    bottom.append(&send);
    grid.attach(&bottom, 0, 6, 2, 1);
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
        attachments: Default::default(),
        chips,
        initial: Default::default(),
        reply_to_message_id: draft.reply_to_message_id,
    });
    #[cfg(debug_assertions)]
    OPEN.with(|o| o.borrow_mut().push(Rc::downgrade(&c)));

    *c.initial.borrow_mut() = fields(&c);
    // The title-bar close button and the compositor's close (Super+W) ask first, like Esc.
    c.window.connect_close_request(clone!(
        #[weak]
        c,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_| {
            if is_dirty(&c) {
                close(&c);
                glib::Propagation::Stop
            } else {
                glib::Propagation::Proceed
            }
        }
    ));

    // Signal handlers only hold weak refs; the window owns the compose state.
    let keep = RefCell::new(Some(c.clone()));
    c.window.connect_destroy(move |_| drop(keep.take()));

    fill_from(&c, ui, draft.from.as_deref());
    if ui.identities.borrow().is_none() {
        let client = ui.client.clone().unwrap();
        let wanted = draft.from;
        util::run(
            move || client.identities(),
            clone!(
                #[weak]
                ui,
                #[strong]
                c,
                move |result| {
                    if let Ok(ids) = result {
                        *ui.identities.borrow_mut() = Some(ids);
                        if c.window.is_visible() {
                            fill_from(&c, &ui, wanted.as_deref());
                        }
                    }
                }
            ),
        );
    }

    c.send.connect_clicked(clone!(
        #[weak]
        ui,
        #[weak]
        c,
        move |_| send_message(&c, &ui)
    ));
    attach.connect_clicked(clone!(
        #[weak]
        c,
        move |_| choose_files(&c)
    ));

    // Files dropped anywhere on the window are attached (before the body could take them as text).
    let drop = gtk::DropTarget::new(gdk::FileList::static_type(), gdk::DragAction::COPY);
    drop.set_propagation_phase(gtk::PropagationPhase::Capture);
    drop.connect_drop(clone!(
        #[weak]
        c,
        #[upgrade_or]
        false,
        move |_, value, _, _| on_drop(&c, value)
    ));
    c.window.add_controller(drop);

    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    keys.connect_key_pressed(clone!(
        #[weak]
        ui,
        #[weak]
        c,
        #[upgrade_or]
        glib::Propagation::Proceed,
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
    // A linked account's addresses send through that account; they're labelled with it.
    let mut via: Vec<Option<String>> = vec![None; options.len()];
    for (account, a) in ui.account_identities.borrow().iter() {
        if !options.iter().any(|o| o.email.eq_ignore_ascii_case(&a.email)) {
            options.push(a.clone());
            via.push(Some(crate::api::provider::account_label(account)));
        }
    }
    // A reply in a linked account always goes out through it, from its address, even before
    // that account's addresses have loaded.
    let account_reply =
        c.reply_to_message_id.as_deref().and_then(|id| id.split_once(':')).map(|(account, _)| account.to_string());
    if let (Some(account), Some(w)) = (account_reply, wanted.filter(|w| !w.is_empty()))
        && !options.iter().any(|o| o.email.eq_ignore_ascii_case(w))
    {
        options.push(Address { name: None, email: w.to_string() });
        via.push(Some(crate::api::provider::account_label(&account)));
    }
    // Before your mailboxes load, show the wanted address alone; after, only mailboxes can send.
    if let Some(w) = wanted.filter(|w| !w.is_empty())
        && options.is_empty()
    {
        let name = ids.as_ref().and_then(|i| i.default.as_ref()).and_then(|d| d.name.clone());
        options.push(Address { name, email: w.to_string() });
        via.push(None);
    }
    let labels: Vec<String> = options
        .iter()
        .zip(&via)
        .map(|(a, via)| match via {
            Some(v) => format!("{}  · {v}", a.formatted()),
            None => a.formatted(),
        })
        .collect();
    let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
    c.from.set_model(Some(&gtk::StringList::new(&refs)));
    let selected = wanted.and_then(|w| options.iter().position(|o| o.email.eq_ignore_ascii_case(w))).unwrap_or(0);
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
    let Some(mail) = ui.mail.clone() else { return };
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
        attachments: c.attachments.borrow().clone(),
    };
    c.error.set_label("");
    c.send.set_sensitive(false);
    c.send.set_label("Sending…");
    util::run(
        move || mail.send(&req),
        clone!(
            #[weak]
            ui,
            #[strong]
            c,
            move |result: Result<crate::api::SendResponse, String>| match result {
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
            }
        ),
    );
}

fn is_dirty(c: &Compose) -> bool {
    dirty(&fields(c), &c.initial.borrow(), c.attachments.borrow().len())
}

/// Whether closing would lose something: an edited, non-empty field, or any attached file.
fn dirty(now: &[String; 4], initial: &[String; 4], attachments: usize) -> bool {
    attachments > 0
        || (now.iter().zip(initial.iter()).any(|(a, b)| a.trim() != b.trim())
            && now.iter().any(|f| !f.trim().is_empty()))
}

/// "report.pdf" and "1.2 MB", for a chip.
fn chip_text(a: &OutgoingAttachment) -> (String, String) {
    (a.filename.clone(), util::human_size(a.size() as i64))
}

fn choose_files(c: &Rc<Compose>) {
    let dialog = gtk::FileDialog::builder().title("Attach files").accept_label("Attach").modal(true).build();
    dialog.open_multiple(
        Some(&c.window),
        gio::Cancellable::NONE,
        clone!(
            #[weak]
            c,
            move |result| {
                if let Ok(model) = result {
                    add_files(
                        &c,
                        (0..model.n_items()).filter_map(|i| model.item(i).and_downcast::<gio::File>()).collect(),
                    );
                }
            }
        ),
    );
}

fn on_drop(c: &Rc<Compose>, value: &glib::Value) -> bool {
    match value.get::<gdk::FileList>() {
        Ok(list) => {
            add_files(c, list.files());
            true
        }
        Err(_) => false,
    }
}

/// Reads the files off the main loop and adds a chip for each; the first that can't be read is
/// reported, the rest are still attached.
fn add_files(c: &Rc<Compose>, files: Vec<gio::File>) {
    if files.is_empty() {
        return;
    }
    let paths: Vec<Option<std::path::PathBuf>> = files.iter().map(|f| f.path()).collect();
    util::run(
        move || {
            Ok::<_, String>(
                paths
                    .into_iter()
                    .map(|p| match p {
                        Some(p) => OutgoingAttachment::from_path(&p).map_err(|e| e.message),
                        None => Err("only files on this computer can be attached".to_string()),
                    })
                    .collect::<Vec<_>>(),
            )
        },
        clone!(
            #[weak]
            c,
            move |result: Result<Vec<Result<OutgoingAttachment, String>>, String>| {
                let mut problem = None;
                for r in result.unwrap_or_else(|e| vec![Err(e)]) {
                    match r {
                        Ok(a) => c.attachments.borrow_mut().push(a),
                        Err(e) => problem = problem.or(Some(e)),
                    }
                }
                c.error.set_label(&problem.map(|e| format!("Couldn't attach: {e}")).unwrap_or_default());
                render_chips(&c);
            }
        ),
    );
}

fn render_chips(c: &Rc<Compose>) {
    c.chips.remove_all();
    for (i, a) in c.attachments.borrow().iter().enumerate() {
        let (name, size) = chip_text(a);
        let chip = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        chip.add_css_class("attachment-chip");
        chip.set_halign(gtk::Align::Start);
        chip.set_tooltip_text(Some(&a.mime_type));
        chip.append(
            &gtk::Label::builder()
                .label(&name)
                .ellipsize(gtk::pango::EllipsizeMode::Middle)
                .max_width_chars(32)
                .build(),
        );
        let size = gtk::Label::new(Some(&size));
        size.add_css_class("dim");
        chip.append(&size);
        let remove = gtk::Button::with_label("×");
        remove.add_css_class("flat");
        remove.set_tooltip_text(Some(&format!("Remove {name}")));
        remove.connect_clicked(clone!(
            #[weak]
            c,
            move |_| {
                if i < c.attachments.borrow().len() {
                    c.attachments.borrow_mut().remove(i);
                }
                render_chips(&c);
            }
        ));
        chip.append(&remove);
        c.chips.append(&chip);
    }
    c.chips.set_visible(!c.attachments.borrow().is_empty());
}

/// Debug builds: drops `path` on the open compose window, as dragging it there would.
#[cfg(debug_assertions)]
pub fn drop_on_open(path: &str) -> bool {
    let open =
        OPEN.with(|o| o.borrow().iter().rev().filter_map(std::rc::Weak::upgrade).find(|c| c.window.is_visible()));
    let Some(c) = open else { return false };
    on_drop(&c, &gdk::FileList::from_array(&[gio::File::for_path(path)]).to_value())
}

/// Debug builds: clicks the remove button on the open compose window's `i`th chip.
#[cfg(debug_assertions)]
pub fn unattach_on_open(i: usize) -> bool {
    let open =
        OPEN.with(|o| o.borrow().iter().rev().filter_map(std::rc::Weak::upgrade).find(|c| c.window.is_visible()));
    let button = open
        .and_then(|c| c.chips.child_at_index(i as i32))
        .and_then(|chip| chip.child())
        .and_then(|b| b.last_child())
        .and_downcast::<gtk::Button>();
    button.map(|b| b.emit_clicked()).is_some()
}

/// Debug builds: the open compose window's attachments, and whether it has unsaved changes.
#[cfg(debug_assertions)]
pub fn open_state() -> Option<(Vec<String>, bool)> {
    let open =
        OPEN.with(|o| o.borrow().iter().rev().filter_map(std::rc::Weak::upgrade).find(|c| c.window.is_visible()))?;
    let names = open.attachments.borrow().iter().map(|a| a.filename.clone()).collect();
    Some((names, is_dirty(&open)))
}

fn close(c: &Rc<Compose>) {
    if !is_dirty(c) {
        c.window.destroy();
        return;
    }
    let dialog = gtk::AlertDialog::builder()
        .message(if c.attachments.borrow().is_empty() {
            "Discard this draft?"
        } else {
            "Discard this draft and its attachments?"
        })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn f(to: &str, body: &str) -> [String; 4] {
        [to.into(), String::new(), String::new(), body.into()]
    }

    #[test]
    fn attachments_make_a_draft_worth_keeping() {
        let initial = f("a@b.c", "");
        assert!(!dirty(&initial, &initial, 0), "untouched");
        assert!(dirty(&initial, &initial, 1), "a file was attached");
        assert!(dirty(&f("a@b.c", "hi"), &initial, 0));
        assert!(!dirty(&f("", ""), &f("", ""), 0), "empty");
        assert!(dirty(&f("", ""), &f("", ""), 2), "only files");
    }

    #[test]
    fn chips_show_name_and_size() {
        let a = OutgoingAttachment {
            filename: "report.pdf".into(),
            mime_type: "application/pdf".into(),
            content: vec![0; 1536],
        };
        assert_eq!(chip_text(&a), ("report.pdf".to_string(), "2 KB".to_string()));
    }
}
