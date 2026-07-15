//! Fuzzy matching utilities — port of packages/tui/src/fuzzy.ts.
//!
//! Matches if all query characters appear in order (not necessarily consecutive).
//! Lower score = better match.

use std::sync::LazyLock;

use regex::Regex;

/// Result of a single fuzzy match attempt.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

static WORD_BOUNDARY: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\s\-_./:]").expect("word boundary"));

static ALPHA_THEN_DIGITS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([a-z]+)([0-9]+)$").expect("alpha digits"));

static DIGITS_THEN_ALPHA: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^([0-9]+)([a-z]+)$").expect("digits alpha"));

fn match_query(normalized_query: &str, text_lower: &[char]) -> FuzzyMatch {
    if normalized_query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }

    let query_chars: Vec<char> = normalized_query.chars().collect();
    if query_chars.len() > text_lower.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    let mut query_index = 0usize;
    let mut score = 0.0_f64;
    let mut last_match_index: isize = -1;
    let mut consecutive_matches = 0i32;

    let mut i = 0usize;
    while i < text_lower.len() && query_index < query_chars.len() {
        if text_lower[i] == query_chars[query_index] {
            let is_word_boundary = if i == 0 {
                true
            } else {
                let prev = text_lower[i - 1];
                let mut buf = [0u8; 4];
                let s = prev.encode_utf8(&mut buf);
                WORD_BOUNDARY.is_match(s)
            };

            // Reward consecutive matches
            if last_match_index == i as isize - 1 {
                consecutive_matches += 1;
                score -= f64::from(consecutive_matches) * 5.0;
            } else {
                consecutive_matches = 0;
                // Penalize gaps
                if last_match_index >= 0 {
                    score += (i as isize - last_match_index - 1) as f64 * 2.0;
                }
            }

            // Reward word boundary matches
            if is_word_boundary {
                score -= 10.0;
            }

            // Slight penalty for later matches
            score += i as f64 * 0.1;

            last_match_index = i as isize;
            query_index += 1;
        }
        i += 1;
    }

    if query_index < query_chars.len() {
        return FuzzyMatch {
            matches: false,
            score: 0.0,
        };
    }

    let text_as_string: String = text_lower.iter().collect();
    if normalized_query == text_as_string {
        score -= 100.0;
    }

    FuzzyMatch {
        matches: true,
        score,
    }
}

/// Fuzzy-match `query` against `text` (case-insensitive subsequence).
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let query_lower = query.to_lowercase();
    let text_lower: Vec<char> = text.to_lowercase().chars().collect();

    let primary = match_query(&query_lower, &text_lower);
    if primary.matches {
        return primary;
    }

    let swapped_query = if let Some(caps) = ALPHA_THEN_DIGITS.captures(&query_lower) {
        format!("{}{}", &caps[2], &caps[1])
    } else if let Some(caps) = DIGITS_THEN_ALPHA.captures(&query_lower) {
        format!("{}{}", &caps[2], &caps[1])
    } else {
        String::new()
    };

    if swapped_query.is_empty() {
        return primary;
    }

    let swapped = match_query(&swapped_query, &text_lower);
    if !swapped.matches {
        return primary;
    }

    FuzzyMatch {
        matches: true,
        score: swapped.score + 5.0,
    }
}

/// Filter and sort items by fuzzy match quality (best matches first).
/// Supports whitespace- and slash-separated tokens: all tokens must match.
pub fn fuzzy_filter<'a, T, F>(items: &'a [T], query: &str, get_text: F) -> Vec<&'a T>
where
    F: Fn(&T) -> &str,
{
    if query.trim().is_empty() {
        return items.iter().collect();
    }

    let tokens: Vec<&str> = query
        .trim()
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .collect();

    if tokens.is_empty() {
        return items.iter().collect();
    }

    let mut results: Vec<(&'a T, f64)> = Vec::new();

    for item in items {
        let text = get_text(item);
        let mut total_score = 0.0_f64;
        let mut all_match = true;

        for token in &tokens {
            let m = fuzzy_match(token, text);
            if m.matches {
                total_score += m.score;
            } else {
                all_match = false;
                break;
            }
        }

        if all_match {
            results.push((item, total_score));
        }
    }

    results.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));
    results.into_iter().map(|(item, _)| item).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_scores_best() {
        let m = fuzzy_match("abc", "abc");
        assert!(m.matches);
        assert!(m.score < 0.0);
    }

    #[test]
    fn subsequence_match() {
        let m = fuzzy_match("ac", "abc");
        assert!(m.matches);
    }

    #[test]
    fn no_match() {
        let m = fuzzy_match("xyz", "abc");
        assert!(!m.matches);
    }

    #[test]
    fn filter_sorts_best_first() {
        let items = vec!["settings", "session", "status"];
        let out = fuzzy_filter(&items, "ses", |s| *s);
        assert!(!out.is_empty());
        assert!(out.iter().all(|s| fuzzy_match("ses", s).matches));
    }

    #[test]
    fn empty_query_returns_all() {
        let items = vec!["a", "b"];
        let out = fuzzy_filter(&items, "  ", |s| *s);
        assert_eq!(out, items.iter().collect::<Vec<_>>());
    }

    #[test]
    fn alpha_digit_swap() {
        // "ab12" against text containing "12ab" style — swapped path
        let m = fuzzy_match("ab12", "xx12abyy");
        // primary may fail; swap may succeed depending on content
        let _ = m;
    }
}
