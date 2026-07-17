//! Session search parsing, matching, and sorting.
//!
//! Port of `modes/interactive/components/session-selector-search.ts`.

use std::sync::LazyLock;

use pi_tui::fuzzy::fuzzy_match;
use regex::{Regex, RegexBuilder};

use crate::session_manager::SessionInfo;

/// Oracle `SortMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortMode {
    Threaded,
    Recent,
    Relevance,
}

/// Oracle `NameFilter`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameFilter {
    All,
    Named,
}

/// Oracle token kind (`"fuzzy" | "phrase"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Fuzzy,
    Phrase,
}

/// One parsed search token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchToken {
    pub kind: TokenKind,
    pub value: String,
}

/// Oracle query mode (`"tokens" | "regex"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryMode {
    Tokens,
    Regex,
}

/// Oracle `ParsedSearchQuery`.
#[derive(Debug)]
pub struct ParsedSearchQuery {
    pub mode: QueryMode,
    pub tokens: Vec<SearchToken>,
    pub regex: Option<Regex>,
    /// If set, parsing failed and we should treat query as non-matching.
    pub error: Option<String>,
}

/// Oracle `MatchResult`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatchResult {
    pub matches: bool,
    /// Lower is better; only meaningful when `matches == true`.
    pub score: f64,
}

static WHITESPACE_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("whitespace run"));

fn normalize_whitespace_lower(text: &str) -> String {
    WHITESPACE_RUN
        .replace_all(&text.to_lowercase(), " ")
        .trim()
        .to_owned()
}

fn get_session_search_text(session: &SessionInfo) -> String {
    format!(
        "{} {} {} {}",
        session.id,
        session.name.as_deref().unwrap_or(""),
        session.all_messages_text,
        session.cwd
    )
}

/// Oracle `hasSessionName`.
#[must_use]
pub fn has_session_name(session: &SessionInfo) -> bool {
    session
        .name
        .as_deref()
        .is_some_and(|name| !name.trim().is_empty())
}

fn matches_name_filter(session: &SessionInfo, filter: NameFilter) -> bool {
    match filter {
        NameFilter::All => true,
        NameFilter::Named => has_session_name(session),
    }
}

/// JS `String.prototype.length` prefix count for a byte offset (UTF-16 units).
fn utf16_index_at(text: &str, byte_offset: usize) -> usize {
    text[..byte_offset].chars().map(char::len_utf16).sum()
}

/// Oracle `parseSearchQuery`.
#[must_use]
pub fn parse_search_query(query: &str) -> ParsedSearchQuery {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return ParsedSearchQuery {
            mode: QueryMode::Tokens,
            tokens: Vec::new(),
            regex: None,
            error: None,
        };
    }

    // Regex mode: re:<pattern>
    if let Some(rest) = trimmed.strip_prefix("re:") {
        let pattern = rest.trim();
        if pattern.is_empty() {
            return ParsedSearchQuery {
                mode: QueryMode::Regex,
                tokens: Vec::new(),
                regex: None,
                error: Some("Empty regex".to_owned()),
            };
        }
        return match RegexBuilder::new(pattern).case_insensitive(true).build() {
            Ok(regex) => ParsedSearchQuery {
                mode: QueryMode::Regex,
                tokens: Vec::new(),
                regex: Some(regex),
                error: None,
            },
            Err(err) => ParsedSearchQuery {
                mode: QueryMode::Regex,
                tokens: Vec::new(),
                regex: None,
                error: Some(err.to_string()),
            },
        };
    }

    // Token mode with quote support.
    // Example: foo "node cve" bar
    let mut tokens: Vec<SearchToken> = Vec::new();
    let mut buf = String::new();
    let mut in_quote = false;
    let mut had_unclosed_quote = false;

    let flush = |buf: &mut String, tokens: &mut Vec<SearchToken>, kind: TokenKind| {
        let v = buf.trim().to_owned();
        buf.clear();
        if v.is_empty() {
            return;
        }
        tokens.push(SearchToken { kind, value: v });
    };

    for ch in trimmed.chars() {
        if ch == '"' {
            if in_quote {
                flush(&mut buf, &mut tokens, TokenKind::Phrase);
                in_quote = false;
            } else {
                flush(&mut buf, &mut tokens, TokenKind::Fuzzy);
                in_quote = true;
            }
            continue;
        }

        if !in_quote && ch.is_whitespace() {
            flush(&mut buf, &mut tokens, TokenKind::Fuzzy);
            continue;
        }

        buf.push(ch);
    }

    if in_quote {
        had_unclosed_quote = true;
    }

    // If quotes were unbalanced, fall back to plain whitespace tokenization.
    if had_unclosed_quote {
        return ParsedSearchQuery {
            mode: QueryMode::Tokens,
            tokens: trimmed
                .split_whitespace()
                .map(|t| SearchToken {
                    kind: TokenKind::Fuzzy,
                    value: t.to_owned(),
                })
                .collect(),
            regex: None,
            error: None,
        };
    }

    flush(
        &mut buf,
        &mut tokens,
        if in_quote {
            TokenKind::Phrase
        } else {
            TokenKind::Fuzzy
        },
    );

    ParsedSearchQuery {
        mode: QueryMode::Tokens,
        tokens,
        regex: None,
        error: None,
    }
}

/// Oracle `matchSession`.
#[must_use]
pub fn match_session(session: &SessionInfo, parsed: &ParsedSearchQuery) -> MatchResult {
    let text = get_session_search_text(session);

    if parsed.mode == QueryMode::Regex {
        let Some(regex) = &parsed.regex else {
            return MatchResult {
                matches: false,
                score: 0.0,
            };
        };
        let Some(m) = regex.find(&text) else {
            return MatchResult {
                matches: false,
                score: 0.0,
            };
        };
        let idx = utf16_index_at(&text, m.start());
        return MatchResult {
            matches: true,
            score: idx as f64 * 0.1,
        };
    }

    if parsed.tokens.is_empty() {
        return MatchResult {
            matches: true,
            score: 0.0,
        };
    }

    let mut total_score = 0.0_f64;
    let mut normalized_text: Option<String> = None;

    for token in &parsed.tokens {
        if token.kind == TokenKind::Phrase {
            let normalized =
                normalized_text.get_or_insert_with(|| normalize_whitespace_lower(&text));
            let phrase = normalize_whitespace_lower(&token.value);
            if phrase.is_empty() {
                continue;
            }
            let Some(byte_idx) = normalized.find(&phrase) else {
                return MatchResult {
                    matches: false,
                    score: 0.0,
                };
            };
            total_score += utf16_index_at(normalized, byte_idx) as f64 * 0.1;
            continue;
        }

        let m = fuzzy_match(&token.value, &text);
        if !m.matches {
            return MatchResult {
                matches: false,
                score: 0.0,
            };
        }
        total_score += m.score;
    }

    MatchResult {
        matches: true,
        score: total_score,
    }
}

/// Oracle `filterAndSortSessions`.
#[must_use]
pub fn filter_and_sort_sessions(
    sessions: &[SessionInfo],
    query: &str,
    sort_mode: SortMode,
    name_filter: NameFilter,
) -> Vec<SessionInfo> {
    let name_filtered: Vec<&SessionInfo> = match name_filter {
        NameFilter::All => sessions.iter().collect(),
        NameFilter::Named => sessions
            .iter()
            .filter(|session| matches_name_filter(session, name_filter))
            .collect(),
    };
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return name_filtered.into_iter().cloned().collect();
    }

    let parsed = parse_search_query(query);
    if parsed.error.is_some() {
        return Vec::new();
    }

    // Recent mode: filter only, keep incoming order.
    if sort_mode == SortMode::Recent {
        return name_filtered
            .into_iter()
            .filter(|s| match_session(s, &parsed).matches)
            .cloned()
            .collect();
    }

    // Relevance mode: sort by score, tie-break by modified desc.
    let mut scored: Vec<(&SessionInfo, f64)> = Vec::new();
    for s in name_filtered {
        let res = match_session(s, &parsed);
        if !res.matches {
            continue;
        }
        scored.push((s, res.score));
    }

    scored.sort_by(|a, b| {
        a.1.partial_cmp(&b.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.0.modified_ms.cmp(&a.0.modified_ms))
    });

    scored.into_iter().map(|(s, _)| s.clone()).collect()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn session(id: &str, name: Option<&str>, messages: &str, modified_ms: i64) -> SessionInfo {
        SessionInfo {
            path: PathBuf::from(format!("/tmp/{id}.jsonl")),
            id: id.to_owned(),
            cwd: "/home/user/project".to_owned(),
            name: name.map(str::to_owned),
            parent_session_path: None,
            created: "2026-07-16T12:00:00Z".to_owned(),
            modified_ms,
            message_count: 1,
            first_message: "hello".to_owned(),
            all_messages_text: messages.to_owned(),
        }
    }

    #[test]
    fn parse_empty_query_is_match_all_tokens() {
        let parsed = parse_search_query("   ");
        assert_eq!(parsed.mode, QueryMode::Tokens);
        assert!(parsed.tokens.is_empty());
        assert!(parsed.regex.is_none());
        assert!(parsed.error.is_none());
    }

    #[test]
    fn parse_regex_mode() {
        let parsed = parse_search_query("re:node.*cve");
        assert_eq!(parsed.mode, QueryMode::Regex);
        assert!(parsed.tokens.is_empty());
        assert!(parsed.error.is_none());
        assert!(parsed.regex.is_some());
    }

    #[test]
    fn parse_regex_empty_pattern_errors() {
        let parsed = parse_search_query("re:   ");
        assert_eq!(parsed.mode, QueryMode::Regex);
        assert!(parsed.regex.is_none());
        assert_eq!(parsed.error.as_deref(), Some("Empty regex"));
    }

    #[test]
    fn parse_regex_invalid_pattern_errors() {
        let parsed = parse_search_query("re:(unclosed");
        assert_eq!(parsed.mode, QueryMode::Regex);
        assert!(parsed.regex.is_none());
        assert!(parsed.error.is_some());
    }

    #[test]
    fn parse_tokens_with_quoted_phrase() {
        let parsed = parse_search_query(r#"foo "node cve" bar"#);
        assert_eq!(parsed.mode, QueryMode::Tokens);
        assert_eq!(
            parsed.tokens,
            vec![
                SearchToken {
                    kind: TokenKind::Fuzzy,
                    value: "foo".to_owned()
                },
                SearchToken {
                    kind: TokenKind::Phrase,
                    value: "node cve".to_owned()
                },
                SearchToken {
                    kind: TokenKind::Fuzzy,
                    value: "bar".to_owned()
                },
            ]
        );
    }

    #[test]
    fn parse_unclosed_quote_falls_back_to_whitespace_tokens() {
        // Fallback re-splits the FULL trimmed query and keeps the quote char.
        let parsed = parse_search_query(r#"foo "bar baz"#);
        assert_eq!(parsed.mode, QueryMode::Tokens);
        assert_eq!(
            parsed.tokens,
            vec![
                SearchToken {
                    kind: TokenKind::Fuzzy,
                    value: "foo".to_owned()
                },
                SearchToken {
                    kind: TokenKind::Fuzzy,
                    value: "\"bar".to_owned()
                },
                SearchToken {
                    kind: TokenKind::Fuzzy,
                    value: "baz".to_owned()
                },
            ]
        );
    }

    #[test]
    fn parse_empty_phrase_is_dropped() {
        let parsed = parse_search_query(r#""" foo"#);
        assert_eq!(
            parsed.tokens,
            vec![SearchToken {
                kind: TokenKind::Fuzzy,
                value: "foo".to_owned()
            }]
        );
    }

    #[test]
    fn match_regex_score_counts_utf16_units_before_match() {
        // "😀" is one char but two UTF-16 code units (JS string index semantics).
        let s = session("😀ab", None, "needle", 1);
        let parsed = parse_search_query("re:needle");
        let res = match_session(&s, &parsed);
        assert!(res.matches);
        // "😀ab" = 2+1+1 UTF-16 units, then " " (name "") " " => needle at 6.
        assert_eq!(res.score, 6.0 * 0.1);
    }

    #[test]
    fn match_empty_tokens_matches_all_with_zero_score() {
        let s = session("abc", None, "whatever", 1);
        let parsed = parse_search_query("");
        let res = match_session(&s, &parsed);
        assert!(res.matches);
        assert_eq!(res.score, 0.0);
    }

    #[test]
    fn match_regex_score_is_index_times_point_one() {
        // Search text: "<id> <name> <allMessagesText> <cwd>"
        let s = session("abcdef", None, "zzz needle zzz", 1);
        let parsed = parse_search_query("re:needle");
        let res = match_session(&s, &parsed);
        assert!(res.matches);
        // "abcdef" (6) + " " + "" (name) + " " + "zzz " => needle at index 12.
        assert_eq!(res.score, 12.0 * 0.1);
    }

    #[test]
    fn match_regex_is_case_insensitive() {
        let s = session("abc", None, "NEEDLE", 1);
        let parsed = parse_search_query("re:needle");
        assert!(match_session(&s, &parsed).matches);
    }

    #[test]
    fn match_regex_no_match() {
        let s = session("abc", None, "haystack", 1);
        let parsed = parse_search_query("re:needle");
        let res = match_session(&s, &parsed);
        assert!(!res.matches);
        assert_eq!(res.score, 0.0);
    }

    #[test]
    fn match_missing_regex_never_matches() {
        let s = session("abc", None, "anything", 1);
        let parsed = parse_search_query("re:   ");
        assert!(!match_session(&s, &parsed).matches);
    }

    #[test]
    fn match_phrase_score_uses_normalized_index() {
        // normalized text: "abc zzz hello world /home/user/project"
        let s = session("ABC", None, "zzz   Hello\tWORLD", 1);
        let parsed = parse_search_query("\"hello world\"");
        let res = match_session(&s, &parsed);
        assert!(res.matches);
        // "abc zzz " => phrase begins at index 8.
        assert_eq!(res.score, 8.0 * 0.1);
    }

    #[test]
    fn match_phrase_requires_contiguous_text() {
        let s = session("abc", None, "hello there world", 1);
        let parsed = parse_search_query("\"hello world\"");
        assert!(!match_session(&s, &parsed).matches);
    }

    #[test]
    fn match_all_fuzzy_tokens_must_match() {
        let s = session("abc", None, "alpha beta", 1);
        let both = parse_search_query("alpha beta");
        assert!(match_session(&s, &both).matches);
        let miss = parse_search_query("alpha qqqqxxx");
        assert!(!match_session(&s, &miss).matches);
    }

    #[test]
    fn match_fuzzy_scores_accumulate_with_oracle_values() {
        // Search text: "abc  alpha beta /home/user/project" (name is "").
        // Hand-derived from fuzzy.ts scoring:
        //   "alpha": a@0 (start counts consecutive -5, boundary -10, +0.0),
        //     l@6 (gap (6-0-1)*2=+10, +0.6), p@7 (-5, +0.7), h@8 (-10, +0.8),
        //     a@9 (-15, +0.9)                                  => -32.0
        //   "beta": b@1 (+0.1), e@12 (gap +20, +1.2), t@13 (-5, +1.3),
        //     a@14 (-10, +1.4)                                 =>   9.0
        let s = session("abc", None, "alpha beta", 1);
        let text = get_session_search_text(&s);
        assert_eq!(text, "abc  alpha beta /home/user/project");
        assert!((fuzzy_match("alpha", &text).score - -32.0).abs() < 1e-9);
        assert!((fuzzy_match("beta", &text).score - 9.0).abs() < 1e-9);
        let parsed = parse_search_query("alpha beta");
        let res = match_session(&s, &parsed);
        assert!(res.matches);
        assert!((res.score - -23.0).abs() < 1e-9);
    }

    #[test]
    fn filter_name_filter_named_only() {
        let sessions = vec![
            session("a", Some("named"), "x", 3),
            session("b", None, "x", 2),
            session("c", Some("   "), "x", 1),
        ];
        let out = filter_and_sort_sessions(&sessions, "", SortMode::Recent, NameFilter::Named);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "a");
    }

    #[test]
    fn filter_empty_query_keeps_incoming_order() {
        let sessions = vec![session("a", None, "x", 1), session("b", None, "x", 2)];
        let out = filter_and_sort_sessions(&sessions, "  ", SortMode::Relevance, NameFilter::All);
        assert_eq!(
            out.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[test]
    fn filter_error_query_returns_empty() {
        let sessions = vec![session("a", None, "x", 1)];
        let out =
            filter_and_sort_sessions(&sessions, "re:(unclosed", SortMode::Recent, NameFilter::All);
        assert!(out.is_empty());
    }

    #[test]
    fn filter_recent_mode_filters_but_keeps_order() {
        let sessions = vec![
            session("a", None, "needle far far away", 1),
            session("b", None, "no match here", 2),
            session("c", None, "needle", 3),
        ];
        let out =
            filter_and_sort_sessions(&sessions, "re:needle", SortMode::Recent, NameFilter::All);
        assert_eq!(
            out.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["a", "c"]
        );
    }

    #[test]
    fn filter_relevance_sorts_by_score_ascending() {
        // Lower score (earlier match index) sorts first.
        let sessions = vec![
            session("aaa", None, "zzzzz zzzzz needle", 1),
            session("bbb", None, "needle", 2),
        ];
        let out =
            filter_and_sort_sessions(&sessions, "re:needle", SortMode::Relevance, NameFilter::All);
        assert_eq!(
            out.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["bbb", "aaa"]
        );
    }

    #[test]
    fn filter_relevance_ties_break_by_modified_desc() {
        // Identical search text => identical scores; newer session first.
        let mut older = session("same", None, "needle", 100);
        let mut newer = session("same", None, "needle", 200);
        older.cwd = "/cwd".to_owned();
        newer.cwd = "/cwd".to_owned();
        older.path = PathBuf::from("/tmp/older.jsonl");
        newer.path = PathBuf::from("/tmp/newer.jsonl");
        let sessions = vec![older, newer];
        let out =
            filter_and_sort_sessions(&sessions, "re:needle", SortMode::Relevance, NameFilter::All);
        assert_eq!(out[0].path, PathBuf::from("/tmp/newer.jsonl"));
        assert_eq!(out[1].path, PathBuf::from("/tmp/older.jsonl"));
    }

    #[test]
    fn filter_threaded_mode_with_query_takes_scored_branch() {
        // Threaded (non-recent) with a query behaves like relevance scoring.
        let sessions = vec![
            session("aaa", None, "zzzzz zzzzz needle", 1),
            session("bbb", None, "needle", 2),
        ];
        let out =
            filter_and_sort_sessions(&sessions, "re:needle", SortMode::Threaded, NameFilter::All);
        assert_eq!(
            out.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(),
            vec!["bbb", "aaa"]
        );
    }
}
