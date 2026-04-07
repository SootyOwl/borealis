mod store;

use std::sync::{Arc, Mutex};

use crate::config::Settings;

pub use store::{Link, Memory, MemoryError, MemoryResult, Note, NoteSummary, PaginatedNotes, SqliteMemory, TagCount};
pub(crate) use store::normalize_tag;

/// Maximum length (in bytes) of a query string accepted by `search_notes`
/// call sites. Shared between the MCP tool layer (`memory_search`) and the
/// pipeline's automatic `retrieve_memories` path so both apply the same cap.
pub const MEMORY_SEARCH_MAX_QUERY_LEN: usize = 1024;

/// FTS5 operator keywords that are rejected as standalone tokens during
/// query sanitization. Case-insensitive match.
const FTS_OPERATOR_KEYWORDS: &[&str] = &["AND", "OR", "NOT", "NEAR"];

/// FTS5 metacharacters replaced with space during query sanitization. The
/// full list of characters with syntactic meaning in FTS5: `"` (phrase),
/// `*` (prefix), `:` (column filter), `^` (initial-token), `(`/`)` (group),
/// `-`/`+` (exclude/require), `~` (NOT in some configs).
const FTS_METACHARS: &[char] = &['"', '*', ':', '^', '(', ')', '-', '+', '~'];

/// Sanitize a free-form string for safe use as an FTS5 `MATCH` query.
///
/// - Replaces FTS5 metacharacters ([`FTS_METACHARS`]) with whitespace.
/// - Drops bare operator tokens ([`FTS_OPERATOR_KEYWORDS`]) so a user asking
///   "what is AND OR" doesn't accidentally execute a boolean query.
/// - Collapses whitespace and joins surviving tokens with a single space,
///   which FTS5 treats as implicit AND across the indexed columns.
/// - Truncates the result to [`MEMORY_SEARCH_MAX_QUERY_LEN`] bytes on a
///   character boundary, so a very long input doesn't stall the FTS5
///   tokenizer.
///
/// Returns an empty string if nothing salvageable remains; callers should
/// treat that as "no query" (e.g. return `Ok(vec![])` without hitting the
/// store).
///
/// This is the sanitizer used by `retrieve_memories` in the conversation
/// pipeline, where the "query" is raw user-message text that must never be
/// interpreted as FTS5 syntax but should still benefit from tokenization
/// and stemming.
pub fn sanitize_for_fts(input: &str) -> String {
    // Truncate first (on a char boundary) so sanitization work is bounded.
    let truncated = if input.len() > MEMORY_SEARCH_MAX_QUERY_LEN {
        let mut cut = MEMORY_SEARCH_MAX_QUERY_LEN;
        while cut > 0 && !input.is_char_boundary(cut) {
            cut -= 1;
        }
        &input[..cut]
    } else {
        input
    };

    // Replace metacharacters with space, then split on whitespace and filter
    // out bare operator keywords.
    let cleaned: String = truncated
        .chars()
        .map(|c| if FTS_METACHARS.contains(&c) { ' ' } else { c })
        .collect();

    cleaned
        .split_whitespace()
        .filter(|token| {
            !FTS_OPERATOR_KEYWORDS
                .iter()
                .any(|op| op.eq_ignore_ascii_case(token))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod sanitize_tests {
    use super::*;

    #[test]
    fn passes_through_normal_text() {
        assert_eq!(
            sanitize_for_fts("what did we cook last week"),
            "what did we cook last week"
        );
    }

    #[test]
    fn collapses_whitespace() {
        assert_eq!(
            sanitize_for_fts("hello   world\n\tagain"),
            "hello world again"
        );
    }

    #[test]
    fn empty_input_returns_empty() {
        assert_eq!(sanitize_for_fts(""), "");
        assert_eq!(sanitize_for_fts("   "), "");
        assert_eq!(sanitize_for_fts("\n\t\n"), "");
    }

    #[test]
    fn strips_fts_metacharacters() {
        // Each metacharacter should become a space (and then collapse).
        assert_eq!(sanitize_for_fts(r#"foo"bar"#), "foo bar");
        assert_eq!(sanitize_for_fts("foo*bar"), "foo bar");
        assert_eq!(sanitize_for_fts("title:foo"), "title foo");
        assert_eq!(sanitize_for_fts("^foo"), "foo");
        assert_eq!(sanitize_for_fts("(foo)"), "foo");
        assert_eq!(sanitize_for_fts("foo-bar"), "foo bar");
        assert_eq!(sanitize_for_fts("foo+bar"), "foo bar");
        assert_eq!(sanitize_for_fts("~foo"), "foo");
    }

    #[test]
    fn drops_bare_operator_keywords() {
        assert_eq!(sanitize_for_fts("foo AND bar"), "foo bar");
        assert_eq!(sanitize_for_fts("foo OR bar"), "foo bar");
        assert_eq!(sanitize_for_fts("foo NOT bar"), "foo bar");
        assert_eq!(sanitize_for_fts("foo NEAR bar"), "foo bar");
        // Case-insensitive match.
        assert_eq!(sanitize_for_fts("foo and bar"), "foo bar");
        assert_eq!(sanitize_for_fts("foo Or bar"), "foo bar");
        // But operators as substrings of real words must be preserved.
        assert_eq!(sanitize_for_fts("android orchestra"), "android orchestra");
        assert_eq!(sanitize_for_fts("notion nearby"), "notion nearby");
    }

    #[test]
    fn truncates_long_input_on_char_boundary() {
        // Build an input longer than the cap with multi-byte chars near the
        // boundary to verify char-boundary handling.
        let mut input = "a".repeat(MEMORY_SEARCH_MAX_QUERY_LEN - 2);
        input.push('é'); // 2 bytes — straddles the cap
        input.push_str("xyz");
        let out = sanitize_for_fts(&input);
        // Result must be shorter than the original (truncation happened) and
        // must be valid UTF-8 (the call wouldn't return a String otherwise).
        assert!(out.len() <= MEMORY_SEARCH_MAX_QUERY_LEN);
        assert!(!out.contains("xyz"));
    }

    #[test]
    fn injection_attempt_neutralised() {
        // A query designed to be interpreted as FTS5 syntax should be reduced
        // to harmless tokens.
        let evil = r#"title:"oauth refresh" AND content:secret*"#;
        let cleaned = sanitize_for_fts(evil);
        // Surviving tokens are the words, with column-filter colons and
        // quotes stripped, and bare AND dropped.
        assert_eq!(cleaned, "title oauth refresh content secret");
    }

    #[test]
    fn whitespace_only_after_metachar_strip_returns_empty() {
        // A query that's nothing but metachars should reduce to empty.
        assert_eq!(sanitize_for_fts(r#""""""#), "");
        assert_eq!(sanitize_for_fts("***"), "");
        assert_eq!(sanitize_for_fts("AND OR NOT"), "");
    }

    #[test]
    fn unicode_text_passes_through() {
        // Non-ASCII Unicode is not in the metachar set; the porter unicode61
        // tokenizer in FTS5 will handle it correctly downstream.
        assert_eq!(sanitize_for_fts("café résumé"), "café résumé");
    }
}

// ---------------------------------------------------------------------------
// Inventory-based memory backend registration
// ---------------------------------------------------------------------------

/// Dependencies passed to memory backend factories at construction time.
pub struct MemoryDeps<'a> {
    pub settings: &'a Settings,
    pub db_conn: Arc<Mutex<rusqlite::Connection>>,
}

/// A self-registering memory backend factory.
///
/// Each memory backend module submits one of these via `inventory::submit!`.
/// At startup, `build_memory()` iterates them to construct the configured
/// backend.
pub struct MemoryRegistration {
    /// Backend name (e.g. "sqlite").
    pub name: &'static str,
    /// Build the memory backend from shared dependencies.
    pub build_fn: fn(&MemoryDeps) -> anyhow::Result<Arc<dyn Memory>>,
}

inventory::collect!(MemoryRegistration);

/// Build a memory backend from inventory-registered backends.
///
/// Looks up the backend named "sqlite" specifically rather than taking the
/// first registered backend.
pub fn build_memory(deps: MemoryDeps) -> anyhow::Result<Arc<dyn Memory>> {
    for reg in inventory::iter::<MemoryRegistration> {
        if reg.name == "sqlite" {
            return (reg.build_fn)(&deps);
        }
    }
    anyhow::bail!("no memory backend named 'sqlite' registered")
}
