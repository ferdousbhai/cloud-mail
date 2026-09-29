use chrono::{Datelike, Local, TimeZone};

pub fn short_time(ms: i64) -> String {
    let Some(t) = Local.timestamp_millis_opt(ms).single() else {
        return String::new();
    };
    let now = Local::now();
    let age = now.signed_duration_since(t);
    if t.date_naive() == now.date_naive() {
        t.format("%H:%M").to_string()
    } else if age.num_days() < 6 && age.num_seconds() >= 0 {
        t.format("%a").to_string()
    } else if t.year() == now.year() {
        t.format("%b %-d").to_string()
    } else {
        t.format("%Y-%m-%d").to_string()
    }
}

pub fn long_time(ms: i64) -> String {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%a, %b %-d %Y at %H:%M").to_string())
        .unwrap_or_default()
}

/// Splits "a@b.com, Last, First <c@d.com>" into addresses, keeping commas that
/// belong to display names.
pub fn split_addresses(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut in_angle = false;
    for ch in input.chars() {
        match ch {
            '"' => in_quotes = !in_quotes,
            '<' => in_angle = true,
            '>' => in_angle = false,
            ',' | ';' if !in_quotes && !in_angle
                && current.contains('@') => {
                    out.push(current.trim().to_string());
                    current.clear();
                    continue;
                }
            _ => {}
        }
        current.push(ch);
    }
    if !current.trim().is_empty() {
        out.push(current.trim().to_string());
    }
    out.retain(|a| !a.is_empty());
    out
}

pub fn reply_subject(subject: &str) -> String {
    let s = subject.trim();
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("re:") || lower.starts_with("re :") {
        s.to_string()
    } else {
        format!("Re: {s}")
    }
}

pub fn quote(text: &str, who: &str, date_ms: i64) -> String {
    let mut out = format!("\n\nOn {}, {who} wrote:\n", long_time(date_ms));
    for line in text.trim_end().lines() {
        if line.starts_with('>') {
            out.push_str(&format!(">{line}\n"));
        } else if line.is_empty() {
            out.push_str(">\n");
        } else {
            out.push_str(&format!("> {line}\n"));
        }
    }
    out
}

/// Very small HTML → text for quoting HTML-only mails.
pub fn html_to_text(html: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    let mut tag = String::new();
    let mut skip = false;
    for ch in html.chars() {
        if in_tag {
            if ch == '>' {
                in_tag = false;
                let t = tag.trim().to_ascii_lowercase();
                let name: String = t.trim_start_matches('/').chars().take_while(|c| c.is_ascii_alphanumeric()).collect();
                if name == "style" || name == "script" || name == "head" {
                    skip = !t.starts_with('/');
                }
                if matches!(name.as_str(), "br" | "p" | "div" | "tr" | "li" | "h1" | "h2" | "h3" | "table") {
                    out.push('\n');
                }
                tag.clear();
            } else {
                tag.push(ch);
            }
        } else if ch == '<' {
            in_tag = true;
        } else if !skip {
            out.push(ch);
        }
    }
    let out = out
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let mut lines: Vec<&str> = out.lines().map(str::trim_end).collect();
    lines.dedup_by(|a, b| a.trim().is_empty() && b.trim().is_empty());
    lines.join("\n").trim().to_string()
}

pub fn human_size(bytes: i64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 {
        format!("{:.1} MB", b / 1024.0 / 1024.0)
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// Lowercased bare address from "Name <a@b.com>" or "a@b.com".
pub fn bare_email(addr: &str) -> String {
    let a = addr.trim();
    let inner = match (a.rfind('<'), a.rfind('>')) {
        (Some(l), Some(r)) if l < r => &a[l + 1..r],
        _ => a,
    };
    inner.trim().trim_matches('"').to_ascii_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_addresses() {
        assert_eq!(
            split_addresses("a@b.com, \"Last, First\" <c@d.com>; e@f.com"),
            vec!["a@b.com", "\"Last, First\" <c@d.com>", "e@f.com"]
        );
        assert_eq!(split_addresses("Last, First <c@d.com>"), vec!["Last, First <c@d.com>"]);
    }

    #[test]
    fn reply_subjects() {
        assert_eq!(reply_subject("Hi"), "Re: Hi");
        assert_eq!(reply_subject("RE: Hi"), "RE: Hi");
    }

    #[test]
    fn bare_emails() {
        assert_eq!(bare_email("Jane <Jane@X.com>"), "jane@x.com");
        assert_eq!(bare_email(" a@b.c "), "a@b.c");
    }

    #[test]
    fn html_text() {
        assert_eq!(html_to_text("<style>x{}</style><p>Hi &amp; bye</p><p>2</p>"), "Hi & bye\n\n2");
    }

    #[test]
    fn quotes() {
        let q = quote("one\n\n> two", "Jane", 0);
        assert!(q.ends_with("> one\n>\n>> two\n"), "{q}");
    }
}
