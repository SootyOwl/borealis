use std::path::PathBuf;
use std::sync::LazyLock;

use regex::Regex;
use tracing::warn;

use super::event::{Directive, FileKind};

/// Fenced code block pattern: matches ``` ... ``` (with optional language tag).
/// Uses `(?s)` (dot-matches-newline) so the block can span multiple lines.
static FENCED_CODE_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?s)```[^\n]*\n.*?```").expect("fenced code block regex"));

/// Matches `<actions>...</actions>` blocks (case-insensitive tag name, dot-matches-newline).
static ACTIONS_BLOCK: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?si)<actions>(.*?)</actions>").expect("actions block regex"));

// Individual directive tag patterns

static TAG_NOREPLY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)<noreply\s*/>").expect("noreply regex"));

static TAG_REACT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<react\s+(?P<attrs>[^>]*)/?>"#).expect("react regex"));

static TAG_VOICE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?si)<voice>(.*?)</voice>").expect("voice regex"));

static TAG_SENDFILE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)<sendfile\s+(?P<attrs>[^>]*)/?>"#).expect("sendfile regex"));

static TAG_SEND: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?si)<send\s+(?P<attrs>[^>]*)>(?P<body>.*?)</send>"#).expect("send regex")
});

/// Extracts the value of a named XML attribute from an attribute string.
///
/// Handles both single- and double-quoted values.
fn attr(attrs: &str, name: &str) -> Option<String> {
    let pattern = format!(r#"(?i){name}\s*=\s*(?:"([^"]*)"|'([^']*)')"#);
    let re = Regex::new(&pattern).ok()?;
    let caps = re.captures(attrs)?;
    caps.get(1)
        .or_else(|| caps.get(2))
        .map(|m| m.as_str().to_owned())
}

/// Parse all directives from an LLM response string.
///
/// **Two-phase strategy** (per design spec):
/// 1. Strip fenced code blocks so that XML examples in markdown are ignored.
/// 2. Regex-scan the remaining text for `<actions>...</actions>` blocks.
/// 3. Within each block, match known directive tags and collect them.
///
/// Malformed XML within an action block logs a warning and is skipped.
/// Unrecognised tags are logged and skipped.
pub fn parse_directives(response: &str) -> Vec<Directive> {
    // Phase 1: strip fenced code blocks.
    let stripped = FENCED_CODE_BLOCK.replace_all(response, "");

    // Phase 2: find all <actions> blocks.
    let mut directives = Vec::new();

    for block_match in ACTIONS_BLOCK.captures_iter(&stripped) {
        let inner = &block_match[1];
        parse_inner_directives(inner, &mut directives);
    }

    directives
}

/// Strips directives from an LLM response, returning only the display text.
///
/// Removes fenced code blocks' *content* is preserved (they are real content);
/// only `<actions>...</actions>` blocks are removed from the output.
pub fn strip_directives(response: &str) -> String {
    ACTIONS_BLOCK.replace_all(response, "").trim().to_owned()
}

/// Parse individual directive tags from the inner content of an `<actions>` block.
fn parse_inner_directives(inner: &str, out: &mut Vec<Directive>) {
    // Track which byte offsets have been claimed by a matched tag so we can
    // detect unrecognised leftovers.
    let mut claimed: Vec<(usize, usize)> = Vec::new();

    // NoReply
    for m in TAG_NOREPLY.find_iter(inner) {
        out.push(Directive::NoReply);
        claimed.push((m.start(), m.end()));
    }

    // React
    for caps in TAG_REACT.captures_iter(inner) {
        let attrs_str = &caps["attrs"];
        match attr(attrs_str, "emoji") {
            Some(emoji) => {
                let message_id = attr(attrs_str, "message_id");
                out.push(Directive::React { emoji, message_id });
                claimed.push((caps.get(0).unwrap().start(), caps.get(0).unwrap().end()));
            }
            None => {
                warn!(
                    tag = "react",
                    attrs = attrs_str,
                    "missing required 'emoji' attribute — skipping"
                );
            }
        }
    }

    // Voice
    for caps in TAG_VOICE.captures_iter(inner) {
        let text = caps[1].trim().to_owned();
        if text.is_empty() {
            warn!(tag = "voice", "empty voice text — skipping");
        } else {
            out.push(Directive::Voice { text });
            claimed.push((caps.get(0).unwrap().start(), caps.get(0).unwrap().end()));
        }
    }

    // SendFile
    for caps in TAG_SENDFILE.captures_iter(inner) {
        let attrs_str = &caps["attrs"];
        let path = attr(attrs_str, "path");
        let kind = attr(attrs_str, "kind").and_then(|k| FileKind::from_str_opt(&k));
        match (path, kind) {
            (Some(p), Some(k)) => {
                out.push(Directive::SendFile {
                    path: PathBuf::from(p),
                    kind: k,
                });
                claimed.push((caps.get(0).unwrap().start(), caps.get(0).unwrap().end()));
            }
            (None, _) => {
                warn!(
                    tag = "sendfile",
                    attrs = attrs_str,
                    "missing required 'path' attribute — skipping"
                );
            }
            (_, None) => {
                warn!(
                    tag = "sendfile",
                    attrs = attrs_str,
                    "missing or unrecognised 'kind' attribute — skipping"
                );
            }
        }
    }

    // Send
    for caps in TAG_SEND.captures_iter(inner) {
        let attrs_str = &caps["attrs"];
        let body = caps["body"].trim().to_owned();
        let channel = attr(attrs_str, "channel");
        let chat = attr(attrs_str, "chat");
        match (channel, chat) {
            (Some(ch), Some(ct)) => {
                if body.is_empty() {
                    warn!(tag = "send", "empty send body — skipping");
                } else {
                    out.push(Directive::Send {
                        channel: ch,
                        chat: ct,
                        text: body,
                    });
                    claimed.push((caps.get(0).unwrap().start(), caps.get(0).unwrap().end()));
                }
            }
            _ => {
                warn!(
                    tag = "send",
                    attrs = attrs_str,
                    "missing required 'channel' and/or 'chat' attributes — skipping"
                );
            }
        }
    }

    // Warn about unrecognised tags
    let unknown_tag = Regex::new(r"(?i)<([a-z_][a-z0-9_]*)\b[^>]*/?>").expect("unknown tag regex");
    for m in unknown_tag.find_iter(inner) {
        let is_claimed = claimed
            .iter()
            .any(|&(start, end)| m.start() >= start && m.end() <= end);
        if !is_claimed {
            let tag_text = m.as_str();
            if !tag_text.starts_with("</") {
                warn!(tag = tag_text, "unsupported directive tag — skipping");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_noreply() {
        let input = "Some text <actions><noreply/></actions>";
        let directives = parse_directives(input);
        assert_eq!(directives, vec![Directive::NoReply]);
    }

    #[test]
    fn parse_noreply_with_space() {
        let input = "<actions><noreply /></actions>";
        let directives = parse_directives(input);
        assert_eq!(directives, vec![Directive::NoReply]);
    }

    #[test]
    fn parse_react_basic() {
        let input = r#"<actions><react emoji="thumbsup"/></actions>"#;
        let directives = parse_directives(input);
        assert_eq!(
            directives,
            vec![Directive::React {
                emoji: "thumbsup".into(),
                message_id: None,
            }]
        );
    }

    #[test]
    fn parse_react_with_message_id() {
        let input = r#"<actions><react emoji="👍" message_id="12345"/></actions>"#;
        let directives = parse_directives(input);
        assert_eq!(
            directives,
            vec![Directive::React {
                emoji: "👍".into(),
                message_id: Some("12345".into()),
            }]
        );
    }

    #[test]
    fn parse_voice() {
        let input = "<actions><voice>Hello world!</voice></actions>";
        let directives = parse_directives(input);
        assert_eq!(
            directives,
            vec![Directive::Voice {
                text: "Hello world!".into(),
            }]
        );
    }

    #[test]
    fn parse_sendfile() {
        let input = r#"<actions><sendfile path="/tmp/cat.png" kind="image"/></actions>"#;
        let directives = parse_directives(input);
        assert_eq!(
            directives,
            vec![Directive::SendFile {
                path: PathBuf::from("/tmp/cat.png"),
                kind: FileKind::Image,
            }]
        );
    }

    #[test]
    fn parse_send() {
        let input =
            r#"<actions><send channel="discord" chat="general">hello everyone</send></actions>"#;
        let directives = parse_directives(input);
        assert_eq!(
            directives,
            vec![Directive::Send {
                channel: "discord".into(),
                chat: "general".into(),
                text: "hello everyone".into(),
            }]
        );
    }

    #[test]
    fn code_blocks_excluded() {
        let input = r#"Here is an example:
```xml
<actions><react emoji="fire"/></actions>
```
And then the real one:
<actions><noreply/></actions>"#;
        let directives = parse_directives(input);
        assert_eq!(directives, vec![Directive::NoReply]);
    }

    #[test]
    fn multiple_actions_blocks() {
        let input =
            r#"First <actions><noreply/></actions> then <actions><react emoji="👍"/></actions>"#;
        let directives = parse_directives(input);
        assert_eq!(
            directives,
            vec![
                Directive::NoReply,
                Directive::React {
                    emoji: "👍".into(),
                    message_id: None,
                },
            ]
        );
    }

    #[test]
    fn multiple_directives_in_one_block() {
        let input = r#"<actions><react emoji="heart"/><voice>Love you all!</voice></actions>"#;
        let directives = parse_directives(input);
        assert_eq!(
            directives,
            vec![
                Directive::React {
                    emoji: "heart".into(),
                    message_id: None,
                },
                Directive::Voice {
                    text: "Love you all!".into(),
                },
            ]
        );
    }

    #[test]
    fn malformed_react_missing_emoji_is_skipped() {
        let input = r#"<actions><react/></actions>"#;
        let directives = parse_directives(input);
        assert!(directives.is_empty());
    }

    #[test]
    fn empty_voice_is_skipped() {
        let input = "<actions><voice>  </voice></actions>";
        let directives = parse_directives(input);
        assert!(directives.is_empty());
    }

    #[test]
    fn strip_directives_removes_actions_blocks() {
        let input = "Hello! <actions><noreply/></actions> How are you?";
        let result = strip_directives(input);
        assert_eq!(result, "Hello!  How are you?");
    }

    #[test]
    fn case_insensitive_tags() {
        let input = r#"<ACTIONS><React emoji="wave"/></ACTIONS>"#;
        let directives = parse_directives(input);
        assert_eq!(
            directives,
            vec![Directive::React {
                emoji: "wave".into(),
                message_id: None,
            }]
        );
    }

    #[test]
    fn no_actions_block_returns_empty() {
        let input = "Just a normal response with no directives.";
        let directives = parse_directives(input);
        assert!(directives.is_empty());
    }
}
