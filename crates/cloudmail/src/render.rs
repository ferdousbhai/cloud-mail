//! Human-readable rendering for terminals.

use cloudmail_api::text::{html_to_text, human_size, long_time, short_time};
use cloudmail_api::{Mailbox, PendingSender, Sender, ThreadDetail, ThreadSummary};

use crate::output::{bold, dim};

pub fn width() -> usize {
    std::env::var("COLUMNS").ok().and_then(|c| c.parse().ok()).unwrap_or(100).clamp(60, 200)
}

pub fn truncate(s: &str, max: usize) -> String {
    let s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if s.chars().count() <= max {
        return s;
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn pad(s: &str, w: usize) -> String {
    let n = s.chars().count();
    if n >= w { s.to_string() } else { format!("{s}{}", " ".repeat(w - n)) }
}

/// Server data made safe for a terminal: control characters (escape sequences that could move
/// the cursor, rewrite earlier lines such as the sender warning, set the clipboard or title)
/// become `?`. Newlines and tabs stay.
pub fn clean(s: &str) -> String {
    s.chars().map(|c| if c.is_control() && c != '\n' && c != '\t' { '?' } else { c }).collect()
}

/// `clean` for one-line fields, where a newline would also break the layout.
fn clean_line(s: &str) -> String {
    clean(s).replace(['\n', '\t'], " ")
}

pub fn threads(list: &[ThreadSummary], show_folder: bool) -> String {
    if list.is_empty() {
        return String::new();
    }
    let w = width();
    let id_w = list.iter().map(|t| t.id.chars().count()).max().unwrap_or(0);
    let from_w = 22;
    let when_w = 10;
    let folder_w = if show_folder { 10 } else { 0 };
    let subject_w = w.saturating_sub(id_w + from_w + when_w + folder_w + 8).max(20);
    list.iter()
        .map(|t| {
            let mark = if t.unread { "●" } else { " " };
            let from = clean_line(&t.from.as_ref().map(|a| a.display()).unwrap_or_default());
            let mut subject = clean_line(&t.subject);
            if t.has_attachments {
                subject.push_str(" 📎");
            }
            if t.message_count > 1 {
                subject.push_str(&format!(" ({})", t.message_count));
            }
            let folder = if show_folder { pad(&clean_line(&t.folder), folder_w) } else { String::new() };
            let line = format!(
                "{mark} {} {folder}{} {} {}",
                dim(&pad(&t.id, id_w)),
                pad(&truncate(&from, from_w), from_w),
                pad(&truncate(&subject, subject_w), subject_w),
                dim(&short_time(t.last_at)),
            );
            if t.unread { bold(&line) } else { line }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn thread(detail: &ThreadDetail, html: bool) -> String {
    let t = &detail.thread;
    let mut out = format!(
        "{}\n{}",
        bold(&clean_line(&t.subject)),
        dim(&clean_line(&format!("{} · {} · {}", t.id, t.folder, t.to_address.as_deref().unwrap_or(""))))
    );
    for m in &detail.messages {
        out.push_str(&format!("\n\n{}\n", dim(&"─".repeat(width().min(80)))));
        out.push_str(&format!(
            "{} <{}>{}\n",
            bold(&clean_line(&m.from.display())),
            clean_line(&m.from.email),
            if m.outgoing { dim("  (sent)") } else { String::new() }
        ));
        if m.dmarc_failed() {
            out.push_str("⚠ sender not verified (its domain didn't authenticate this message): the From address may be forged\n");
        }
        let to = m.to.iter().map(|a| a.formatted()).collect::<Vec<_>>().join(", ");
        out.push_str(&dim(&clean_line(&format!("to {to}"))));
        if !m.cc.is_empty() {
            out.push_str(&dim(&clean_line(&format!(" · cc {}", m.cc.iter().map(|a| a.formatted()).collect::<Vec<_>>().join(", ")))));
        }
        out.push('\n');
        out.push_str(&dim(&clean_line(&format!("{} · {}", long_time(m.date), m.id))));
        out.push_str("\n\n");
        let body = if html {
            m.html.clone().or_else(|| m.text.clone()).unwrap_or_default()
        } else {
            match (&m.text, &m.html) {
                (Some(t), _) if !t.trim().is_empty() => t.clone(),
                (_, Some(h)) => html_to_text(h),
                _ => String::new(),
            }
        };
        out.push_str(&clean(body.trim_end()));
        let atts: Vec<_> = m.attachments.iter().filter(|a| !a.inline).collect();
        if !atts.is_empty() {
            out.push('\n');
            for a in atts {
                out.push_str(&format!("\n📎 {} ({}, {}) {}", clean_line(&a.filename), clean_line(&a.mime_type), human_size(a.size), dim(&clean_line(&a.id))));
            }
        }
    }
    out
}

pub fn screener(list: &[PendingSender]) -> String {
    let w = width();
    list.iter()
        .map(|s| {
            let who = if s.name.as_deref().is_some_and(|n| !n.trim().is_empty()) {
                clean_line(&format!("{} <{}>", s.display(), s.email))
            } else {
                clean_line(&s.email)
            };
            let subject = clean_line(&s.last_subject.clone().unwrap_or_default());
            let count = if s.thread_count > 1 { format!(" (+{} more)", s.thread_count - 1) } else { String::new() };
            format!(
                "{}  {}{}  {}",
                pad(&truncate(&who, 40), 40),
                truncate(&subject, w.saturating_sub(62).max(20)),
                dim(&count),
                dim(&short_time(s.last_at))
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn senders(list: &[Sender]) -> String {
    list.iter()
        .map(|s| {
            let when = s.decided_at.map(short_time).unwrap_or_default();
            format!("{}  {}  {}", pad(&clean_line(&s.email), 36), pad(&truncate(&clean_line(&s.name), 28), 28), dim(&when))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn mailboxes(list: &[Mailbox]) -> String {
    let w = list.iter().map(|m| m.email.len()).max().unwrap_or(0);
    list.iter()
        .enumerate()
        .map(|(i, m)| {
            let kind = if m.screen { "screened" } else { "direct" };
            let default = if i == 0 { dim("  (default From)") } else { String::new() };
            format!("{}  {}  {}{default}", pad(&clean_line(&m.email), w), pad(kind, 8), clean_line(&m.name))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_escapes_from_mail_are_neutralised() {
        let evil = "hi\x1b[2A\x1b[2K\x1b]52;c;cGF5bG9hZA==\x07\u{9b}31m\nnext\tline";
        let out = clean(evil);
        assert!(!out.chars().any(|c| c.is_control() && c != '\n' && c != '\t'), "{out:?}");
        assert!(out.contains("\nnext\tline"));
        assert!(!clean_line(evil).contains('\n'));
    }

    #[test]
    fn truncates() {
        assert_eq!(truncate("hello   world", 20), "hello world");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
    }
}
