use crate::complete::{CompleteFlags, Completion};
use crate::history::History;
use fish_wcstringutil::{StringFuzzyMatch, string_fuzzy_match_string};
use std::sync::Arc;
use std::time::SystemTime;
use crate::prelude::*;

/// Maximum number of history items to scan.
const MAX_HISTORY_SCAN: usize = 10_000;

/// Maximum number of results to return.
const MAX_RESULTS: usize = 100;

/// Flags shared by all path jump completions.
const PATH_COMPLETION_FLAGS: CompleteFlags = CompleteFlags::REPLACES_LINE
    .union(CompleteFlags::DONT_ESCAPE)
    .union(CompleteFlags::DONT_SORT);

/// Compute a frecency score from frequency and last_used time.
fn frecency_score(frequency: u32, last_used: SystemTime, now: SystemTime) -> f64 {
    let age_secs = now
        .duration_since(last_used)
        .unwrap_or_default()
        .as_secs();
    let recency_weight: f64 = if age_secs < 3600 {
        16.0
    } else if age_secs < 86400 {
        8.0
    } else if age_secs < 604800 {
        4.0
    } else if age_secs < 2_592_000 {
        2.0
    } else {
        1.0
    };
    frequency as f64 * recency_weight
}

/// Expand `h/` prefix to `~/` in search terms for convenience.
fn expand_search_shorthand(term: &wstr) -> WString {
    if term.starts_with("h/") {
        let mut expanded = WString::from_str("~/");
        expanded.push_utfstr(&term[2..]);
        expanded
    } else {
        term.to_owned()
    }
}

/// Resolve `~/` to `$HOME/` in a path. Returns the path unchanged if no `$HOME` is set.
fn resolve_tilde(path: &wstr) -> WString {
    if path.starts_with("~/") {
        if let Ok(home) = std::env::var("HOME") {
            let mut resolved = WString::from_str(&home);
            resolved.push_utfstr(&path[1..]);
            return resolved;
        }
    }
    path.to_owned()
}

/// Format a path for insertion into the command line.
/// Expands `~/` to the actual home directory, then single-quotes if the path contains spaces.
pub fn format_path_for_insertion(path: &wstr) -> WString {
    let expanded = resolve_tilde(path);

    // Single-quote if it contains shell metacharacters.
    if expanded.chars().any(|c| matches!(c,
        ' ' | '\t' | '(' | ')' | '\\' | '\'' | '"' | '$' |
        '&' | ';' | '|' | '<' | '>' | '*' | '?' | '#' | '{' | '}'
    )) {
        // Escape any single quotes inside the path by ending the quote, adding escaped quote, reopening.
        let mut result = WString::from_str("'");
        for c in expanded.chars() {
            if c == '\'' {
                result.push_str("'\\''");
            } else {
                result.push(c);
            }
        }
        result.push('\'');
        result
    } else {
        expanded
    }
}

/// Pre-scored, sorted path index. Built once per ^J session.
pub struct PathIndex {
    /// (path, frecency_score), sorted by score descending.
    entries: Vec<(WString, f64)>,
}

/// Build the full scored index from history. Expensive — call on background thread.
pub fn build_index(history: &Arc<History>) -> PathIndex {
    let now = SystemTime::now();
    let raw_entries = history.collect_path_entries(MAX_HISTORY_SCAN);

    let mut entries: Vec<(WString, f64)> = raw_entries
        .into_iter()
        .map(|(path, freq, last_used)| {
            let score = frecency_score(freq, last_used, now);
            (path, score)
        })
        .collect();

    entries.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    PathIndex { entries }
}

/// Filter a pre-built index by search term. Cheap — call synchronously on main thread.
pub fn filter_index(index: &PathIndex, search_term: &wstr) -> Vec<Completion> {
    let search_expanded = expand_search_shorthand(search_term);
    let search_resolved = resolve_tilde(&search_expanded);

    index
        .entries
        .iter()
        .filter(|(path, _)| {
            search_term.is_empty()
                || string_fuzzy_match_string(&search_expanded, path, false).is_some()
                || string_fuzzy_match_string(&search_resolved, path, false).is_some()
        })
        .take(MAX_RESULTS)
        .map(|(path, _)| {
            Completion::new(
                path.clone(),
                WString::new(),
                StringFuzzyMatch::exact_match(),
                PATH_COMPLETION_FLAGS,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::osstr2wcstring;
    use crate::history::{HistoryItem, PersistenceMode};
    use std::time::Duration;

    /// Create a test history in a temp directory with the given items.
    /// Each item is (command_text, paths, timestamp).
    fn create_test_history(
        items: &[(&str, &[&str], SystemTime)],
    ) -> (Arc<History>, fish_tempfile::TempDir) {
        let tmpdir = fish_tempfile::new_dir().unwrap();
        let hist_dir = osstr2wcstring(tmpdir.path());
        let history = History::new(L!("path_jump_test"), Some(hist_dir));
        for (cmd, paths, when) in items {
            let mut item =
                HistoryItem::new(WString::from_str(cmd), *when, PersistenceMode::Memory);
            item.set_required_paths(paths.iter().map(|p| WString::from_str(p)).collect());
            history.add(item, false);
        }
        (history, tmpdir)
    }

    fn paths_from_completions(completions: &[Completion]) -> Vec<WString> {
        completions.iter().map(|c| c.completion.clone()).collect()
    }

    // ---- frecency_score ----

    #[test]
    fn test_frecency_score_recent_items_score_higher() {
        let now = SystemTime::now();
        let one_minute_ago = now - Duration::from_secs(60);
        let one_week_ago = now - Duration::from_secs(7 * 86400);

        let recent = frecency_score(1, one_minute_ago, now);
        let old = frecency_score(1, one_week_ago, now);
        assert!(recent > old, "recent={recent} should be > old={old}");
    }

    #[test]
    fn test_frecency_score_frequency_multiplies() {
        let now = SystemTime::now();
        let ts = now - Duration::from_secs(60);

        let once = frecency_score(1, ts, now);
        let five_times = frecency_score(5, ts, now);
        assert_eq!(five_times, once * 5.0);
    }

    #[test]
    fn test_frecency_score_decay_buckets() {
        let now = SystemTime::now();
        // <1h: 16x
        assert_eq!(frecency_score(1, now - Duration::from_secs(30 * 60), now), 16.0);
        // <1d: 8x
        assert_eq!(frecency_score(1, now - Duration::from_secs(2 * 3600), now), 8.0);
        // <1w: 4x
        assert_eq!(frecency_score(1, now - Duration::from_secs(3 * 86400), now), 4.0);
        // <1mo: 2x
        assert_eq!(frecency_score(1, now - Duration::from_secs(14 * 86400), now), 2.0);
        // older: 1x
        assert_eq!(frecency_score(1, now - Duration::from_secs(60 * 86400), now), 1.0);
    }

    // ---- expand_search_shorthand ----

    #[test]
    fn test_expand_search_shorthand_h_prefix() {
        assert_eq!(expand_search_shorthand(L!("h/foo")), "~/foo");
        assert_eq!(expand_search_shorthand(L!("h/")), "~/");
    }

    #[test]
    fn test_expand_search_shorthand_no_prefix() {
        assert_eq!(expand_search_shorthand(L!("foo")), "foo");
        assert_eq!(expand_search_shorthand(L!("/usr/bin")), "/usr/bin");
        assert_eq!(expand_search_shorthand(L!("")), "");
    }

    // ---- format_path_for_insertion ----

    #[test]
    fn test_format_path_simple() {
        assert_eq!(format_path_for_insertion(L!("/usr/bin")), "/usr/bin");
        assert_eq!(format_path_for_insertion(L!("foo/bar")), "foo/bar");
    }

    #[test]
    fn test_format_path_with_spaces() {
        assert_eq!(
            format_path_for_insertion(L!("/my dir/file")),
            "'/my dir/file'"
        );
    }

    #[test]
    fn test_format_path_with_single_quote() {
        assert_eq!(
            format_path_for_insertion(L!("/it's/a/path")),
            "'/it'\\''s/a/path'"
        );
    }

    #[test]
    fn test_format_path_with_various_metacharacters() {
        // Dollar sign
        assert_eq!(format_path_for_insertion(L!("/foo$bar")), "'/foo$bar'");
        // Asterisk
        assert_eq!(format_path_for_insertion(L!("/foo*")), "'/foo*'");
        // Parentheses
        assert_eq!(format_path_for_insertion(L!("/(foo)")), "'/(foo)'");
    }

    // ---- build_index + filter_index (integration) ----

    #[test]
    fn test_build_index_empty_history() {
        let (history, _tmpdir) = create_test_history(&[]);
        let index = build_index(&history);
        assert!(index.entries.is_empty());
    }

    #[test]
    fn test_build_index_items_without_paths_ignored() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("ls", &[], now),
            ("echo hello", &[], now),
        ]);
        let index = build_index(&history);
        assert!(index.entries.is_empty());
    }

    #[test]
    fn test_build_index_deduplicates_paths() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("vim /foo/bar", &["/foo/bar"], now),
            ("cat /foo/bar", &["/foo/bar"], now),
            ("less /foo/bar", &["/foo/bar"], now),
        ]);
        let index = build_index(&history);
        // /foo/bar appears 3 times but should be deduplicated to one entry.
        assert_eq!(index.entries.len(), 1);
        assert_eq!(index.entries[0].0, "/foo/bar");
    }

    #[test]
    fn test_build_index_frequency_affects_score() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("vim /rare", &["/rare"], now),
            ("vim /common", &["/common"], now),
            ("cat /common", &["/common"], now),
            ("less /common", &["/common"], now),
        ]);
        let index = build_index(&history);
        // /common (freq=3) should rank above /rare (freq=1).
        assert_eq!(index.entries.len(), 2);
        assert_eq!(index.entries[0].0, "/common");
        assert_eq!(index.entries[1].0, "/rare");
    }

    #[test]
    fn test_build_index_recency_affects_ranking() {
        let now = SystemTime::now();
        let old = now - Duration::from_secs(60 * 86400); // 60 days ago
        let (history, _tmpdir) = create_test_history(&[
            ("vim /old", &["/old"], old),
            ("vim /old", &["/old"], old),
            ("vim /old", &["/old"], old),
            ("vim /recent", &["/recent"], now),
        ]);
        let index = build_index(&history);
        // /recent (freq=1, weight=16) = 16.0 vs /old (freq=3, weight=1) = 3.0
        assert_eq!(index.entries[0].0, "/recent");
        assert_eq!(index.entries[1].0, "/old");
    }

    #[test]
    fn test_filter_index_empty_term_returns_all() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("vim /a", &["/a"], now),
            ("vim /b", &["/b"], now),
        ]);
        let index = build_index(&history);
        let results = filter_index(&index, L!(""));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn test_filter_index_fuzzy_matching() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("vim /usr/local/bin/python", &["/usr/local/bin/python"], now),
            ("vim /etc/hosts", &["/etc/hosts"], now),
            ("vim /usr/bin/perl", &["/usr/bin/perl"], now),
        ]);
        let index = build_index(&history);
        // "python" should match /usr/local/bin/python.
        let results = filter_index(&index, L!("python"));
        let paths = paths_from_completions(&results);
        assert!(paths.contains(&WString::from_str("/usr/local/bin/python")));
        assert!(!paths.contains(&WString::from_str("/etc/hosts")));
    }

    #[test]
    fn test_filter_index_preserves_score_order() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("vim /foo/ab", &["/foo/ab"], now),
            ("vim /bar/ab", &["/bar/ab"], now),
            ("cat /bar/ab", &["/bar/ab"], now),
        ]);
        let index = build_index(&history);
        // Filter for "ab" — both match, but /bar/ab has higher frequency.
        let results = filter_index(&index, L!("ab"));
        let paths = paths_from_completions(&results);
        assert_eq!(paths.len(), 2);
        assert_eq!(paths[0], "/bar/ab");
        assert_eq!(paths[1], "/foo/ab");
    }

    #[test]
    fn test_filter_index_no_match() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("vim /foo/bar", &["/foo/bar"], now),
        ]);
        let index = build_index(&history);
        let results = filter_index(&index, L!("zzz_no_match"));
        assert!(results.is_empty());
    }

    #[test]
    fn test_filter_index_h_shorthand() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("vim ~/documents/notes.txt", &["~/documents/notes.txt"], now),
            ("vim /etc/hosts", &["/etc/hosts"], now),
        ]);
        let index = build_index(&history);
        // "h/doc" should expand to "~/doc" and match "~/documents/notes.txt".
        let results = filter_index(&index, L!("h/doc"));
        let paths = paths_from_completions(&results);
        assert_eq!(paths.len(), 1);
        assert_eq!(paths[0], "~/documents/notes.txt");
    }

    #[test]
    fn test_filter_index_max_results_capped() {
        let now = SystemTime::now();
        let tmpdir = fish_tempfile::new_dir().unwrap();
        let hist_dir = osstr2wcstring(tmpdir.path());
        let history = History::new(L!("path_jump_cap_test"), Some(hist_dir));
        for i in 0..150 {
            let path = format!("/path/{i}");
            let mut item =
                HistoryItem::new(WString::from_str(&path), now, PersistenceMode::Memory);
            item.set_required_paths(vec![WString::from_str(&path)]);
            history.add(item, false);
        }

        let index = build_index(&history);
        assert_eq!(index.entries.len(), 150);

        let results = filter_index(&index, L!(""));
        assert_eq!(results.len(), MAX_RESULTS);
    }

    #[test]
    fn test_filter_index_completion_flags() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("vim /foo", &["/foo"], now),
        ]);
        let index = build_index(&history);
        let results = filter_index(&index, L!(""));
        assert_eq!(results.len(), 1);
        assert!(results[0].flags.contains(CompleteFlags::REPLACES_LINE));
        assert!(results[0].flags.contains(CompleteFlags::DONT_ESCAPE));
        assert!(results[0].flags.contains(CompleteFlags::DONT_SORT));
    }

    #[test]
    fn test_build_index_multiple_paths_per_item() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("diff /a /b", &["/a", "/b"], now),
        ]);
        let index = build_index(&history);
        let paths: Vec<&WString> = index.entries.iter().map(|(p, _)| p).collect();
        assert!(paths.contains(&&WString::from_str("/a")));
        assert!(paths.contains(&&WString::from_str("/b")));
    }

    #[test]
    fn test_build_index_sorted_descending_by_score() {
        let now = SystemTime::now();
        let (history, _tmpdir) = create_test_history(&[
            ("vim /low", &["/low"], now - Duration::from_secs(60 * 86400)),
            ("vim /mid", &["/mid"], now - Duration::from_secs(3 * 86400)),
            ("vim /high", &["/high"], now),
        ]);
        let index = build_index(&history);
        let scores: Vec<f64> = index.entries.iter().map(|(_, s)| *s).collect();
        for window in scores.windows(2) {
            assert!(window[0] >= window[1], "scores not descending: {scores:?}");
        }
    }
}
