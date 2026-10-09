//! The Accounts window: your linked accounts (HEY, Gmail, iCloud Mail) with whether each works,
//! signing one in again, unlinking it, and linking another. What it does is `cloudmail account`'s
//! (both call `cloudmail_api::accounts`); a change shows once Cloudmail restarts.

use gtk::{gio, glib, glib::clone, prelude::*};
use std::rc::Rc;

use super::Ui;
use crate::api::accounts::{self, Link};
use crate::api::{AccountStatus, config};
use crate::util;

struct Accounts {
    ui: Rc<Ui>,
    window: gtk::Window,
    list: gtk::Box,
    status: gtk::Label,
    restart: gtk::Button,
}

pub fn show(ui: &Rc<Ui>) {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 12);
    root.add_css_class("accounts");
    root.set_margin_top(18);
    root.set_margin_bottom(18);
    root.set_margin_start(18);
    root.set_margin_end(18);
    let heading = gtk::Label::builder().label("Linked accounts").xalign(0.0).build();
    heading.add_css_class("column-title");
    root.append(&heading);
    let list = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.append(&list);

    let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let add_label = gtk::Label::builder().label("Link").xalign(0.0).build();
    add_row.append(&add_label);
    let buttons: Vec<(&str, gtk::Button)> = [("gmail", "Gmail"), ("icloud", "iCloud Mail"), ("hey", "HEY")]
        .into_iter()
        .map(|(provider, title)| (provider, gtk::Button::with_label(title)))
        .collect();
    for (_, b) in &buttons {
        add_row.append(b);
    }
    root.append(&add_row);

    let status = gtk::Label::builder().xalign(0.0).wrap(true).build();
    status.add_css_class("column-sub");
    root.append(&status);
    let restart = gtk::Button::with_label("Restart Cloudmail");
    restart.add_css_class("suggested");
    restart.set_visible(false);
    root.append(&restart);

    let window = gtk::Window::builder().title("Accounts").transient_for(&ui.window).modal(true).default_width(560).child(&root).build();
    window.add_css_class("cloudmail");
    let a = Rc::new(Accounts { ui: ui.clone(), window, list, status, restart });

    for (provider, button) in buttons {
        button.connect_clicked(clone!(#[weak] a, move |b| link(&a, provider, b)));
    }
    a.restart.connect_clicked(clone!(#[weak] a, move |_| {
        crate::RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(app) = a.ui.window.application() {
            app.quit();
        }
    }));
    refresh(&a);
    a.window.present();
}

fn say(a: &Accounts, text: &str) {
    a.status.set_label(text);
}

/// Something changed: the app shows it after a restart.
fn changed(a: &Rc<Accounts>, text: &str) {
    say(a, &format!("{text}. Restart Cloudmail to show it."));
    a.restart.set_visible(true);
    refresh(a);
}

/// Reads each linked account's status, off the main loop.
fn refresh(a: &Rc<Accounts>) {
    while let Some(child) = a.list.first_child() {
        a.list.remove(&child);
    }
    let loading = gtk::Label::builder().label("Checking your accounts…").xalign(0.0).build();
    a.list.append(&loading);
    util::run(
        || -> Result<Vec<AccountStatus>, String> {
            let file = config::read_file(&config::path())?.unwrap_or_default();
            Ok(file
                .accounts
                .iter()
                .map(|(name, cfg)| match accounts::open(name) {
                    Ok(p) => p.status(),
                    Err(e) => AccountStatus { name: name.clone(), provider: cfg.provider(name).to_string(), label: name.clone(), ok: false, addresses: Vec::new(), detail: e.message },
                })
                .collect())
        },
        clone!(#[weak] a, move |result: Result<Vec<AccountStatus>, String>| {
            while let Some(child) = a.list.first_child() {
                a.list.remove(&child);
            }
            match result {
                Ok(list) if list.is_empty() => a.list.append(&gtk::Label::builder().label("None yet: link one below.").xalign(0.0).build()),
                Ok(list) => {
                    for s in list {
                        a.list.append(&row(&a, s));
                    }
                }
                Err(e) => say(&a, &e),
            }
        }),
    );
}

fn row(a: &Rc<Accounts>, s: AccountStatus) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let state = if s.ok { "signed in" } else { "needs attention" };
    let title = format!("{} ({}, {state})", s.label, s.name);
    text.append(&gtk::Label::builder().label(&title).xalign(0.0).build());
    let detail = if s.addresses.is_empty() { s.detail.clone() } else { format!("{}\n{}", s.addresses.join(", "), s.detail) };
    let detail = gtk::Label::builder().label(&detail).xalign(0.0).wrap(true).build();
    detail.add_css_class("column-sub");
    text.append(&detail);
    row.append(&text);
    if !s.ok {
        let sign_in = gtk::Button::with_label("Sign in");
        let name = s.name.clone();
        sign_in.connect_clicked(clone!(#[weak] a, move |b| {
            b.set_sensitive(false);
            say(&a, "Finish signing in in the browser or sign-in window…");
            let name = name.clone();
            util::run(
                move || accounts::sign_in(&name).map_err(|e| e.to_string()),
                clone!(#[weak] a, move |r: Result<AccountStatus, String>| match r {
                    Ok(s) => changed(&a, &format!("Signed in to {}", s.label)),
                    Err(e) => {
                        say(&a, &e);
                        refresh(&a);
                    }
                }),
            );
        }));
        row.append(&sign_in);
    }
    let remove = gtk::Button::with_label("Unlink");
    let (name, label) = (s.name.clone(), s.label.clone());
    remove.connect_clicked(clone!(#[weak] a, move |_| {
        let dialog = gtk::AlertDialog::builder()
            .message(format!("Unlink {label}?"))
            .detail("Its mail stops showing here; nothing changes in the account itself.")
            .buttons(["Cancel", "Unlink"])
            .cancel_button(0)
            .default_button(0)
            .modal(true)
            .build();
        let name = name.clone();
        dialog.choose(Some(&a.window), gio::Cancellable::NONE, clone!(#[weak] a, move |res| {
            if res != Ok(1) {
                return;
            }
            let name = name.clone();
            util::run(
                move || accounts::unlink(&name).map(|_| name).map_err(|e| e.message),
                clone!(#[weak] a, move |r: Result<String, String>| match r {
                    Ok(name) => changed(&a, &format!("Unlinked {name}")),
                    Err(e) => say(&a, &e),
                }),
            );
        }));
    }));
    row.append(&remove);
    row
}

/// Links an account, signing in when needed (the browser, or icloud-session's window).
fn link(a: &Rc<Accounts>, provider: &'static str, button: &gtk::Button) {
    button.set_sensitive(false);
    say(a, "Linking… finish any sign-in in the browser or sign-in window.");
    let mail = a.ui.mail.clone();
    let button = button.clone();
    util::run(
        move || {
            let req = Link { provider: provider.into(), can_sign_in: true, ..Default::default() };
            accounts::link(&req, mail.as_ref(), &|_| {}).map_err(|e| e.to_string())
        },
        clone!(#[weak] a, move |r: Result<accounts::Linked, String>| {
            button.set_sensitive(true);
            match r {
                Ok(l) => {
                    let mut text = format!("Linked {}{}", l.label, if l.addresses.is_empty() { String::new() } else { format!(" ({})", l.addresses.join(", ")) });
                    if let Some(Err(why)) = &l.screened_in {
                        text.push_str(&format!("; its correspondents couldn't be screened in ({why})"));
                    }
                    changed(&a, &text);
                }
                Err(e) => say(&a, &e),
            }
        }),
    );
}
