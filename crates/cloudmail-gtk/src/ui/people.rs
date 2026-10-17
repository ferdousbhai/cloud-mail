//! The People window, for the admin (who deployed the worker to their Cloudflare account): who
//! else reads their mail on it, inviting someone, a fresh link, adding an address, their copy of
//! every message, and signing them out or removing them. What it does is `cloudmail user`'s.

use gtk::{gdk, gio, glib, glib::clone, prelude::*};
use std::rc::Rc;

use super::Ui;
use crate::api::{Client, Invite, MailboxUpdate, User};
use crate::util;

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
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.add_css_class("accounts");
    root.set_margin_top(18);
    root.set_margin_bottom(18);
    root.set_margin_start(18);
    root.set_margin_end(18);
    let heading = gtk::Label::builder().label("People on your worker").xalign(0.0).build();
    heading.add_css_class("column-title");
    root.append(&heading);
    let sub = gtk::Label::builder()
        .label("Each sees only the mail of their own addresses, and needs no Cloudflare account.")
        .xalign(0.0)
        .wrap(true)
        .build();
    sub.add_css_class("column-sub");
    root.append(&sub);
    // Lined up with the headings' padding.
    let list = gtk::Box::new(gtk::Orientation::Vertical, 14);
    list.set_margin_start(14);
    list.set_margin_end(14);
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(240)
        .max_content_height(480)
        .propagate_natural_height(true)
        .build();
    root.append(&scroller);

    let invite_heading = gtk::Label::builder().label("Invite someone").xalign(0.0).build();
    invite_heading.add_css_class("column-title");
    root.append(&invite_heading);
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
    let note = gtk::Label::builder()
        .label("Their addresses' Email Routing must point at your worker: `cloudmail mailbox add <address> --route`.")
        .xalign(0.0)
        .wrap(true)
        .build();
    note.add_css_class("column-sub");
    root.append(&note);

    let latest = gtk::Box::new(gtk::Orientation::Vertical, 6);
    latest.set_visible(false);
    latest.set_margin_start(14);
    latest.set_margin_end(14);
    root.append(&latest);
    let status = gtk::Label::builder().xalign(0.0).wrap(true).selectable(true).build();
    status.add_css_class("column-sub");
    root.append(&status);

    let window = gtk::Window::builder()
        .title("People")
        .transient_for(&ui.window)
        .modal(true)
        .default_width(620)
        .child(&root)
        .build();
    window.add_css_class("cloudmail");
    let p = Rc::new(People {
        client,
        window,
        list,
        status,
        invite: latest,
    });
    // Every handler holds `p` weakly, so the window keeps it until it closes.
    let keep = std::cell::RefCell::new(Some(p.clone()));
    p.window.connect_destroy(move |_| drop(keep.take()));

    let submit = clone!(
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
    );
    invite_btn.connect_clicked(clone!(
        #[strong]
        submit,
        move |_| submit()
    ));
    addresses.connect_activate(move |_| submit());

    refresh(&p);
    p.window.present();
}

fn split_addresses(text: &str) -> Vec<String> {
    text.split([',', ' ', ';'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn say(p: &People, text: &str) {
    p.status.set_label(text);
}

fn when(ms: i64) -> String {
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|d| d.format("%b %e, %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn copy(p: &People, text: &str) {
    if let Some(display) = gdk::Display::default() {
        display.clipboard().set_text(text);
    }
    say(p, "Copied.");
}

/// Everyone but the admin, each with their copy-of-every-message setting, off the main loop.
fn refresh(p: &Rc<People>) {
    while let Some(child) = p.list.first_child() {
        p.list.remove(&child);
    }
    p.list
        .append(&gtk::Label::builder().label("Loading…").xalign(0.0).build());
    let client = p.client.clone();
    util::run(
        move || -> Result<Vec<(User, Option<String>)>, String> {
            let users = client.users().map_err(|e| e.to_string())?;
            Ok(users
                .into_iter()
                .filter(|u| u.role != "admin")
                .map(|u| {
                    let forward = client.settings(Some(&u.name)).ok().map(|s| s.forward_to);
                    (u, forward)
                })
                .collect())
        },
        clone!(
            #[weak]
            p,
            move |r: Result<Vec<(User, Option<String>)>, String>| {
                while let Some(child) = p.list.first_child() {
                    p.list.remove(&child);
                }
                match r {
                    Ok(users) if users.is_empty() => p.list.append(
                        &gtk::Label::builder()
                            .label("Only you so far: invite someone below.")
                            .xalign(0.0)
                            .build(),
                    ),
                    Ok(users) => {
                        for (u, forward) in users {
                            p.list.append(&row(&p, u, forward));
                        }
                    }
                    Err(e) => say(&p, &e),
                }
            }
        ),
    );
}

fn state(u: &User) -> String {
    if u.revoked {
        "signed out".into()
    } else if u.connected {
        "connected".into()
    } else if let Some(t) = u.invite_expires_at {
        format!("invited, link valid until {}", when(t))
    } else {
        "not connected, no valid link".into()
    }
}

fn row(p: &Rc<People>, u: User, forward: Option<String>) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let title = gtk::Label::builder()
        .label(format!("{} ({})", u.name, state(&u)))
        .xalign(0.0)
        .hexpand(true)
        .build();
    top.append(&title);
    let name = u.name.clone();

    let link = gtk::Button::with_label(if u.connected { "New link" } else { "New invite link" });
    link.set_tooltip_text(Some(
        "A one-time link for another computer; using it signs out the computer they use now",
    ));
    link.connect_clicked(clone!(
        #[weak]
        p,
        #[strong]
        name,
        move |_| invite(&p, name.clone(), Vec::new())
    ));
    top.append(&link);
    if u.connected {
        let out = gtk::Button::with_label("Sign out");
        out.set_tooltip_text(Some(
            "Signs them out everywhere; their mail stays, and a new link lets them back in",
        ));
        out.connect_clicked(clone!(
            #[weak]
            p,
            #[strong]
            name,
            move |_| revoke(&p, name.clone(), false)
        ));
        top.append(&out);
    }
    let remove = gtk::Button::with_label("Remove…");
    remove.add_css_class("error");
    remove.connect_clicked(clone!(
        #[weak]
        p,
        #[strong]
        name,
        move |_| revoke(&p, name.clone(), true)
    ));
    top.append(&remove);
    row.append(&top);

    let boxes = if u.mailboxes.is_empty() {
        "No addresses yet".to_string()
    } else {
        u.mailboxes.join(", ")
    };
    let detail = gtk::Label::builder()
        .label(&boxes)
        .xalign(0.0)
        .wrap(true)
        .selectable(true)
        .build();
    detail.add_css_class("column-sub");
    row.append(&detail);

    let edits = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let address = gtk::Entry::builder()
        .placeholder_text("add an address")
        .hexpand(true)
        .build();
    let add = gtk::Button::with_label("Add");
    let add_address = clone!(
        #[weak]
        p,
        #[weak]
        address,
        #[strong]
        name,
        move || {
            let email = address.text().trim().to_string();
            if email.is_empty() {
                return;
            }
            let client = p.client.clone();
            let user = name.clone();
            util::run(
                move || {
                    let update = MailboxUpdate {
                        user: Some(user),
                        screen: Some(true),
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
    );
    add.connect_clicked(clone!(
        #[strong]
        add_address,
        move |_| add_address()
    ));
    address.connect_activate(move |_| add_address());
    edits.append(&address);
    edits.append(&add);
    row.append(&edits);

    // A copy of every message elsewhere (often their old inbox, while they move over).
    if let Some(forward) = forward {
        let fwd = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let entry = gtk::Entry::builder()
            .text(&forward)
            .placeholder_text("nobody")
            .tooltip_text("A verified Email Routing destination address; empty for none")
            .hexpand(true)
            .build();
        let save = gtk::Button::with_label("Save");
        let set = clone!(
            #[weak]
            p,
            #[weak]
            entry,
            #[strong]
            name,
            move || {
                let to = entry.text().trim().to_string();
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
                        #[strong]
                        name,
                        move |r: Result<crate::api::Settings, String>| match r {
                            Ok(s) if s.forward_to.is_empty() =>
                                say(&p, &format!("{name}'s mail is no longer copied anywhere.")),
                            Ok(s) => say(&p, &format!("A copy of {name}'s mail goes to {}.", s.forward_to)),
                            Err(e) => say(&p, &e),
                        }
                    ),
                );
            }
        );
        save.connect_clicked(clone!(
            #[strong]
            set,
            move |_| set()
        ));
        entry.connect_activate(move |_| set());
        fwd.append(&gtk::Label::new(Some("Copy of every message to")));
        fwd.append(&entry);
        fwd.append(&save);
        row.append(&fwd);
    }
    row
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
    while let Some(child) = p.invite.first_child() {
        p.invite.remove(&child);
    }
    let text = format!(
        "{name}'s link works once, until {}. Send it privately (anyone who uses it first reads their mail). On their computer, with Cloudmail installed:",
        when(invite.expires_at)
    );
    p.invite
        .append(&gtk::Label::builder().label(&text).xalign(0.0).wrap(true).build());
    let command = format!("cloudmail connect '{}'", invite.url);
    let shown = gtk::Label::builder()
        .label(&command)
        .xalign(0.0)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::Char)
        .selectable(true)
        .build();
    p.invite.append(&shown);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let copy_command = gtk::Button::with_label("Copy command");
    copy_command.connect_clicked(clone!(
        #[weak]
        p,
        move |_| copy(&p, &command)
    ));
    let copy_link = gtk::Button::with_label("Copy link");
    let url = invite.url.clone();
    copy_link.connect_clicked(clone!(
        #[weak]
        p,
        move |_| copy(&p, &url)
    ));
    buttons.append(&copy_command);
    buttons.append(&copy_link);
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
    let dialog = gtk::AlertDialog::builder()
        .message(message)
        .detail(detail)
        .buttons(["Cancel", action])
        .cancel_button(0)
        .default_button(0)
        .modal(true)
        .build();
    dialog.choose(
        Some(&p.window),
        gio::Cancellable::NONE,
        clone!(
            #[weak]
            p,
            move |res| {
                if res != Ok(1) {
                    return;
                }
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
