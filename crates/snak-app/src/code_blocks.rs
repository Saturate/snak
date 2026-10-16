//! Pulls code blocks out of message HTML so they can be drawn as highlighted
//! Markdown code blocks. GPUI Kit's HTML renderer has no code blocks: it
//! collapses the newlines inside `<pre><code>` and draws the code inline.

#[derive(Debug, PartialEq, Eq)]
pub enum Segment {
    Html(String),
    Code { lang: Option<String>, code: String },
}

/// Splits `html` at `<pre>` blocks and Teams' `<codeblock>`. Returns one
/// `Html` segment holding everything when there is no code.
pub fn split(html: &str) -> Vec<Segment> {
    let mut segments = Vec::new();
    let mut rest = html;
    while let Some((start, tag)) = next_block(rest) {
        let open_end = match rest[start..].find('>') {
            Some(i) => start + i + 1,
            None => break,
        };
        let close = format!("</{tag}");
        let Some(close_start) = find_ci(&rest[open_end..], &close).map(|i| open_end + i) else {
            break;
        };
        let close_end = rest[close_start..]
            .find('>')
            .map_or(rest.len(), |i| close_start + i + 1);

        push_html(&mut segments, &rest[..start]);
        let inner = &rest[open_end..close_start];
        let code = code_text(inner);
        // Teams sometimes leaves an empty `<pre></pre>` behind.
        if !code.trim().is_empty() {
            segments.push(Segment::Code {
                lang: language(&rest[start..open_end]).or_else(|| language(inner)),
                code,
            });
        }
        rest = &rest[close_end..];
    }
    push_html(&mut segments, rest);
    if segments.is_empty() {
        segments.push(Segment::Html(String::new()));
    }
    segments
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

fn next_block(html: &str) -> Option<(usize, &'static str)> {
    ["pre", "codeblock"]
        .into_iter()
        .filter_map(|tag| open_tag(html, tag).map(|i| (i, tag)))
        .min_by_key(|(i, _)| *i)
}

/// Position of `<tag` followed by `>` or whitespace, so `<pre` doesn't match `<prefix>`.
fn open_tag(html: &str, tag: &str) -> Option<usize> {
    let needle = format!("<{tag}");
    let mut from = 0;
    while let Some(i) = find_ci(&html[from..], &needle) {
        let at = from + i;
        let after = html[at + needle.len()..].chars().next();
        if matches!(after, Some(c) if c == '>' || c.is_whitespace()) {
            return Some(at);
        }
        from = at + needle.len();
    }
    None
}

fn find_ci(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .to_ascii_lowercase()
        .find(&needle.to_ascii_lowercase())
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
