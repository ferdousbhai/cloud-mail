//! The Accounts window: your linked accounts (HEY, Gmail, iCloud Mail) with whether each works,
//! signing one in again, unlinking it, and linking another. What it does is `cloudmail account`'s
//! (both call `cloudmail_api::accounts`); a change shows once Cloudmail restarts.

use gtk::{glib, glib::clone, prelude::*};
use omui::tr;
use std::cell::RefCell;
use std::rc::Rc;

use super::{Live, Ui, bind_title, keep_until_closed, tool_window};
use crate::api::accounts::{self, Link};
use crate::api::{AccountStatus, config};
use crate::util;

struct Accounts {
    ui: Rc<Ui>,
    window: gtk::Window,
    list: gtk::Box,
    /// The accounts as last read, to be shown again after a switch of language; none while they are being read.
    statuses: RefCell<Option<Result<Vec<AccountStatus>, String>>>,
    status: Live,
    restart: gtk::Button,
}

pub fn show(ui: &Rc<Ui>) {
    let (window, root, heading) = tool_window(ui, "accounts-window", 560);
    bind_title(&window, "Accounts");
    omui::i18n::bind_label(&heading, "Linked accounts");
    let list = gtk::Box::new(gtk::Orientation::Vertical, 10);
    root.append(&list);

    let add_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let add_label = gtk::Label::builder().xalign(0.0).build();
    omui::i18n::bind_label(&add_label, "Link");
    add_row.append(&add_label);
    let buttons: Vec<(&str, gtk::Button)> = [("gmail", "Gmail"), ("icloud", "iCloud Mail"), ("hey", "HEY")]
        .into_iter()
        .map(|(provider, title)| (provider, gtk::Button::with_label(title)))
        .collect();
    for (_, b) in &buttons {
        add_row.append(b);
    }
    root.append(&add_row);

    let status = Live::new(omui::label("", "column-sub"));
    root.append(&status.label);
    let restart = gtk::Button::new();
    omui::i18n::bind_label(&restart, "Restart Cloudmail");
    restart.add_css_class("suggested-action");
    restart.set_visible(false);
    root.append(&restart);

    let a = Rc::new(Accounts {
        ui: ui.clone(),
        window,
        list,
        statuses: RefCell::new(None),
        status,
        restart,
    });
    // Every handler holds `a` weakly.
    keep_until_closed(&a.window, &a);
    omui::i18n::on_change(
        &a.window,
        clone!(
            #[weak]
            a,
            move || render(&a)
        ),
    );

    for (provider, button) in buttons {
        button.connect_clicked(clone!(
            #[weak]
            a,
            move |b| link(&a, provider, b)
        ));
    }
    a.restart.connect_clicked(clone!(
        #[weak]
        a,
        move |_| {
            // By closing the main window rather than quitting outright: a draft being written asks first, and what
            // is still on its way (a send, a delete waiting out its undo) holds the app until it is done. The app
            // ends, and starts again, once nothing is.
            crate::RESTART.store(true, std::sync::atomic::Ordering::SeqCst);
            a.ui.window.close();
            if a.ui.is_open() {
                // A draft kept the window open; closing it later is not a restart.
                crate::RESTART.store(false, std::sync::atomic::Ordering::SeqCst);
            }
        }
    ));
    refresh(&a);
    a.window.present();
}

/// Says `text()` under the list, in the language of the moment.
fn say(a: &Accounts, text: impl Fn() -> String + 'static) {
    a.status.set(text);
}

/// Something changed, which `text()` says in a sentence of its own: the app shows it after a restart.
fn changed(a: &Rc<Accounts>, text: impl Fn() -> String + 'static) {
    say(a, move || omui::tr!("{} Restart Cloudmail to show it.", text()));
    a.restart.set_visible(true);
    refresh(a);
}

/// Shows the accounts as last read.
fn render(a: &Rc<Accounts>) {
    omui::clear(&a.list);
    match &*a.statuses.borrow() {
        None => a
            .list
            .append(&omui::label(&tr("Checking your accounts…"), "column-sub")),
        Some(Ok(list)) if list.is_empty() => a
            .list
            .append(&omui::label(&tr("None yet: link one below."), "column-sub")),
        Some(Ok(list)) => {
            for s in list {
                a.list.append(&row(a, s.clone()));
            }
        }
        Some(Err(_)) => {}
    }
}

/// Reads each linked account's status, off the main loop.
fn refresh(a: &Rc<Accounts>) {
    a.statuses.replace(None);
    render(a);
    util::run(
        || -> Result<Vec<AccountStatus>, String> {
            let file = config::read_file(&config::path())?.unwrap_or_default();
            Ok(file
                .accounts
                .iter()
                .map(|(name, cfg)| match accounts::open(name) {
                    Ok(p) => p.status(),
                    Err(e) => AccountStatus {
                        name: name.clone(),
                        provider: cfg.provider(name).to_string(),
                        label: name.clone(),
                        ok: false,
                        addresses: Vec::new(),
                        detail: e.message,
                    },
                })
                .collect())
        },
        clone!(
            #[weak]
            a,
            move |result: Result<Vec<AccountStatus>, String>| {
                if let Err(e) = &result {
                    a.status.set_plain(e);
                }
                a.statuses.replace(Some(result));
                render(&a);
            }
        ),
    );
}

fn row(a: &Rc<Accounts>, s: AccountStatus) -> gtk::Box {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let text_box = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text_box.set_hexpand(true);
    let title = if s.ok {
        omui::tr!("{} ({}, signed in)", s.label, s.name)
    } else {
        omui::tr!("{} ({}, needs attention)", s.label, s.name)
    };
    text_box.append(&gtk::Label::builder().label(&title).xalign(0.0).build());
    let detail = if s.addresses.is_empty() {
        s.detail.clone()
    } else {
        format!("{}\n{}", s.addresses.join(", "), s.detail)
    };
    text_box.append(&omui::label(&detail, "column-sub"));
    row.append(&text_box);
    if !s.ok {
        let sign_in = gtk::Button::with_label(&tr("Sign in"));
        let name = s.name.clone();
        sign_in.connect_clicked(clone!(
            #[weak]
            a,
            move |b| {
                b.set_sensitive(false);
                say(&a, || tr("Finish signing in in the browser or sign-in window…"));
                let name = name.clone();
                util::run(
                    move || accounts::sign_in(&name).map_err(|e| e.to_string()),
                    clone!(
                        #[weak]
                        a,
                        move |r: Result<AccountStatus, String>| match r {
                            Ok(s) => changed(&a, move || omui::tr!("Signed in to {}.", s.label)),
                            Err(e) => {
                                a.status.set_plain(&e);
                                refresh(&a);
                            }
                        }
                    ),
                );
            }
        ));
        row.append(&sign_in);
    }
    let remove = gtk::Button::with_label(&tr("Unlink"));
    let (name, label) = (s.name.clone(), s.label.clone());
    remove.connect_clicked(clone!(
        #[weak]
        a,
        move |_| {
            let name = name.clone();
            let label = label.clone();
            super::ask_destructive(
                &a.window,
                move || {
                    (
                        omui::tr!("Unlink {}?", label),
                        tr("Its mail stops showing here; nothing changes in the account itself."),
                        tr("Unlink"),
                    )
                },
                clone!(
                    #[weak]
                    a,
                    move || {
                        util::write(
                            move || accounts::unlink(&name).map(|_| name).map_err(|e| e.message),
                            clone!(
                                #[weak]
                                a,
                                move |r: Result<String, String>| match r {
                                    Ok(name) => changed(&a, move || omui::tr!("Unlinked {}.", name)),
                                    Err(e) => a.status.set_plain(&e),
                                }
                            ),
                        );
                    }
                ),
            );
        }
    ));
    row.append(&remove);
    row
}

/// Links an account, signing in when needed (the browser, or icloud-session's window).
fn link(a: &Rc<Accounts>, provider: &'static str, button: &gtk::Button) {
    button.set_sensitive(false);
    say(a, || {
        tr("Linking… finish any sign-in in the browser or sign-in window.")
    });
    let mail = a.ui.mail.clone();
    let button = button.clone();
    // Like a sign-in, this waits on the browser, so closing the app abandons it rather than holding the app open.
    util::run(
        move || {
            // Gmail: one more account each time (gmail, then gmail-2, …). The others have one sign-in each.
            let name = match provider {
                "gmail" => Some(accounts::next_name(provider).map_err(|e| e.to_string())?),
                _ => None,
            };
            let req = Link {
                provider: provider.into(),
                name,
                can_sign_in: true,
                ..Default::default()
            };
            accounts::link(&req, mail.as_ref(), &|_| {}).map_err(|e| e.to_string())
        },
        clone!(
            #[weak]
            a,
            move |r: Result<accounts::Linked, String>| {
                button.set_sensitive(true);
                match r {
                    Ok(l) => {
                        let addresses = l.addresses.join(", ");
                        let unscreened = match &l.screened_in {
                            Some(Err(why)) => Some(why.to_string()),
                            _ => None,
                        };
                        changed(&a, move || {
                            let linked = if addresses.is_empty() {
                                omui::tr!("Linked {}.", l.label)
                            } else {
                                omui::tr!("Linked {} ({}).", l.label, addresses)
                            };
                            match &unscreened {
                                Some(why) => {
                                    omui::tr!("{} Its correspondents couldn't be screened in ({}).", linked, why)
                                }
                                None => linked,
                            }
                        });
                    }
                    Err(e) => a.status.set_plain(&e),
                }
            }
        ),
    );
}
