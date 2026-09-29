use gtk::{pango, prelude::*};

use crate::api::{PendingSender, ThreadSummary};
use crate::util::short_time;

fn label(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::builder()
        .label(text)
        .xalign(0.0)
        .ellipsize(pango::EllipsizeMode::End)
        .single_line_mode(true)
        .build();
    l.add_css_class(class);
    l
}

/// `default_email` hides the receiving-address label for mail sent to your main address.
pub fn thread_row(t: &ThreadSummary, sent_view: bool, default_email: Option<&str>) -> gtk::ListBoxRow {
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let dot = label(if t.unread { "●" } else { " " }, "unread-dot");
    top.append(&dot);
    let who = t.from.as_ref().map(|a| a.display()).unwrap_or_else(|| "(unknown)".into());
    let who = if t.message_count > 1 { format!("{who} ({})", t.message_count) } else { who };
    let sender = label(&who, "sender");
    sender.set_hexpand(true);
    top.append(&sender);
    if t.has_attachments {
        top.append(&label("\u{f0c6}", "time"));
    }
    top.append(&label(&short_time(t.last_at), "time"));

    let subject = if t.subject.trim().is_empty() { "(no subject)" } else { t.subject.as_str() };
    let body = gtk::Box::new(gtk::Orientation::Vertical, 2);
    body.append(&top);
    let subject = label(subject, "subject");
    subject.set_margin_start(16);
    body.append(&subject);
    let bottom = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    bottom.set_margin_start(16);
    let snippet = label(&t.snippet, "snippet");
    snippet.set_hexpand(true);
    bottom.append(&snippet);
    if let (Some(to), Some(default)) = (t.to_address.as_deref(), default_email)
        && !sent_view && !to.eq_ignore_ascii_case(default) {
            // Not ellipsized: the snippet gives way so the address stays readable.
            let to_label = gtk::Label::new(Some(to));
            to_label.add_css_class("time");
            to_label.set_tooltip_text(Some(&format!("Sent to {to}")));
            bottom.append(&to_label);
        }
    body.append(&bottom);
    if t.unread {
        body.add_css_class("unread");
    }
    gtk::ListBoxRow::builder().child(&body).build()
}

pub fn set_row_unread(row: &gtk::ListBoxRow, unread: bool) {
    let Some(body) = row.child() else { return };
    if unread {
        body.add_css_class("unread");
    } else {
        body.remove_css_class("unread");
    }
    if let Some(dot) = body
        .first_child()
        .and_then(|top| top.first_child())
        .and_then(|w| w.downcast::<gtk::Label>().ok())
    {
        dot.set_label(if unread { "●" } else { " " });
    }
}

pub struct SenderRow {
    pub row: gtk::ListBoxRow,
    pub approve: gtk::Button,
    pub block: gtk::Button,
}

pub fn sender_row(s: &PendingSender) -> SenderRow {
    let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
    text.set_hexpand(true);
    let top = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let name = label(&s.display(), "sender");
    name.set_hexpand(true);
    top.append(&name);
    top.append(&label(&short_time(s.last_at), "time"));
    text.append(&top);
    if s.display() != s.email {
        text.append(&label(&s.email, "snippet"));
    }
    let count = if s.thread_count > 1 { format!("{} emails · ", s.thread_count) } else { String::new() };
    text.append(&label(
        &format!("{count}{}", s.last_subject.as_deref().unwrap_or("(no subject)")),
        "subject",
    ));
    text.add_css_class("unread");

    let approve = gtk::Button::builder().label("Yes").tooltip_text("Let them in (y)").build();
    approve.add_css_class("approve");
    let block = gtk::Button::builder().label("No").tooltip_text("Block them (n)").build();
    block.add_css_class("block");
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    buttons.set_valign(gtk::Align::Center);
    buttons.append(&approve);
    buttons.append(&block);

    let body = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    body.append(&text);
    body.append(&buttons);
    SenderRow { row: gtk::ListBoxRow::builder().child(&body).build(), approve, block }
}
