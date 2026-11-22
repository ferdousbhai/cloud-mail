use gtk::{gdk, gio, glib, glib::clone, prelude::*};
use omui::tr;
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use super::{Live, Ui};
use crate::api::{Address, OutgoingAttachment, SendRequest};
use crate::drafts;
use crate::util::{self, Draft};

/// How long after an edit the draft on disk catches up with the window: often enough that little is lost when the
/// app ends under it, seldom enough that typing is not a write a key.
const SAVE_AFTER: Duration = Duration::from_secs(2);

struct Compose {
    window: gtk::Window,
    from: gtk::DropDown,
    from_emails: RefCell<Vec<String>>,
    to: gtk::Entry,
    cc: gtk::Entry,
    subject: gtk::Entry,
    body: gtk::TextView,
    /// What went wrong (a send, a write of the draft, a file), said again after a switch of language.
    error: Live,
    send: gtk::Button,
    /// The files to send, each shown as a removable chip in `chips`.
    attachments: RefCell<Vec<OutgoingAttachment>>,
    chips: gtk::FlowBox,
    /// To, Cc, Subject and body as the window opened, to tell whether anything was edited.
    initial: RefCell<[String; 4]>,
    reply_to_message_id: Option<String>,
    /// Where the draft is kept while it is being written, and under what name; none without a state directory.
    store: Option<drafts::Store>,
    draft_id: String,
    /// Set while an edit waits to be written to the draft on disk.
    save_timer: RefCell<Option<glib::SourceId>>,
    /// The last write of the draft failed and the window says so, until one succeeds.
    save_failed: Cell<bool>,
    /// The message is on its way: the window can be put away, but not edited into another send.
    sending: Cell<bool>,
}

fn fields(c: &Compose) -> [String; 4] {
    [
        c.to.text().to_string(),
        c.cc.text().to_string(),
        c.subject.text().to_string(),
        body_text(c),
    ]
}

#[cfg(debug_assertions)]
thread_local! {
    /// Open compose windows, for the smoke-test script (which can't drive a file chooser).
    static OPEN: RefCell<Vec<std::rc::Weak<Compose>>> = const { RefCell::new(Vec::new()) };
}

pub fn open(ui: &Rc<Ui>, draft: Draft) {
    if ui.mail.is_none() {
        return;
    }
    let window = gtk::Window::builder()
        .transient_for(&ui.window)
        // Hyprland centres a dialog on its parent, so one wider than a half-screen tile hangs off the edge.
        // A draft opened as the app starts comes before the main window has a size, so its default stands in.
        .default_width(
            (Some(ui.window.width())
                .filter(|w| *w > 0)
                .unwrap_or(ui.window.default_width())
                - 40)
                .clamp(480, 760),
        )
        .default_height(600)
        .build();
    window.add_css_class("compose");
    if draft.reply_to_message_id.is_some() {
        super::bind_title(&window, "Reply");
    } else {
        super::bind_title(&window, "New message");
    }

    let grid = gtk::Grid::builder()
        .row_spacing(8)
        .column_spacing(10)
        .margin_top(14)
        .margin_bottom(14)
        .margin_start(14)
        .margin_end(14)
        .build();
    let field = |row: i32, name: &'static str, widget: &gtk::Widget| {
        // The end of the caption meets its field, whichever way the language reads.
        let label = gtk::Label::builder().xalign(1.0).build();
        omui::i18n::bind(&label, move |label| label.set_label(&omui::i18n::translate(name)));
        label.add_css_class("dim-label");
        grid.attach(&label, 0, row, 1, 1);
        // The caption is the field's name to a screen reader, and to dev/qa's scripts.
        widget.update_relation(&[gtk::accessible::Relation::LabelledBy(&[label.upcast_ref()])]);
        widget.set_hexpand(true);
        grid.attach(widget, 1, row, 1, 1);
    };

    let from = gtk::DropDown::from_strings(&[]);
    let to = gtk::Entry::builder().text(&draft.to).build();
    omui::i18n::bind(&to, |to| to.set_placeholder_text(Some(&tr("name@example.com, …"))));
    let cc = gtk::Entry::builder().text(&draft.cc).build();
    let subject = gtk::Entry::builder().text(&draft.subject).build();
    // The captions are translated where `field` binds them; these calls name them for the catalog test.
    field(0, caption("From"), from.upcast_ref());
    field(1, caption("To"), to.upcast_ref());
    field(2, caption("Cc"), cc.upcast_ref());
    field(3, caption("Subject"), subject.upcast_ref());

    let body = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .accepts_tab(false)
        .vexpand(true)
        .build();
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
    let scroller = gtk::ScrolledWindow::builder()
        .child(&body)
        .vexpand(true)
        .hexpand(true)
        .build();
    // In a frame the body is a field like the ones above it.
    grid.attach(&gtk::Frame::builder().child(&scroller).build(), 0, 5, 2, 1);

    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let attach = gtk::Button::new();
    omui::i18n::bind_label(&attach, "Attach…");
    omui::i18n::bind_tooltip(&attach, "Attach files (or drop them on this window)");
    let error = Live::new(gtk::Label::builder().xalign(0.0).hexpand(true).wrap(true).build());
    error.label.add_css_class("error");
    let hint = gtk::Label::new(None);
    omui::i18n::bind_label(&hint, "Ctrl+Enter to send · Esc to close");
    hint.add_css_class("dim-label");
    let send = gtk::Button::new();
    send.add_css_class("suggested-action");
    bottom.append(&attach);
    bottom.append(&error.label);
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
        store: drafts::Store::open(),
        draft_id: draft.id.clone().unwrap_or_else(drafts::new_id),
        save_timer: Default::default(),
        save_failed: Cell::new(false),
        sending: Cell::new(false),
    });
    #[cfg(debug_assertions)]
    OPEN.with(|o| o.borrow_mut().push(Rc::downgrade(&c)));
    // The Send button says what it is doing, in the language of the moment; the chips' sizes and names too.
    relabel(&c);
    omui::i18n::on_change(
        &c.window,
        clone!(
            #[weak]
            c,
            move || {
                relabel(&c);
                render_chips(&c);
            }
        ),
    );

    // A draft opened from the store is unsaved work from its first moment: what it started from (a reply's quote,
    // a mailto: link's subject) is not known any more, so all of it counts as written, and closing asks.
    if draft.id.is_none() {
        *c.initial.borrow_mut() = fields(&c);
    }
    // The title-bar close button and the compositor's close (Super+W) ask first, like Esc.
    c.window.connect_close_request(clone!(
        #[weak]
        c,
        #[upgrade_or]
        glib::Propagation::Proceed,
        move |_| {
            if c.sending.get() || is_dirty(&c) {
                close(&c);
                glib::Propagation::Stop
            } else {
                forget_draft(&c);
                glib::Propagation::Proceed
            }
        }
    ));

    // Signal handlers only hold weak refs; the window owns the compose state.
    let keep = RefCell::new(Some(c.clone()));
    c.window.connect_destroy(move |_| {
        // Sending, discarding and Save draft settle the draft and cancel the write waiting for it, so one still
        // waiting here means the window went without being asked: the app quitting (Ctrl+Q) or its last window
        // closing under it. The last edits are written then, rather than lost to a timer that never fires.
        if let Some(c) = keep.take()
            && c.save_timer.borrow().is_some()
        {
            save_now(&c);
        }
    });

    // Every edit reaches the draft on disk a moment later, so the app ending under the window loses little.
    for entry in [&c.to, &c.cc, &c.subject] {
        entry.connect_changed(clone!(
            #[weak]
            c,
            move |_| save_soon(&c)
        ));
    }
    c.body.buffer().connect_changed(clone!(
        #[weak]
        c,
        move |_| save_soon(&c)
    ));

    fill_from(&c, ui, draft.from.as_deref());
    if ui.identities.borrow().is_none()
        && let Some(client) = ui.client.clone()
    {
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
        ui,
        #[weak]
        c,
        move |_| choose_files(&c, ui.picker.as_ref())
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
            if key == gdk::Key::Escape && !omui::dialog_open(&c.window) {
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
    // `?` is a character here, so the keys are listed on F1 and Ctrl+?, as in any window that is typed into; Ctrl+W
    // closes the draft (asking first) and Ctrl+Q every window.
    super::shortcuts::install(&c.window, true);
    omui::quit_keys(&c.window);

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
    let account_reply = c
        .reply_to_message_id
        .as_deref()
        .and_then(|id| id.split_once(':'))
        .map(|(account, _)| account.to_string());
    if let (Some(account), Some(w)) = (account_reply, wanted.filter(|w| !w.is_empty()))
        && !options.iter().any(|o| o.email.eq_ignore_ascii_case(w))
    {
        options.push(Address {
            name: None,
            email: w.to_string(),
        });
        via.push(Some(crate::api::provider::account_label(&account)));
    }
    // Before your mailboxes load, show the wanted address alone; after, only mailboxes can send.
    if let Some(w) = wanted.filter(|w| !w.is_empty())
        && options.is_empty()
    {
        let name = ids
            .as_ref()
            .and_then(|i| i.default.as_ref())
            .and_then(|d| d.name.clone());
        options.push(Address {
            name,
            email: w.to_string(),
        });
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
    let selected = wanted
        .and_then(|w| options.iter().position(|o| o.email.eq_ignore_ascii_case(w)))
        .unwrap_or(0);
    c.from.set_selected(selected as u32);
    *c.from_emails.borrow_mut() = options.into_iter().map(|a| a.email).collect();
}

/// Names a compose field's caption: it is translated where it is shown, now and after a switch of language.
fn caption(text: &'static str) -> &'static str {
    text
}

/// The window's own words that depend on its state, in the language of the moment.
fn relabel(c: &Compose) {
    c.send
        .set_label(&if c.sending.get() { tr("Sending…") } else { tr("Send") });
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
        c.error.set(|| tr("Add at least one recipient."));
        c.to.grab_focus();
        return;
    }
    let text = body_text(c);
    if text.trim().is_empty() {
        c.error.set(|| tr("The message is empty."));
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
    c.error.set_plain("");
    c.save_failed.set(false);
    c.sending.set(true);
    c.send.set_sensitive(false);
    relabel(c);
    // The main window is held strongly, not weakly: this must run to its end even if that window has closed and
    // the app has been started again with a new one, or a failure would have nowhere to be said.
    util::write(
        move || mail.send(&req),
        clone!(
            #[strong]
            ui,
            #[strong]
            c,
            move |result: Result<crate::api::SendResponse, String>| {
                c.sending.set(false);
                relabel(&c);
                match result {
                    Ok(resp) => {
                        forget_draft(&c);
                        c.window.destroy();
                        match &resp.warning {
                            Some(w) => ui.shell.toast(&omui::tr!("Sent, but {}", w)),
                            None => ui.shell.toast(&tr("Sent")),
                        }
                        ui.after_send(resp.thread_id.as_deref());
                    }
                    Err(e) => {
                        c.send.set_sensitive(true);
                        let error = e.clone();
                        c.error.set(move || omui::tr!("Couldn't send: {}", error));
                        if !c.window.is_visible() {
                            send_failed_out_of_sight(&c, &ui, &e);
                        }
                    }
                }
            }
        ),
    );
}

/// A send failed after its window was closed on it. With the main window still there, the message comes back with
/// what went wrong. With that gone too the app is about to end, so the message stays as a draft for the next start
/// and a desktop notification says so; if it cannot even be kept, its window comes back alone rather than lose it.
fn send_failed_out_of_sight(c: &Rc<Compose>, ui: &Ui, error: &str) {
    if ui.window.is_visible() {
        c.window.present();
    } else if c.store.is_some() && save_now(c) {
        util::notify(
            &tr("Your message wasn't sent"),
            &omui::tr!(
                "{}\n\nIt is kept as a draft and opens when Cloudmail next starts.",
                error
            ),
        );
        c.window.destroy();
    } else {
        c.window.set_transient_for(gtk::Window::NONE);
        if let Some(app) = gio::Application::default().and_downcast::<gtk::Application>() {
            app.add_window(&c.window);
        }
        c.window.present();
    }
}

/// The window's fields as a draft to keep.
fn snapshot(c: &Compose) -> Draft {
    let [to, cc, subject, body] = fields(c);
    Draft {
        to,
        cc,
        subject,
        body,
        reply_to_message_id: c.reply_to_message_id.clone(),
        from: c.from_emails.borrow().get(c.from.selected() as usize).cloned(),
        id: Some(c.draft_id.clone()),
    }
}

/// Writes the draft once typing has paused, at most every [`SAVE_AFTER`].
fn save_soon(c: &Rc<Compose>) {
    if c.store.is_none() || c.save_timer.borrow().is_some() {
        return;
    }
    let timer = glib::timeout_add_local_once(
        SAVE_AFTER,
        clone!(
            #[weak]
            c,
            move || {
                // This timer has run, so there is nothing left of it to remove.
                c.save_timer.take();
                save_now(&c);
            }
        ),
    );
    c.save_timer.replace(Some(timer));
}

/// Brings the draft on disk in step with the window: written when there is text worth keeping, removed when the
/// text is back to what the window opened with. Says in the window when that fails, and answers whether it worked.
/// A draft is a few kilobytes, written here rather than on a worker thread so that a write, a later one and a
/// removal can never land out of order.
fn save_now(c: &Compose) -> bool {
    if let Some(timer) = c.save_timer.take() {
        timer.remove();
    }
    let Some(store) = &c.store else { return false };
    let result = if dirty(&fields(c), &c.initial.borrow(), 0) {
        store.save(&c.draft_id, &snapshot(c))
    } else {
        store.remove(&c.draft_id)
    };
    match result {
        Ok(()) => {
            if c.save_failed.replace(false) {
                c.error.set_plain("");
            }
            true
        }
        Err(e) => {
            c.save_failed.set(true);
            c.error
                .set(move || omui::tr!("Couldn't keep this draft on disk: {}", e));
            false
        }
    }
}

/// Removes the draft from disk: it was sent, discarded, or never had anything in it.
fn forget_draft(c: &Compose) {
    if let Some(timer) = c.save_timer.take() {
        timer.remove();
    }
    if let Some(store) = &c.store {
        // A draft that cannot be removed opens again at the next start, where it can be discarded again.
        let _ = store.remove(&c.draft_id);
    }
}

thread_local! {
    /// Whether the kept drafts have been opened in this run of the app.
    static RESTORED: Cell<bool> = const { Cell::new(false) };
}

/// Opens every draft kept from an earlier run, each in a compose window of its own, and says how many. Once a run:
/// a main window opened again later in the same run would otherwise open the drafts of windows that are still up,
/// or of a message that is on its way.
pub fn restore(ui: &Rc<Ui>) {
    if ui.mail.is_none() || RESTORED.replace(true) {
        return;
    }
    let drafts = drafts::Store::open().map(|store| store.load()).unwrap_or_default();
    if let Some(text) = restored_text(drafts.len()) {
        ui.shell.toast(&text);
    }
    for draft in drafts {
        open(ui, draft);
    }
}

/// What the main window says about the drafts it opened, if any.
fn restored_text(count: usize) -> Option<String> {
    (count > 0).then(|| omui::trn!("Opened the {n} drafts kept from last time", count))
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
    (a.filename.clone(), crate::format::size(a.size() as i64))
}

/// Asks `picker` (the desktop portal's file chooser) for files to attach, and hands `done` the files chosen, or the
/// reason none were, for the window to say. Closing the chooser is no error.
fn pick_attachments(
    picker: &dyn omui::FilePicker,
    done: impl FnOnce(Result<Vec<std::path::PathBuf>, String>) + 'static,
) {
    let options = omui::PickOptions {
        accept_label: Some(tr("Attach")),
        multiple: true,
        ..omui::PickOptions::new(&tr("Attach files"))
    };
    picker.open(
        options,
        Box::new(move |picked| match picked {
            Ok(files) => done(Ok(files)),
            Err(omui::PickError::Cancelled) => {}
            Err(failed) => done(Err(failed.to_string())),
        }),
    );
}

fn choose_files(c: &Rc<Compose>, picker: &dyn omui::FilePicker) {
    let weak = Rc::downgrade(c);
    pick_attachments(picker, move |picked| {
        let Some(c) = weak.upgrade() else { return };
        match picked {
            Ok(paths) => add_files(&c, paths.iter().map(gio::File::for_path).collect()),
            Err(e) => c.error.set(move || omui::tr!("Couldn't open the file chooser: {}", e)),
        }
    });
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
                        None => Err(tr("only files on this computer can be attached")),
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
                match problem {
                    Some(e) => c.error.set(move || omui::tr!("Couldn't attach: {}", e)),
                    None => c.error.set_plain(""),
                }
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
        size.add_css_class("dim-label");
        chip.append(&size);
        let remove = omui::icon_button(omui::icons::name::CLOSE, &omui::tr!("Remove {}", name));
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
    let open = OPEN.with(|o| {
        o.borrow()
            .iter()
            .rev()
            .filter_map(std::rc::Weak::upgrade)
            .find(|c| c.window.is_visible())
    });
    let Some(c) = open else { return false };
    on_drop(&c, &gdk::FileList::from_array(&[gio::File::for_path(path)]).to_value())
}

/// Debug builds: clicks the remove button on the open compose window's `i`th chip.
#[cfg(debug_assertions)]
pub fn unattach_on_open(i: usize) -> bool {
    let open = OPEN.with(|o| {
        o.borrow()
            .iter()
            .rev()
            .filter_map(std::rc::Weak::upgrade)
            .find(|c| c.window.is_visible())
    });
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
    let open = OPEN.with(|o| {
        o.borrow()
            .iter()
            .rev()
            .filter_map(std::rc::Weak::upgrade)
            .find(|c| c.window.is_visible())
    })?;
    let names = open.attachments.borrow().iter().map(|a| a.filename.clone()).collect();
    Some((names, is_dirty(&open)))
}

fn close(c: &Rc<Compose>) {
    if c.sending.get() {
        // The message is on its way and the application is held until it has gone (`util::write`), so the
        // window is only put away: sent, it closes for good; failed, it comes back saying why. The draft is written
        // first, in case the app is ended before either.
        save_now(c);
        c.window.set_visible(false);
        return;
    }
    if !is_dirty(c) {
        forget_draft(c);
        c.window.destroy();
        return;
    }
    let attachments = !c.attachments.borrow().is_empty();
    // Only text is kept in a draft, so one that is nothing but attached files has nothing to save.
    let can_save = c.store.is_some() && dirty(&fields(c), &c.initial.borrow(), 0);
    // `omui::confirm` with the first button named for what it does here, and a third that keeps the draft. Its words
    // are written again after a switch of language while it is open, as `confirm`'s are.
    let dialog = omui::Dialog::new(Some(""), Some(""));
    dialog.add_response("keep", "");
    if can_save {
        dialog.add_response("save", "");
    }
    dialog.add_response("discard", "");
    dialog.bind(move |dialog| {
        dialog.set_heading(&if attachments {
            tr("Discard this draft and its attachments?")
        } else {
            tr("Discard this draft?")
        });
        dialog.set_body(&close_detail(can_save, attachments));
        dialog.set_response_label("keep", &tr("Keep editing"));
        if can_save {
            dialog.set_response_label("save", &tr("Save draft"));
        }
        dialog.set_response_label("discard", &tr("Discard"));
    });
    dialog.set_response_appearance("discard", omui::Appearance::Destructive);
    dialog.set_default_response("keep");
    dialog.set_close_response("keep");
    let weak = Rc::downgrade(c);
    dialog.connect_response(move |_, id| {
        let Some(c) = weak.upgrade() else { return };
        match id {
            "discard" => {
                forget_draft(&c);
                c.window.destroy();
            }
            // A draft that could not be written stays open, with the reason in its window.
            "save" if save_now(&c) => c.window.destroy(),
            // Keep editing, Escape, and any answer not named here: nothing is lost.
            _ => {}
        }
    });
    dialog.present(&c.window);
}

/// What the question about closing a draft says under its heading: where a saved draft goes, and what is not saved.
fn close_detail(can_save: bool, attachments: bool) -> String {
    match (can_save, attachments) {
        (true, false) => tr("A saved draft opens again the next time Cloudmail starts."),
        (true, true) => tr(
            "A saved draft opens again the next time Cloudmail starts, without its attached files: attach them again then.",
        ),
        (false, true) => tr("Attached files aren't saved with a draft."),
        (false, false) => tr("Drafts can't be saved here: there is no state directory to keep them in."),
    }
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
    fn closing_a_draft_says_what_saving_keeps() {
        assert!(close_detail(true, false).contains("opens again"));
        assert!(close_detail(true, true).contains("without its attached files"));
        assert!(!close_detail(false, true).contains("opens again"));
        assert!(close_detail(false, false).contains("can't be saved"));
    }

    #[test]
    fn a_draft_opened_from_the_store_is_unsaved_work() {
        // Its window's starting point is empty fields, so whatever it holds asks before it is closed.
        let restored = f("sam@example.com", "Half a sentence");
        assert!(dirty(&restored, &Default::default(), 0));
        assert!(!dirty(&f("", " "), &Default::default(), 0), "emptied again");
    }

    #[test]
    fn restored_drafts_are_announced() {
        crate::test_catalogs();
        assert_eq!(restored_text(0), None);
        assert_eq!(restored_text(1).unwrap(), "Opened the draft kept from last time");
        assert_eq!(
            restored_text(1200).unwrap(),
            "Opened the 1,200 drafts kept from last time"
        );
        assert_eq!(restored_text(3).unwrap(), "Opened the 3 drafts kept from last time");
    }

    #[test]
    fn files_are_chosen_through_the_picker_and_its_failures_are_said() {
        use std::cell::RefCell;
        let picker = omui::FakeFilePicker::new();
        type Answers = Rc<RefCell<Vec<Result<Vec<std::path::PathBuf>, String>>>>;
        let answers: Answers = Rc::default();
        let take = |answers: &Rc<RefCell<Vec<_>>>| {
            let answers = answers.clone();
            move |picked| answers.borrow_mut().push(picked)
        };
        picker.answer(Ok(vec!["/tmp/report.pdf".into(), "/tmp/map.gpx".into()]));
        pick_attachments(&picker, take(&answers));
        // Closing the chooser says nothing; the fake answers Cancelled once it has no answers left.
        pick_attachments(&picker, take(&answers));
        picker.answer(Err(omui::PickError::Failed("no portal".into())));
        pick_attachments(&picker, take(&answers));
        assert_eq!(
            *answers.borrow(),
            [
                Ok(vec!["/tmp/report.pdf".into(), "/tmp/map.gpx".into()]),
                Err("no portal".to_string())
            ]
        );
        let asked = picker.asked();
        assert_eq!(asked.len(), 3);
        let (saving, options) = &asked[0];
        assert!(!saving && options.multiple, "several files, to open");
        assert_eq!(options.title, "Attach files");
        assert_eq!(options.accept_label.as_deref(), Some("Attach"));
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
