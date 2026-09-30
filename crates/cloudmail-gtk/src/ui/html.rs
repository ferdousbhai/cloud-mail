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
    let subject = if detail.thread.subject.trim().is_empty() { "(no subject)" } else { detail.thread.subject.as_str() };
    let heading = format!(r#"<h1 class="subject">{}</h1>"#, escape_html(subject));
    let note = if blocked_remote && !remote_images {
        r#"<div class="note">Remote images are blocked to stop tracking. Press <b>L</b> to load them.</div>"#
    } else {
        ""
    };
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="default-src 'none'; img-src {img_src}; style-src 'unsafe-inline'; font-src 'none'">
<style>
html, body {{ background: {bg}; color: {fg}; }}
body {{ margin: 0; padding: 18px 22px 60px; font: 10.5pt "JetBrainsMono Nerd Font", "JetBrains Mono", monospace; }}
a {{ color: {accent}; }}
.subject {{ color: {bfg}; font-size: 13pt; font-weight: bold; margin: 0 0 16px; overflow-wrap: anywhere; }}
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
/* The frame is outside the message's reach (it can style its own host, not this parent):
   paint containment makes it the containing block for fixed-position content and clips it,
   so a message can't draw over the headers or the sender warning. Its white background and
   border mark where the message starts even if the message makes its own card transparent. */
.frame {{ position: relative; contain: paint; isolation: isolate; border-radius: 8px;
  background: #ffffff; border: 1px solid {muted}; }}
.paper {{ background: #ffffff; color: #1f2328; border-radius: 8px; padding: 18px 20px; overflow-x: auto;
  font: 11pt -apple-system, "Inter", "Noto Sans", "Helvetica Neue", Arial, sans-serif; }}
.atts {{ margin-top: 10px; }}
.att {{ display: inline-block; border: 1px solid {muted}; border-radius: 6px; padding: 4px 10px; margin: 6px 8px 0 0;
  color: {fg}; text-decoration: none; background: {lbg}; }}
.att:hover {{ border-color: {accent}; color: {bfg}; }}
.att small {{ color: {dfg}; }}
</style></head><body>{heading}{note}{body}</body></html>"#,
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
    list.iter().map(|a| escape_html(&a.display())).collect::<Vec<_>>().join(", ")
}

fn addr_list_full(list: &[Address]) -> String {
    list.iter().map(|a| a.formatted()).collect::<Vec<_>>().join(", ")
}

fn message(m: &Message, open: bool, blocked_remote: &mut bool) -> String {
    let mut rcpt = String::new();
    let mut rcpt_full = String::new();
    if !m.to.is_empty() {
        rcpt.push_str(&format!("to {}", addr_list(&m.to)));
        rcpt_full.push_str(&format!("To: {}", addr_list_full(&m.to)));
    }
    if !m.cc.is_empty() {
        rcpt.push_str(&format!(" · cc {}", addr_list(&m.cc)));
        rcpt_full.push_str(&format!("\nCc: {}", addr_list_full(&m.cc)));
    }
    let preview = m
        .text
        .as_deref()
        .map(|t| t.split_whitespace().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let preview: String = preview.chars().take(160).collect();
    let header = format!(
        r#"<div class="hdr"><div><span class="from">{name}</span> <span class="addr">&lt;{email}&gt;</span>{warn}<div class="rcpt" title="{rcpt_full}">{rcpt}</div><div class="preview">{preview}</div></div><div class="date">{date}</div></div>"#,
        name = escape_html(&m.from.display()),
        email = escape_html(&m.from.email),
        warn = if m.dmarc_failed() {
            r#" <span class="warn" title="The sender's domain didn't authenticate this message (no DMARC pass, and no aligned DKIM or SPF): the From address may be forged.">⚠ sender not verified</span>"#
        } else {
            ""
        },
        date = escape_html(&long_time(m.date)),
        preview = escape_html(&preview),
        rcpt_full = escape_html(&rcpt_full),
    );

    // The white card is for designed mail (newsletters, receipts). A personal note that
    // also carries a trivial HTML part, including everything sent from here, reads
    // better as text in the theme's colours.
    let text = m.text.as_deref().filter(|t| !t.trim().is_empty());
    let use_html = match (&m.html, text) {
        (Some(html), Some(text)) => !html.trim().is_empty() && !m.outgoing && (is_designed(html) || is_stub(text, html)),
        (Some(html), None) => !html.trim().is_empty(),
        _ => false,
    };
    let content = match (&m.html, &m.text) {
        (Some(html), _) if use_html => {
            if has_remote_images(html) {
                *blocked_remote = true;
            }
            format!(
                r#"<div class="frame"><div class="paper"><template shadowrootmode="open"><style>:host {{ display: block; }} img {{ max-width: 100%; height: auto; }} table {{ max-width: 100%; }}</style>{}</template></div></div>"#,
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

/// A text part that only points elsewhere ("view this email in your browser") while the
/// HTML carries the message.
fn is_stub(text: &str, html: &str) -> bool {
    let text_len = text.trim().chars().count();
    let html_len = cloudmail_api::text::html_to_text(html).trim().chars().count();
    html_len > 200 && html_len > text_len * 2
}

/// Whether an HTML part has real layout or imagery, as opposed to text in a few divs.
fn is_designed(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    ["<table", "<img", "<style", "background", "bgcolor", "<h1", "<h2", "<h3", "<hr", "<font", "<button", "<svg"]
        .iter()
        .any(|tag| lower.contains(tag))
}

fn has_remote_images(html: &str) -> bool {
    let lower = html.to_ascii_lowercase();
    ["src=\"http", "src='http", "src=http", "url(http", "url('http", "url(\"http", "background=\"http"]
        .iter()
        .any(|p| lower.contains(p))
}

/// JavaScript is disabled, so this only needs to keep the markup from escaping
/// its shadow root and from loading anything behind the CSP's back.
/// An allowlist sanitiser (ammonia) rather than parse-and-reserialise: re-serialising lets a
/// message exploit differences between two HTML parsers (MathML/SVG namespace tricks) to leave
/// its card. Only HTML elements survive; SVG and MathML are dropped, and <style> is kept so
/// designed mail still looks designed. Remote loads are still gated by the page's CSP.
fn sanitize(html: &str) -> String {
    static CLEANER: std::sync::OnceLock<ammonia::Builder<'static>> = std::sync::OnceLock::new();
    CLEANER
        .get_or_init(|| {
            let mut b = ammonia::Builder::default();
            b.rm_clean_content_tags(&["style"])
                .add_tags(&["style", "font", "big", "tfoot", "label"])
                .add_generic_attributes(&[
                    "style", "class", "id", "align", "valign", "bgcolor", "background", "width", "height",
                    "border", "cellpadding", "cellspacing", "color", "face", "size", "dir", "role",
                ])
                .url_schemes(["http", "https", "mailto", "data"].into_iter().collect());
            b
        })
        .clean(html)
        .to_string()
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

    /// What a sanitised body must never contain: anything that could close the template, card or
    /// message it is placed in, or run code.
    fn assert_contained(input: &str) {
        let out = sanitize(input);
        // Text inside <style> is raw CSS that only </style> can end, so it is inert here.
        let mut lower = out.to_ascii_lowercase();
        while let Some(start) = lower.find("<style>") {
            let end = lower[start..].find("</style>").map(|e| start + e + 8).unwrap_or(lower.len());
            lower.replace_range(start..end, "");
        }
        for bad in ["</template", "</details", "</div></div>", "<script", "<iframe", "<object", "<embed", "<math", "<svg", "<meta", "<base"] {
            assert!(!lower.contains(bad), "{bad} survived in {out}");
        }
        assert_eq!(sanitize(&out), out, "not stable under re-sanitising: {input}");
    }

    #[test]
    fn strips_scripts_and_template_breakouts() {
        let s = sanitize("<p>a</p><SCRIPT>x()</script><b>b</b></TEMPLATE>");
        assert_eq!(s, "<p>a</p><b>b</b>");
        assert_contained("<p>&lt;/template&gt;</p>");
        assert_contained(r#"<iframe src="x"></iframe><embed src="y"><meta http-equiv="refresh" content="0;url=z">ok"#);
        assert_contained("<p>a</p><template><p>x</p></template><p>b</p>");
        // Markup-looking text inside <style> is CSS, not an element to cut at.
        let css = sanitize("<style>/* <script */ p{}</style><p>after</p>");
        assert!(css.contains("<p>after</p>"), "{css}");
        assert_contained("<style>/* <script */ p{}</style><p>after</p>");
    }

    #[test]
    fn namespace_confusion_cannot_escape_the_card() {
        // MathML/SVG parsing differences (mXSS): must not leave </template>, </details> or a live <style>.
        let payload = r#"<table><tr><td>x</td></tr></table><math><mtext><table><mglyph><xmp></mglyph></math></template></div></div></details><style>.warn{display:none!important}</style></xmp></table></mtext></math>"#;
        assert_contained(payload);
        // Its <style> must come out as inert text, never as a live element.
        assert!(!sanitize(payload).to_ascii_lowercase().contains("<style"), "{}", sanitize(payload));
        for raw in ["style", "noembed", "noframes", "xmp", "noscript"] {
            let smuggled = format!(r#"<math><mtext><table><mglyph><{raw}></mglyph></math></template></details><style>.warn{{display:none}}</style></{raw}>"#);
            assert_contained(&smuggled);
            assert!(!sanitize(&smuggled).to_ascii_lowercase().contains("<style"), "{raw}: {}", sanitize(&smuggled));
            assert_contained(&format!(r#"<svg><foreignObject><{raw}></svg></template></details></{raw}></foreignObject></svg>"#));
        }
        assert_contained("<svg><style><img src=x onerror=alert(1)></style></svg>");
    }

    #[test]
    fn designed_mail_gets_the_card_and_notes_do_not() {
        assert!(is_designed(r#"<table><tr><td><img src="x"></td></tr></table>"#));
        assert!(is_designed("<style>.btn{}</style><p>hi</p>"));
        assert!(!is_designed(r#"<div style="font-family: sans-serif; white-space: pre-wrap;">Shipped!</div>"#));
        assert!(!is_designed(r#"<div dir="ltr">7 works!</div><blockquote class="gmail_quote">x</blockquote>"#));
    }

    #[test]
    fn a_stub_text_part_does_not_hide_the_html() {
        let body = "<p>".to_string() + &"Here is the whole newsletter, in plain paragraphs. ".repeat(10) + "</p>";
        let m = Message { text: Some("View this email in your browser.".into()), html: Some(body), ..Default::default() };
        assert!(message(&m, true, &mut false).contains("paper"));
        let m = Message { text: Some("Thanks!".into()), html: Some("<div>Thanks!</div>".into()), ..Default::default() };
        assert!(!message(&m, true, &mut false).contains("paper"));
    }

    #[test]
    fn unclosed_markup_is_balanced() {
        // Whatever a message leaves open is closed before the next message starts.
        for open in ["<p>hi<!-- never closed <textarea>", "<textarea>", "<title>", "<style>", "<xmp>", "<plaintext>", "<a href=\"x", "<table><tr><td>"] {
            assert_contained(&format!("<p>hi</p>{open}rest"));
        }
    }

    #[test]
    fn designed_mail_keeps_its_styling() {
        let out = sanitize(r##"<style>.btn{color:red}</style><table bgcolor="#fff" cellpadding="4"><tr><td align="center" style="padding:8px"><a class="btn" href="https://x.example">Go</a><img src="data:image/png;base64,AA" width="1"></td></tr></table>"##);
        for keep in ["<style>.btn{color:red}</style>", r##"bgcolor="#fff""##, "cellpadding", r#"style="padding:8px""#, r#"class="btn""#, "https://x.example", "data:image/png"] {
            assert!(out.contains(keep), "{keep} missing from {out}");
        }
        assert!(!sanitize(r#"<a href="javascript:alert(1)">x</a>"#).contains("javascript"));
    }

    #[test]
    fn sent_mail_reads_as_text_even_with_an_html_part() {
        let m = Message {
            outgoing: true,
            text: Some("Shipped!".into()),
            html: Some("<table><tr><td>Shipped!</td></tr></table>".into()),
            ..Default::default()
        };
        let out = message(&m, true, &mut false);
        assert!(!out.contains("paper") && out.contains("Shipped!"));
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
