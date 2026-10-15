// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

/// Flattens Teams' message HTML to plain text with line breaks kept.
pub fn to_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;

    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        let name = tag
            .trim_start_matches('/')
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or("");
        match name {
            "br" => out.push('\n'),
            "p" | "div" | "li" | "tr" | "blockquote" | "pre" if tag.starts_with('/') => {
                out.push('\n')
            }
            "li" => out.push_str("• "),
            "img" if tag.contains("itemtype=\"http://schema.skype.com/emoji\"") => {
                if let Some(alt) = attribute(&rest[start..start + end], "alt") {
                    out.push_str(&alt);
                }
            }
            _ => {}
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);

    let decoded = html_escape::decode_html_entities(&out).replace('\u{a0}', " ");
    collapse_blank_lines(decoded.trim())
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let needle = format!("{name}=\"");
    let start = tag.find(&needle)? + needle.len();
    let end = tag[start..].find('"')?;
    Some(tag[start..start + end].to_string())
}

fn collapse_blank_lines(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut blank_run = 0;
    for line in text.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run > 1 {
                continue;
            }
        } else {
            blank_run = 0;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line.trim_end());
    }
    out
}

/// Plain text from the compose box to the HTML Teams expects.
pub fn from_text(text: &str) -> String {
    let escaped = html_escape::encode_text(text.trim());
    format!("<p>{}</p>", escaped.replace('\n', "<br>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paragraphs_and_breaks_become_newlines() {
        assert_eq!(to_text("<p>Hi</p><p>there<br>you</p>"), "Hi\nthere\nyou");
    }

    #[test]
    fn entities_are_decoded() {
        assert_eq!(to_text("<p>a &amp; b&nbsp;&lt;c&gt;</p>"), "a & b <c>");
    }

    #[test]
    fn mentions_keep_their_text() {
        let html = r#"<p>hey <span itemtype="http://schema.skype.com/Mention" itemid="0">Allan</span></p>"#;
        assert_eq!(to_text(html), "hey Allan");
    }

    #[test]
    fn emoji_images_use_alt_text() {
        let html = r#"<p>nice <img itemtype="http://schema.skype.com/Emoji" alt="👍" src="x"></p>"#;
        assert_eq!(to_text(html), "nice 👍");
    }

    #[test]
    fn outgoing_text_is_escaped() {
        assert_eq!(from_text("a < b\nc"), "<p>a &lt; b<br>c</p>");
    }
}
