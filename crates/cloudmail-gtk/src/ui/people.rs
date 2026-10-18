//! The People window, for the admin (who deployed the worker to their Cloudflare account): who
//! else reads their mail on it, inviting someone, a fresh link, adding an address, their copy of
//! every message, and signing them out or removing them. What it does is `cloudmail user`'s.

use gtk::{gdk, glib, glib::clone, pango, prelude::*};
use std::rc::Rc;

use super::{Ui, clear, confirm, keep_until_closed, text, tool_window};
use crate::api::text::shell_arg;
use crate::api::{Client, Invite, MailboxUpdate, User};
use crate::util::{self, long_time, split_addresses};

struct People {
    client: Client,
    window: gtk::Window,
    list: gtk::Box,
    status: gtk::Label,
    /// The latest invite: its link, with buttons to copy it or the command to run.
    invite: gtk::Box,
}

pub fn show(ui: &Rc<Ui>) {
    let Some(client) = ui.client.clone() else { return };
    let (window, root) = tool_window(ui, "People", "people", "People on your worker", 620);
    root.append(&text(
        "Each sees only the mail of their own addresses, and needs no Cloudflare account.",
        "column-sub",
    ));
    // Lined up with the headings' padding.
    let list = gtk::Box::new(gtk::Orientation::Vertical, 14);
    list.set_margin_start(14);
    list.set_margin_end(14);
    list.append(&text("Loading…", "column-sub"));
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(240)
        .max_content_height(480)
        .propagate_natural_height(true)
        .build();
    root.append(&scroller);

    root.append(&text("Invite someone", "column-title"));
    let form = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    form.set_margin_start(14);
    form.set_margin_end(14);
    let name = gtk::Entry::builder()
        .placeholder_text("name, like sam")
        .width_chars(12)
        .build();
    let addresses = gtk::Entry::builder()
        .placeholder_text("their addresses, comma-separated")
        .hexpand(true)
        .build();
    let invite_btn = gtk::Button::with_label("Invite");
    invite_btn.add_css_class("suggested");
    form.append(&name);
    form.append(&addresses);
    form.append(&invite_btn);
    root.append(&form);
    root.append(&text(
        "Their addresses' Email Routing must point at your worker: `cloudmail mailbox add <address> --route`.",
        "column-sub",
    ));

    let invite_box = gtk::Box::new(gtk::Orientation::Vertical, 6);
    invite_box.set_visible(false);
    invite_box.set_margin_start(14);
    invite_box.set_margin_end(14);
    root.append(&invite_box);
    let status = text("", "column-sub");
    status.set_selectable(true);
    root.append(&status);

    let p = Rc::new(People {
        client,
        window,
        list,
        status,
        invite: invite_box,
    });
    // Every handler holds `p` weakly.
    keep_until_closed(&p.window, &p);

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
                    say(&p, "Give them a name first: it's how you refer to them, like sam.");
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

fn say(p: &People, text: &str) {
    p.status.set_label(text);
}

fn copy(p: &People, text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
    say(p, "Copied.");
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
                    Err(e) => return say(&p, &e),
                };
                clear(&p.list);
                let others: Vec<User> = users.into_iter().filter(|u| !u.is_admin()).collect();
                if others.is_empty() {
                    p.list
                        .append(&text("Only you so far: invite someone below.", "column-sub"));
                }
                for u in others {
                    p.list.append(&row(&p, u));
                }
            }
        ),
    );
}

/// A button that runs `f` for this user.
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
        .label(format!("{} · {}", u.name, u.state()))
        .xalign(0.0)
        .hexpand(true)
        .build();
    top.append(&title);
    top.append(&action(
        p,
        if u.connected { "New link" } else { "New invite link" },
        "A one-time link for another computer; using it signs out the computer they use now",
        &u.name,
        |p, name| invite(p, name, Vec::new()),
    ));
    if u.connected {
        top.append(&action(
            p,
            "Sign out",
            "Signs them out everywhere; their mail stays, and a new link lets them back in",
            &u.name,
            |p, name| revoke(p, name, false),
        ));
    }
    let remove = action(
        p,
        "Remove…",
        "Deletes them and all their mail from your worker",
        &u.name,
        |p, name| revoke(p, name, true),
    );
    remove.add_css_class("error");
    top.append(&remove);
    row.append(&top);

    let boxes = if u.mailboxes.is_empty() {
        "No addresses yet".to_string()
    } else {
        u.mailboxes.join(", ")
    };
    let detail = text(&boxes, "column-sub");
    detail.set_selectable(true);
    row.append(&detail);

    let edits = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let address = gtk::Entry::builder()
        .placeholder_text("add an address")
        .hexpand(true)
        .build();
    let add = gtk::Button::with_label("Add");
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
        .placeholder_text("nobody")
        .tooltip_text("A verified Email Routing destination address; empty for none")
        .hexpand(true)
        .build();
    let save = gtk::Button::with_label("Save");
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
    fwd.append(&gtk::Label::new(Some("Copy of every message to")));
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
    util::run(
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
                    say(&p, &format!("{} is {}'s now.", m.email, m.user));
                    refresh(&p);
                }
                Err(e) => say(&p, &e),
            }
        ),
    );
}

fn set_forward(p: &Rc<People>, name: String, to: String) {
    let client = p.client.clone();
    let user = name.clone();
    util::run(
        move || {
            client
                .update_settings(Some(&user), &serde_json::json!({ "forward_to": to }))
                .map_err(|e| e.to_string())
        },
        clone!(
            #[weak]
            p,
            move |r: Result<crate::api::Settings, String>| match r {
                Ok(s) if s.forward_to.is_empty() => say(&p, &format!("{name}'s mail is no longer copied anywhere.")),
                Ok(s) => say(&p, &format!("A copy of {name}'s mail goes to {}.", s.forward_to)),
                Err(e) => say(&p, &e),
            }
        ),
    );
}

/// Invites someone new, or gives someone a fresh link (and any new addresses).
fn invite(p: &Rc<People>, name: String, mailboxes: Vec<String>) {
    say(p, &format!("Making a link for {name}…"));
    let client = p.client.clone();
    util::run(
        move || client.invite_user(&name, &mailboxes).map_err(|e| e.to_string()),
        clone!(
            #[weak]
            p,
            move |r: Result<(User, Invite), String>| match r {
                Ok((user, invite)) => {
                    show_invite(&p, &user.name, &invite);
                    refresh(&p);
                }
                Err(e) => say(&p, &e),
            }
        ),
    );
}

fn show_invite(p: &Rc<People>, name: &str, invite: &Invite) {
    clear(&p.invite);
    p.invite.append(&text(
        &format!(
            "{name}'s link works once, until {}. Send it privately (anyone who uses it first reads their mail). On their computer, with Cloudmail installed:",
            long_time(invite.expires_at)
        ),
        "column-sub",
    ));
    let command = format!("cloudmail connect {}", shell_arg(&invite.url));
    let shown = text(&command, "");
    shown.set_wrap_mode(pango::WrapMode::Char);
    shown.set_selectable(true);
    p.invite.append(&shown);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    for (label, value) in [("Copy command", command), ("Copy link", invite.url.clone())] {
        let b = gtk::Button::with_label(label);
        b.connect_clicked(clone!(
            #[weak]
            p,
            move |_| copy(&p, &value)
        ));
        buttons.append(&b);
    }
    p.invite.append(&buttons);
    p.invite.set_visible(true);
    say(p, "");
}

/// Signs someone out everywhere, or with `purge` removes them and all their mail, after asking.
fn revoke(p: &Rc<People>, name: String, purge: bool) {
    let (message, detail, action) = if purge {
        (
            format!("Remove {name} and delete their mail?"),
            "Their mail, Screener decisions, settings and addresses are deleted from your worker. This can't be undone.",
            "Remove",
        )
    } else {
        (
            format!("Sign {name} out everywhere?"),
            "Their mail stays; a new link lets them back in.",
            "Sign out",
        )
    };
    confirm(
        &p.window,
        &message,
        detail,
        ["Cancel", action],
        clone!(
            #[weak]
            p,
            move || {
                let client = p.client.clone();
                let who = name.clone();
                util::run(
                    move || client.revoke_user(&who, purge).map_err(|e| e.to_string()),
                    clone!(
                        #[weak]
                        p,
                        move |r: Result<(), String>| {
                            match r {
                                Ok(()) if purge => say(&p, &format!("Removed {name}.")),
                                Ok(()) => say(&p, &format!("Signed {name} out.")),
                                Err(e) => say(&p, &e),
                            }
                            refresh(&p);
                        }
                    ),
                );
            }
        ),
    );
}
