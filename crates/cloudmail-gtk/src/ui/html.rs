use crate::api::{Address, Message, ThreadDetail};
use crate::theme::Palette;
use crate::util::{escape_html, human_size, long_time};

pub const ATTACHMENT_SCHEME: &str = "cloudmail-attachment:";

pub fn thread(detail: &ThreadDetail, p: &Palette, remote_images: bool) -> String {
    let img_src = if remote_images { "data: https: http:" } else { "data:" };
    let last = detail.messages.len().saturating_sub(1);
    let mut body = String::new();
    let mut blocked_remote = false;
    for (i, m) in detail.messages.iter().enumerate() {
        let open = i == last || (i + 1 == last && detail.messages.len() <= 3);
        body.push_str(&message(m, open, &mut blocked_remote));
    }
    let note = if blocked_remote && !remote_images {
        r#"<div class="note">Remote images are blocked to stop tracking. Press <b>L</b> to load them.</div>"#
    } else {
        ""
    };
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src {img_src}; style-src 'unsafe-inline'; font-src data:">
<style>
html, body {{ background: {bg}; color: {fg}; }}
body {{ margin: 0; padding: 18px 22px 60px; font: 10.5pt "JetBrainsMono Nerd Font", "JetBrains Mono", monospace; }}
a {{ color: {accent}; }}
.note {{ color: {dfg}; font-size: 9pt; margin: 0 0 14px; }}
.msg {{ margin: 0 0 18px; border-bottom: 1px solid {lbg}; padding-bottom: 14px; }}
.msg:last-child {{ border-bottom: none; }}
summary {{ list-style: none; cursor: pointer; }}
summary::-webkit-details-marker {{ display: none; }}
.hdr {{ display: flex; flex-wrap: wrap; justify-content: space-between; column-gap: 12px; row-gap: 2px; margin-bottom: 10px; }}
.hdr > div:first-child {{ flex: 1 1 220px; min-width: 0; overflow-wrap: break-word; }}
.from {{ color: {bfg}; font-weight: bold; }}
.warn {{ color: {red}; font-size: 9pt; border: 1px solid {red}; border-radius: 4px; padding: 0 5px; margin-left: 4px; white-space: nowrap; }}
.addr, .date, .rcpt, .preview {{ color: {dfg}; }}
.date {{ white-space: nowrap; font-size: 9pt; }}
.rcpt {{ font-size: 9pt; margin-top: 2px; }}
.preview {{ overflow: hidden; text-overflow: ellipsis; white-space: nowrap; margin-top: 2px; }}
details[open] .preview {{ display: none; }}
.plain {{ white-space: pre-wrap; overflow-wrap: anywhere; line-height: 1.55; color: {fg}; }}
.quoted summary {{ color: {dfg}; display: inline-block; border: 1px solid {muted}; border-radius: 4px; padding: 0 6px; line-height: 1.2; margin: 4px 0; }}
.quoted .plain {{ color: {dfg}; }}
.paper {{ background: #ffffff; color: #1f2328; border-radius: 8px; padding: 18px 20px; overflow-x: auto;
  font: 11pt -apple-system, "Inter", "Noto Sans", "Helvetica Neue", Arial, sans-serif; }}
.atts {{ margin-top: 10px; }}
.att {{ display: inline-block; border: 1px solid {muted}; border-radius: 6px; padding: 4px 10px; margin: 6px 8px 0 0;
  color: {fg}; text-decoration: none; background: {lbg}; }}
.att:hover {{ border-color: {accent}; color: {bfg}; }}
.att small {{ color: {dfg}; }}
</style></head><body>{note}{body}</body></html>"#,
        bg = p.background,
        fg = p.foreground,
        bfg = p.bright_foreground,
        dfg = p.dark_foreground,
        lbg = p.lighter_background,
        muted = p.muted,
        accent = p.accent,
        red = p.red,
    )
}

fn addr_list(list: &[Address]) -> String {
    list.iter().map(|a| escape_html(&a.formatted())).collect::<Vec<_>>().join(", ")
}

fn message(m: &Message, open: bool, blocked_remote: &mut bool) -> String {
    let mut rcpt = String::new();
    if !m.to.is_empty() {
        rcpt.push_str(&format!("to {}", addr_list(&m.to)));
    }
    if !m.cc.is_empty() {
        rcpt.push_str(&format!(" · cc {}", addr_list(&m.cc)));
    }
    let preview = m
        .text
        .as_deref()
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let preview: String = preview.chars().take(160).collect();
    let header = format!(
        r#"<div class="hdr"><div><span class="from">{name}</span> <span class="addr">&lt;{email}&gt;</span>{warn}<div class="rcpt">{rcpt}</div><div class="preview">{preview}</div></div><div class="date">{date}</div></div>"#,
        name = escape_html(&m.from.display()),
        email = escape_html(&m.from.email),
        warn = if m.dmarc_failed() {
            r#" <span class="warn" title="The sender's domain did not authorize this message (DMARC fail); the From address may be forged.">⚠ sender not verified</span>"#
        } else {
            ""
        },
        date = escape_html(&long_time(m.date)),
        preview = escape_html(&preview),
    );

    let content = match (&m.html, &m.text) {
        (Some(html), _) if !html.trim().is_empty() => {
            if has_remote_images(html) {
                *blocked_remote = true;
            }
            format!(
                r#"<div class="paper"><template shadowrootmode="open"><style>:host {{ display: block; }} img {{ max-width: 100%; height: auto; }} table {{ max-width: 100%; }}</style>{}</template></div>"#,
                sanitize(html)
            )
        }
        (_, Some(text)) => plain(text),
        _ => r#"<div class="plain addr">(empty message)</div>"#.to_string(),
    };

    let atts: Vec<String> = m
        .attachments
        .iter()
        .filter(|a| !a.inline)
        .map(|a| {
            format!(
                r#"<a class="att" href="{ATTACHMENT_SCHEME}{id}">&#xf0c6; {name} <small>{size}</small></a>"#,
                id = escape_html(&a.id),
                name = escape_html(&a.filename),
                size = human_size(a.size),
            )
        })
        .collect();
    let atts = if atts.is_empty() { String::new() } else { format!(r#"<div class="atts">{}</div>"#, atts.join("")) };

    format!(
        r#"<details class="msg"{open}><summary>{header}</summary>{content}{atts}</details>"#,
        open = if open { " open" } else { "" },
    )
}

fn has_remote_images(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    ["src=\"http", "src='http", "src=http", "url(http", "url('http", "url(\"http", "background=\"http"]
        .iter()
        .any(|p| lower.contains(p))
}

/// JavaScript is disabled, so this only needs to keep the markup from escaping
/// its shadow root and from loading anything behind the CSP's back.
fn sanitize(html: &str) -> String {
    let mut out = strip_element(html, "script");
    out = strip_element(&out, "iframe");
    out = strip_element(&out, "object");
    replace_ci(&out, "</template", "&lt;/template")
}

fn strip_element(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut pos = 0;
    while let Some(start) = lower[pos..].find(&open).map(|i| i + pos) {
        out.push_str(&html[pos..start]);
        match lower[start..].find(&close) {
            Some(end) => pos = start + end + close.len(),
            None => {
                pos = html.len();
                break;
            }
        }
    }
    out.push_str(&html[pos.min(html.len())..]);
    out
}

fn replace_ci(haystack: &str, needle: &str, with: &str) -> String {
    let lower = haystack.to_ascii_lowercase();
    let mut out = String::with_capacity(haystack.len());
    let mut pos = 0;
    while let Some(i) = lower[pos..].find(needle).map(|i| i + pos) {
        out.push_str(&haystack[pos..i]);
        out.push_str(with);
        pos = i + needle.len();
    }
    out.push_str(&haystack[pos..]);
    out
}

fn plain(text: &str) -> String {
    let mut out = String::new();
    let mut quoted: Vec<&str> = Vec::new();
    let flush = |out: &mut String, quoted: &mut Vec<&str>| {
        if quoted.is_empty() {
            return;
        }
        out.push_str(&format!(
            r#"<details class="quoted"><summary>•••</summary><div class="plain">{}</div></details>"#,
            linkify(&escape_html(&quoted.join("\n")))
        ));
        quoted.clear();
    };
    let mut normal: Vec<&str> = Vec::new();
    let lines: Vec<&str> = text.trim_end().lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let next_quoted = lines.get(i + 1).is_some_and(|l| l.starts_with('>'));
        let is_attribution = line.trim_end().ends_with("wrote:") && next_quoted;
        if line.starts_with('>') || (is_attribution && quoted.is_empty()) {
            if !normal.is_empty() {
                out.push_str(&format!(r#"<div class="plain">{}</div>"#, linkify(&escape_html(&normal.join("\n")))));
                normal.clear();
            }
            quoted.push(line);
        } else {
            flush(&mut out, &mut quoted);
            normal.push(line);
        }
    }
    flush(&mut out, &mut quoted);
    if !normal.is_empty() {
        out.push_str(&format!(r#"<div class="plain">{}</div>"#, linkify(&escape_html(&normal.join("\n")))));
    }
    out
}

/// Wraps http(s) URLs in already-escaped text with anchors.
fn linkify(escaped: &str) -> String {
    let mut out = String::with_capacity(escaped.len());
    let mut rest = escaped;
    loop {
        let next = ["https://", "http://"].iter().filter_map(|p| rest.find(p)).min();
        let Some(start) = next else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..start]);
        let tail = &rest[start..];
        let mut end = tail
            .find(|c: char| c.is_whitespace() || c == '<' || c == '"')
            .unwrap_or(tail.len());
        if let Some(amp) = tail[..end].find("&gt;") {
            end = amp;
        }
        let url = tail[..end].trim_end_matches(['.', ',', ')', ';', ':', '!', '?']);
        out.push_str(&format!(r#"<a href="{url}">{url}</a>"#));
        rest = &tail[url.len()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn warns_on_dmarc_fail() {
        let mut m = Message { text: Some("hi".into()), ..Default::default() };
        let mut blocked = false;
        assert!(!message(&m, true, &mut blocked).contains("not verified"));
        m.auth = Some(cloudmail_api::MessageAuth { dmarc: Some("fail".into()), ..Default::default() });
        assert!(message(&m, true, &mut blocked).contains("sender not verified"));
    }

    #[test]
    fn strips_scripts_and_template_breakouts() {
        let s = sanitize("<p>a</p><SCRIPT>x()</script><b>b</b></TEMPLATE>");
        assert_eq!(s, "<p>a</p><b>b</b>&lt;/template>");
    }

    #[test]
    fn links_urls() {
        assert_eq!(
            linkify("see https://x.io/a?b=1&amp;c=2."),
            r#"see <a href="https://x.io/a?b=1&amp;c=2">https://x.io/a?b=1&amp;c=2</a>."#
        );
    }

    #[test]
    fn collapses_quotes() {
        let html = plain("Thanks!\n\nOn Mon, Bob wrote:\n> hi\n> there");
        assert!(html.contains(r#"<details class="quoted">"#));
        assert!(html.starts_with(r#"<div class="plain">Thanks!"#));
    }
}
