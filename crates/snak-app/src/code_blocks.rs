//! Pulls code blocks out of message HTML so they can be drawn as highlighted
//! Markdown code blocks. GPUI Kit's HTML renderer has no code blocks: it
//! collapses the newlines inside `<pre><code>` and draws the code inline.

#[derive(Debug, PartialEq, Eq)]
pub enum Segment {
    Html(String),
    Code { lang: Option<String>, code: String },
}

/// Code blocks drawn per message; the rest of a message past this stays HTML.
/// Each one is its own highlighted view, so a message can't ask for thousands.
const MAX_CODE_BLOCKS: usize = 16;

/// Splits `html` at `<pre>` blocks and Teams' `<codeblock>`. Returns one
/// `Html` segment holding everything when there is no code. One forward pass,
/// since it runs on every render of a message.
pub fn split(html: &str) -> Vec<Segment> {
    // ASCII lowercasing keeps byte offsets, so positions carry over to `html`.
    let lower = html.to_ascii_lowercase();
    let mut segments = Vec::new();
    let mut copied = 0;
    let mut at = 0;
    let mut blocks = 0;
    while blocks < MAX_CODE_BLOCKS {
        let Some(found) = lower[at..].find('<') else {
            break;
        };
        let start = at + found;
        at = start + 1;
        let Some(tag) = ["pre", "codeblock"]
            .into_iter()
            .find(|tag| tag_at(&lower, start + 1, tag))
        else {
            continue;
        };
        let Some(open_end) = lower[start..].find('>').map(|i| start + i + 1) else {
            break;
        };
        let Some(close_start) = closing_tag(&lower, open_end, tag) else {
            break;
        };
        let close_end = lower[close_start..]
            .find('>')
            .map_or(html.len(), |i| close_start + i + 1);

        push_html(&mut segments, &html[copied..start]);
        let inner = &html[open_end..close_start];
        let code = code_text(inner);
        // Teams sometimes leaves an empty `<pre></pre>` behind.
        if !code.trim().is_empty() {
            segments.push(Segment::Code {
                lang: language(&html[start..open_end]).or_else(|| language(inner)),
                code,
            });
            blocks += 1;
        }
        copied = close_end;
        at = close_end;
    }
    push_html(&mut segments, &html[copied..]);
    if segments.is_empty() {
        segments.push(Segment::Html(String::new()));
    }
    segments
}

/// `tag` starts at `at` in `lower` and isn't just a prefix: `<pre` must not
/// match `<prefix>`.
fn tag_at(lower: &str, at: usize, tag: &str) -> bool {
    lower[at..].starts_with(tag)
        && lower[at + tag.len()..]
            .chars()
            .next()
            .is_some_and(|c| c == '>' || c == '/' || c.is_whitespace())
}

/// Start of the `</tag>` that closes a block opened before `from`.
fn closing_tag(lower: &str, from: usize, tag: &str) -> Option<usize> {
    let mut at = from;
    loop {
        let start = at + lower[at..].find("</")?;
        if tag_at(lower, start + 2, tag) {
            return Some(start);
        }
        at = start + 2;
    }
}

/// The source of a fenced Markdown block, with a fence longer than any run of
/// backticks in the code so the code can't close it early.
pub fn fenced(lang: Option<&str>, code: &str) -> String {
    let longest = code.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat(longest.max(2) + 1);
    format!("{fence}{}\n{code}\n{fence}", lang.unwrap_or(""))
}

fn push_html(segments: &mut Vec<Segment>, html: &str) {
    if !html.trim().is_empty() {
        segments.push(Segment::Html(html.to_string()));
    }
}

/// The language from a `class` attribute: `language-rust`, `lang-rust`, or a
/// bare name the way Teams writes it (`class="Python"`).
fn language(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let start = lower.find("class=\"")? + "class=\"".len();
    let end = lower[start..].find('"')? + start;
    let classes: Vec<&str> = lower[start..end].split_whitespace().collect();
    let prefixed = classes.iter().find_map(|class| {
        class
            .strip_prefix("language-")
            .or_else(|| class.strip_prefix("lang-"))
    });
    // A bare name only when it's the sole class; otherwise it's more likely a
    // styling class like `hljs`.
    let name = prefixed.or(match classes.as_slice() {
        [only] => Some(*only),
        _ => None,
    })?;
    let plain = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '#');
    plain.then(|| grammar_name(name)).flatten()
}

/// Language names as people write them, mapped to the grammar names GPUI Kit
/// knows (it already handles `sh`, `js`, `py` and similar). `None` for text.
fn grammar_name(name: &str) -> Option<String> {
    Some(
        match name {
            "c#" => "csharp",
            "c++" => "cpp",
            "shell" | "zsh" => "bash",
            "text" | "plaintext" | "plain" | "none" => return None,
            other => other,
        }
        .to_string(),
    )
}

/// The code inside a block: tags dropped, `<br>` as a newline, entities decoded.
fn code_text(inner: &str) -> String {
    let mut out = String::with_capacity(inner.len());
    let mut rest = inner;
    while let Some(start) = rest.find('<') {
        out.push_str(&rest[..start]);
        let Some(end) = rest[start..].find('>') else {
            out.push_str(&rest[start..]);
            rest = "";
            break;
        };
        let tag = rest[start + 1..start + end].trim().to_ascii_lowercase();
        if tag == "br" || tag.starts_with("br ") || tag.starts_with("br/") {
            out.push('\n');
        }
        rest = &rest[start + end + 1..];
    }
    out.push_str(rest);
    let decoded = html_escape::decode_html_entities(&out).replace('\u{a0}', " ");
    decoded.trim_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_without_code_is_one_segment() {
        assert_eq!(split("<p>hi</p>"), vec![Segment::Html("<p>hi</p>".into())]);
    }

    #[test]
    fn pre_code_keeps_lines_and_language() {
        let html = "<p>Try:</p><pre class=\"language-sh\"><code>for i in 1 2; do\n  echo &quot;$i&quot;\ndone</code></pre><p>after</p>";
        assert_eq!(
            split(html),
            vec![
                Segment::Html("<p>Try:</p>".into()),
                Segment::Code {
                    lang: Some("sh".into()),
                    code: "for i in 1 2; do\n  echo \"$i\"\ndone".into(),
                },
                Segment::Html("<p>after</p>".into()),
            ]
        );
    }

    #[test]
    fn teams_pre_with_highlight_spans() {
        // The shape Teams sends: language on the <pre>, <br> between lines.
        let html = "<pre class=\"language-json\"><code><span class=\"hljs-punctuation\">{</span><br>  <span class=\"hljs-attr\">&quot;a&quot;</span>: 1<br>}</code></pre><pre></pre>";
        assert_eq!(
            split(html),
            vec![Segment::Code {
                lang: Some("json".into()),
                code: "{\n  \"a\": 1\n}".into(),
            }]
        );
    }

    #[test]
    fn teams_codeblock_with_breaks() {
        let html = "<codeblock class=\"Python\"><code>x = 1<br>print(x)</code></codeblock>";
        assert_eq!(
            split(html),
            vec![Segment::Code {
                lang: Some("python".into()),
                code: "x = 1\nprint(x)".into(),
            }]
        );
    }

    #[test]
    fn language_names_map_to_grammars() {
        let block = |class: &str| format!("<pre class=\"{class}\">x</pre>");
        let lang = |class: &str| match split(&block(class)).pop() {
            Some(Segment::Code { lang, .. }) => lang,
            _ => panic!("no code"),
        };
        assert_eq!(lang("C#"), Some("csharp".into()));
        assert_eq!(lang("language-c++"), Some("cpp".into()));
        assert_eq!(lang("plaintext"), None);
        assert_eq!(lang("hljs language-rust"), Some("rust".into()));
    }

    #[test]
    fn a_close_tag_must_be_the_whole_name() {
        assert_eq!(
            split("<pre>a</prefix>b</pre>"),
            vec![Segment::Code {
                lang: None,
                code: "ab".into(),
            }]
        );
    }

    #[test]
    fn code_blocks_per_message_are_capped() {
        let html = "<pre>x</pre>".repeat(MAX_CODE_BLOCKS + 5);
        let segments = split(&html);
        let code = segments
            .iter()
            .filter(|s| matches!(s, Segment::Code { .. }))
            .count();
        assert_eq!(code, MAX_CODE_BLOCKS);
        assert!(matches!(segments.last(), Some(Segment::Html(_))));
    }

    #[test]
    fn splitting_stays_linear() {
        let html = "<pre>a</pre>".repeat(20_000) + &"<p>x</p>".repeat(20_000);
        let started = std::time::Instant::now();
        let _ = split(&html);
        assert!(started.elapsed() < std::time::Duration::from_millis(200));
    }

    #[test]
    fn similar_tag_names_are_not_code() {
        let html = "<prefix>no</prefix>";
        assert_eq!(split(html), vec![Segment::Html(html.into())]);
    }

    #[test]
    fn fence_outlasts_backticks_in_the_code() {
        assert_eq!(fenced(Some("md"), "```x```"), "````md\n```x```\n````");
        assert_eq!(fenced(None, "a"), "```\na\n```");
    }
}
