//! The People window, for the admin (who deployed the worker to their Cloudflare account): who
//! else reads their mail on it, inviting someone, a fresh link, adding an address, their copy of
//! every message, and signing them out or removing them. What it does is `cloudmail user`'s.

use gtk::{gdk, glib, glib::clone, pango, prelude::*};
use omui::tr;
use std::cell::RefCell;
use std::rc::Rc;

use super::{Live, Ui, bind_title, keep_until_closed, tool_window};
use crate::api::text::shell_arg;
use crate::api::{Client, Invite, MailboxUpdate, User};
use crate::format::long_time;
use crate::util::{self, split_addresses};

struct People {
    client: Client,
    window: gtk::Window,
    list: gtk::Box,
    /// Everyone but the admin as last read, to be shown again after a switch of language; none before the first.
    users: RefCell<Option<Vec<User>>>,
    status: Live,
    /// The latest invite: its link, with buttons to copy it or the command to run.
    invite: gtk::Box,
    /// Whom the invite on show is for, to write it again after a switch of language.
    shown_invite: RefCell<Option<(String, Invite)>>,
}

/// Where someone stands, as the window says it; `User::state` is the command line's English.
fn state(u: &User) -> String {
    if u.revoked {
        tr("revoked")
    } else if let Some(t) = u.invite_expires_at {
        omui::tr!("invited (until {})", long_time(t))
    } else if u.connected {
        tr("connected")
    } else {
        tr("not connected")
    }
}

pub fn show(ui: &Rc<Ui>) {
    let Some(client) = ui.client.clone() else { return };
    let (window, root, heading) = tool_window(ui, "people", 620);
    bind_title(&window, "People");
    omui::i18n::bind_label(&heading, "People on your worker");
    let about = omui::label("", "column-sub");
    omui::i18n::bind_label(
        &about,
        "Each sees only the mail of their own addresses, and needs no Cloudflare account.",
    );
    root.append(&about);
    // Lined up with the headings' padding.
    let list = gtk::Box::new(gtk::Orientation::Vertical, 14);
    list.set_margin_start(14);
    list.set_margin_end(14);
    list.append(&omui::label(&tr("Loading…"), "column-sub"));
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(240)
        .max_content_height(480)
        .propagate_natural_height(true)
        .build();
    root.append(&scroller);

    let invite_heading = omui::label("", "column-title");
    omui::i18n::bind_label(&invite_heading, "Invite someone");
    root.append(&invite_heading);
    let form = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    form.set_margin_start(14);
    form.set_margin_end(14);
    let name = gtk::Entry::builder().width_chars(12).build();
    omui::i18n::bind(&name, |e| e.set_placeholder_text(Some(&tr("name, like sam"))));
    let addresses = gtk::Entry::builder().hexpand(true).build();
    omui::i18n::bind(&addresses, |e| {
        e.set_placeholder_text(Some(&tr("their addresses, comma-separated")))
    });
    let invite_btn = gtk::Button::new();
    omui::i18n::bind_label(&invite_btn, "Invite");
    invite_btn.add_css_class("suggested-action");
    form.append(&name);
    form.append(&addresses);
    form.append(&invite_btn);
    root.append(&form);
    let routing = omui::label("", "column-sub");
    omui::i18n::bind_label(
        &routing,
        "Their addresses' Email Routing must point at your worker: `cloudmail mailbox add <address> --route`.",
    );
    root.append(&routing);

    let invite_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    invite_box.set_visible(false);
    invite_box.set_margin_start(14);
    invite_box.set_margin_end(14);
    root.append(&invite_box);
    let status = Live::new(omui::label("", "column-sub"));
    status.label.set_selectable(true);
    root.append(&status.label);

    let p = Rc::new(People {
        client,
        window,
        list,
        users: RefCell::new(None),
        status,
        invite: invite_box,
        shown_invite: RefCell::new(None),
    });
    // Every handler holds `p` weakly.
    keep_until_closed(&p.window, &p);
    omui::i18n::on_change(
        &p.window,
        clone!(
            #[weak]
            p,
            move || {
                render(&p);
                let shown = p.shown_invite.borrow().clone();
                if let Some((name, invite)) = shown {
                    write_invite(&p, &name, &invite);
                }
            }
        ),
    );

    on_submit(
        &addresses,
        &invite_btn,
        clone!(
            #[weak]
            p,
            #[weak]
            name,
            #[weak]
            addresses,
            move || {
                let who = name.text().trim().to_lowercase();
                if who.is_empty() {
                    say(&p, || {
                        tr("Give them a name first: it's how you refer to them, like sam.")
                    });
                    return;
                }
                let boxes = split_addresses(&addresses.text());
                name.set_text("");
                addresses.set_text("");
                invite(&p, who, boxes);
            }
        ),
    );

    refresh(&p);
    p.window.present();
}

/// Runs `f` from the button, or Enter in the entry.
fn on_submit(entry: &gtk::Entry, button: &gtk::Button, f: impl Fn() + Clone + 'static) {
    let g = f.clone();
    button.connect_clicked(move |_| g());
    entry.connect_activate(move |_| f());
}

/// Says `text()` at the foot of the window, in the language of the moment.
fn say(p: &People, text: impl Fn() -> String + 'static) {
    p.status.set(text);
}

fn copy(p: &People, text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
    say(p, || tr("Copied."));
}

/// The people as last read.
fn render(p: &Rc<People>) {
    let Some(others) = p.users.borrow().clone() else { return };
    omui::clear(&p.list);
    if others.is_empty() {
        p.list.append(&omui::label(
            &tr("Only you so far: invite someone below."),
            "column-sub",
        ));
    }
    for u in others {
        p.list.append(&row(p, u));
    }
}

/// Everyone but the admin, off the main loop; the rows shown stay until the new ones are in.
fn refresh(p: &Rc<People>) {
    let client = p.client.clone();
    util::run(
        move || client.users().map_err(|e| e.to_string()),
        clone!(
            #[weak]
            p,
            move |r: Result<Vec<User>, String>| {
                let users = match r {
                    Ok(users) => users,
                    Err(e) => return p.status.set_plain(&e),
                };
                p.users
                    .replace(Some(users.into_iter().filter(|u| !u.is_admin()).collect()));
                render(&p);
            }
        ),
    );
}

/// A button that runs `f` for this user; `label` and `tooltip` are translated already.
fn action(p: &Rc<People>, label: &str, tooltip: &str, name: &str, f: fn(&Rc<People>, String)) -> gtk::Button {
    let b = gtk::Button::with_label(label);
    b.set_tooltip_text(Some(tooltip));
    let name = name.to_string();
    b.connect_clicked(clone!(
        #[weak]
        p,
        move |_| f(&p, name.clone())
    ));
    b
}

fn row(p: &Rc<People>, u: User) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let title = gtk::Label::builder()
        .label(format!("{} · {}", u.name, state(&u)))
        .xalign(0.0)
        .hexpand(true)
        .build();
    top.append(&title);
    top.append(&action(
        p,
        &if u.connected {
            tr("New link")
        } else {
            tr("New invite link")
        },
        &tr("A one-time link for another computer; using it signs out the computer they use now"),
        &u.name,
        |p, name| invite(p, name, Vec::new()),
    ));
    if u.connected {
        top.append(&action(
            p,
            &tr("Sign out"),
            &tr("Signs them out everywhere; their mail stays, and a new link lets them back in"),
            &u.name,
            |p, name| revoke(p, name, false),
        ));
    }
    let remove = action(
        p,
        &tr("Remove…"),
        &tr("Deletes them and all their mail from your worker"),
        &u.name,
        |p, name| revoke(p, name, true),
    );
    remove.add_css_class("destructive-action");
    top.append(&remove);
    row.append(&top);

    let boxes = if u.mailboxes.is_empty() {
        tr("No addresses yet")
    } else {
        u.mailboxes.join(", ")
    };
    let detail = omui::label(&boxes, "column-sub");
    detail.set_selectable(true);
    row.append(&detail);

    let edits = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let address = gtk::Entry::builder()
        .placeholder_text(tr("add an address"))
        .hexpand(true)
        .build();
    let add = gtk::Button::with_label(&tr("Add"));
    let name = u.name.clone();
    on_submit(
        &address,
        &add,
        clone!(
            #[weak]
            p,
            #[weak]
            address,
            #[strong]
            name,
            move || add_mailbox(&p, name.clone(), address.text().trim().to_string())
        ),
    );
    edits.append(&address);
    edits.append(&add);
    row.append(&edits);

    // A copy of every message elsewhere (often their old inbox, while they move over).
    let fwd = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let entry = gtk::Entry::builder()
        .text(&u.forward_to)
        .placeholder_text(tr("nobody"))
        .tooltip_text(tr("A verified Email Routing destination address; empty for none"))
        .hexpand(true)
        .build();
    let save = gtk::Button::with_label(&tr("Save"));
    on_submit(
        &entry,
        &save,
        clone!(
            #[weak]
            p,
            #[weak]
            entry,
            move || set_forward(&p, name.clone(), entry.text().trim().to_string())
        ),
    );
    fwd.append(&gtk::Label::new(Some(&tr("Copy of every message to"))));
    fwd.append(&entry);
    fwd.append(&save);
    row.append(&fwd);
    row
}

/// Gives them an address; the worker decides whether new senders to it are screened.
fn add_mailbox(p: &Rc<People>, name: String, email: String) {
    if email.is_empty() {
        return;
    }
    let client = p.client.clone();
    util::write(
        move || {
            let update = MailboxUpdate {
                user: Some(name),
                ..Default::default()
            };
            client.put_mailbox(&email, &update).map_err(|e| e.to_string())
        },
        clone!(
            #[weak]
            p,
            move |r: Result<crate::api::Mailbox, String>| match r {
                Ok(m) => {
                    say(&p, move || omui::tr!("{} is {}'s now.", m.email, m.user));
                    refresh(&p);
                }
                Err(e) => p.status.set_plain(&e),
            }
        ),
    );
}

fn set_forward(p: &Rc<People>, name: String, to: String) {
    let client = p.client.clone();
    let user = name.clone();
    util::write(
        move || {
            client
                .update_settings(Some(&user), &serde_json::json!({ "forward_to": to }))
                .map_err(|e| e.to_string())
        },
        clone!(
            #[weak]
            p,
            move |r: Result<crate::api::Settings, String>| match r {
                Ok(s) if s.forward_to.is_empty() => {
                    say(&p, move || omui::tr!("{}'s mail is no longer copied anywhere.", name))
                }
                Ok(s) => say(&p, move || omui::tr!(
                    "A copy of {}'s mail goes to {}.",
                    name,
                    s.forward_to
                )),
                Err(e) => p.status.set_plain(&e),
            }
        ),
    );
}

/// Invites someone new, or gives someone a fresh link (and any new addresses).
fn invite(p: &Rc<People>, name: String, mailboxes: Vec<String>) {
    let who = name.clone();
    say(p, move || omui::tr!("Making a link for {}…", who));
    let client = p.client.clone();
    util::write(
        move || client.invite_user(&name, &mailboxes).map_err(|e| e.to_string()),
        clone!(
            #[weak]
            p,
            move |r: Result<(User, Invite), String>| match r {
                Ok((user, invite)) => {
                    show_invite(&p, &user.name, &invite);
                    refresh(&p);
                }
                Err(e) => p.status.set_plain(&e),
            }
        ),
    );
}

fn show_invite(p: &Rc<People>, name: &str, invite: &Invite) {
    p.shown_invite.replace(Some((name.to_owned(), invite.clone())));
    write_invite(p, name, invite);
    say(p, String::new);
}

fn write_invite(p: &Rc<People>, name: &str, invite: &Invite) {
    omui::clear(&p.invite);
    p.invite.append(&omui::label(
        &omui::tr!(
            "{}'s link works once, until {}. Send it privately (anyone who uses it first reads their mail). On their computer, with Cloudmail installed:",
            name,
            long_time(invite.expires_at)
        ),
        "column-sub",
    ));
    let command = format!("cloudmail connect {}", shell_arg(&invite.url));
    let shown = omui::label(&command, "");
    shown.set_wrap_mode(pango::WrapMode::Char);
    shown.set_selectable(true);
    p.invite.append(&shown);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    for (label, value) in [(tr("Copy command"), command), (tr("Copy link"), invite.url.clone())] {
        let b = gtk::Button::with_label(&label);
        b.connect_clicked(clone!(
            #[weak]
            p,
            move |_| copy(&p, &value)
        ));
        buttons.append(&b);
    }
    p.invite.append(&buttons);
    p.invite.set_visible(true);
}

/// Signs someone out everywhere, or with `purge` removes them and all their mail, after asking.
fn revoke(p: &Rc<People>, name: String, purge: bool) {
    let named = name.clone();
    let texts = move || {
        if purge {
            (
                omui::tr!("Remove {} and delete their mail?", named),
                tr(
                    "Their mail, Screener decisions, settings and addresses are deleted from your worker. This can't be undone.",
                ),
                tr("Remove"),
            )
        } else {
            (
                omui::tr!("Sign {} out everywhere?", named),
                tr("Their mail stays; a new link lets them back in."),
                tr("Sign out"),
            )
        }
    };
    super::ask_destructive(
        &p.window,
        texts,
        clone!(
            #[weak]
            p,
            move || {
                let client = p.client.clone();
                let who = name.clone();
                util::write(
                    move || client.revoke_user(&who, purge).map_err(|e| e.to_string()),
                    clone!(
                        #[weak]
                        p,
                        move |r: Result<(), String>| {
                            match r {
                                Ok(()) if purge => say(&p, move || omui::tr!("Removed {}.", name)),
                                Ok(()) => say(&p, move || omui::tr!("Signed {} out.", name)),
                                Err(e) => p.status.set_plain(&e),
                            }
                            refresh(&p);
                        }
                    ),
                );
            }
        ),
    );
}
