//! Telescope-style fuzzy file finder: a bounded file index of the project
//! root, an fzf-like smart-case subsequence scorer (boundary/consecutive/
//! camelCase bonuses, gap penalties) and the popup state (query + selection).
//! Pure and unit-testable; the TUI layer renders the popup and routes keys.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// Directories never worth searching (build artifacts, VCS, deps).
const SKIPPED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".angular",
    "dist",
    "target",
    "coverage",
    ".next",
    ".output",
    "build",
];

/// Hard cap so a runaway tree cannot exhaust memory; plenty for any UI project.
const MAX_FILES: usize = 50_000;

/// One indexed file: absolute path + display path relative to the root
/// (forward slashes, telescope style).
#[derive(Debug, Clone)]
pub struct Item {
    pub path: PathBuf,
    pub display: String,
}

/// A scored match: which item, its score, and the matched char positions
/// (for telescope-style highlight of the matched characters).
#[derive(Debug, Clone)]
pub struct Scored {
    pub index: usize,
    pub score: i64,
    pub indices: Vec<usize>,
}

fn index_files(root: &Path) -> Vec<Item> {
    let mut out = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_entry(|e| {
        // The root itself is never filtered (tempdirs/hidden roots are
        // legitimate); noise rules apply to everything below it.
        if e.depth() == 0 {
            return true;
        }
        let name = e.file_name().to_string_lossy();
        // Same visibility rule as the tree: hidden entries are noise.
        !SKIPPED_DIRS.contains(&name.as_ref()) && !name.starts_with('.')
    }) {
        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_file() || out.len() >= MAX_FILES {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(root)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_default();
        if rel.is_empty() {
            continue;
        }
        out.push(Item {
            path: entry.path().to_path_buf(),
            display: rel,
        });
    }
    // Deterministic listing (empty query shows alphabetical, like telescope).
    out.sort_by(|a, b| a.display.cmp(&b.display));
    out
}

/// fzf-like smart-case subsequence scoring. Returns the score and the matched
/// char positions, or None when the query is not a subsequence.
///
/// Smart case: a query with any uppercase demands exact case; otherwise
/// matching is case-insensitive. Higher is better.
pub fn score(query: &str, text: &str) -> Option<(i64, Vec<usize>)> {
    if query.is_empty() {
        return Some((0, Vec::new()));
    }
    let case_sensitive = query.chars().any(|c| c.is_uppercase());
    let norm = |c: char| {
        if case_sensitive {
            c
        } else {
            c.to_ascii_lowercase()
        }
    };
    let tc: Vec<char> = text.chars().collect();
    let basename_start = tc.iter().rposition(|&c| c == '/').map_or(0, |p| p + 1);

    let mut total = 0i64;
    let mut indices = Vec::new();
    let mut prev: Option<usize> = None;
    for q in query.chars() {
        let start = prev.map_or(0, |p| p + 1);
        // Greedy earliest match, strictly after the previous one: matches are
        // a forward subsequence (indices stay increasing for highlighting).
        let i = (start..tc.len()).find(|&i| norm(tc[i]) == norm(q))?;
        let mut s = 0i64;
        match prev {
            Some(p) if i == p + 1 => s += 12,            // consecutive run
            Some(p) => s -= ((i - p - 1) as i64).min(8), // gap penalty (bounded)
            None => {}
        }
        if i == 0 || !tc[i - 1].is_ascii_alphanumeric() {
            s += 14; // start of word / after a separator
        }
        if i >= basename_start {
            s += 4; // prefer matches inside the file name
        }
        if tc[i].is_ascii_uppercase() && i > 0 && tc[i - 1].is_ascii_lowercase() {
            s += 8; // camelCase boundary
        }
        total += s;
        indices.push(i);
        prev = Some(i);
    }
    Some((total, indices))
}

/// Fuzzy-search the index. Empty query → everything in index order. Otherwise
/// matches sorted by score desc, then shorter paths, then index order.
fn search(query: &str, items: &[Item]) -> Vec<Scored> {
    let mut out: Vec<Scored> = items
        .iter()
        .enumerate()
        .filter_map(|(index, item)| {
            score(query, &item.display).map(|(score_, indices)| Scored {
                index,
                score: score_,
                indices,
            })
        })
        .collect();
    if query.is_empty() {
        return out;
    }
    out.sort_by(|a, b| {
        b.score
            .cmp(&a.score)
            .then_with(|| {
                items[a.index]
                    .display
                    .len()
                    .cmp(&items[b.index].display.len())
            })
            .then_with(|| a.index.cmp(&b.index))
    });
    out
}

/// Popup finder state. `results` is recomputed on every keystroke (the index
/// is small enough that a linear pass per key is imperceptible).
pub struct Finder {
    pub query: String,
    items: Vec<Item>,
    pub results: Vec<Scored>,
    cursor: usize,
}

impl Finder {
    pub fn open(root: &Path) -> Finder {
        let mut finder = Finder {
            query: String::new(),
            items: index_files(root),
            results: Vec::new(),
            cursor: 0,
        };
        finder.recompute();
        finder
    }

    #[allow(dead_code)] // exercised by unit tests; part of the finder API
    pub fn result_count(&self) -> usize {
        self.results.len()
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    fn recompute(&mut self) {
        self.results = search(&self.query, &self.items);
        self.cursor = 0;
    }

    pub fn push(&mut self, c: char) {
        self.query.push(c);
        self.recompute();
    }

    pub fn pop(&mut self) {
        self.query.pop();
        self.recompute();
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.recompute();
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.results.len() {
            self.cursor += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Display path of the indexed item at `index` (popup rendering).
    pub fn item_display(&self, index: usize) -> &str {
        &self.items[index].display
    }

    /// Absolute path of the highlighted result (None on empty results).
    pub fn selected_path(&self) -> Option<&Path> {
        self.results
            .get(self.cursor)
            .map(|r| self.items[r.index].path.as_path())
    }
}

/// Spans for one result row: matched chars highlighted (cyan bold), the rest
/// in the base style (which carries the selection background).
pub fn result_line(
    display: &str,
    indices: &[usize],
    selected: bool,
) -> ratatui::text::Line<'static> {
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::{Line, Span};

    let base = if selected {
        Style::default().bg(Color::DarkGray)
    } else {
        Style::default()
    };
    let highlight = base.fg(Color::Cyan).add_modifier(Modifier::BOLD);
    let matched: HashSet<usize> = indices.iter().copied().collect();

    let mut spans: Vec<Span> = Vec::new();
    let mut chunk = String::new();
    let mut chunk_matched: Option<bool> = None;
    for (i, ch) in display.chars().enumerate() {
        let is_matched = matched.contains(&i);
        if chunk_matched == Some(is_matched) || chunk.is_empty() {
            chunk.push(ch);
            chunk_matched = Some(is_matched);
        } else {
            spans.push(Span::styled(
                std::mem::take(&mut chunk),
                if chunk_matched == Some(true) {
                    highlight
                } else {
                    base
                },
            ));
            chunk.push(ch);
            chunk_matched = Some(is_matched);
        }
    }
    if !chunk.is_empty() {
        spans.push(Span::styled(
            chunk,
            if chunk_matched == Some(true) {
                highlight
            } else {
                base
            },
        ));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let write = |rel: &str, content: &str| {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        };
        write("gallery/gallery.component.ts", "a");
        write("gallery/photo-card/photo-card.component.ts", "b");
        write("helper.ts", "c");
        write("Button.tsx", "d");
        write("node_modules/pkg/index.js", "e");
        write(".hidden/file.ts", "f");
        dir
    }

    #[test]
    fn index_collects_files_relative_and_skips_noise() {
        let dir = fixture();
        let finder = Finder::open(dir.path());
        let displays: Vec<&str> = finder.items.iter().map(|i| i.display.as_str()).collect();
        assert_eq!(
            displays,
            vec![
                "Button.tsx",
                "gallery/gallery.component.ts",
                "gallery/photo-card/photo-card.component.ts",
                "helper.ts",
            ]
        );
    }

    #[test]
    fn empty_query_lists_everything_in_index_order() {
        let dir = fixture();
        let finder = Finder::open(dir.path());
        assert_eq!(finder.result_count(), 4);
        assert_eq!(
            finder.selected_path(),
            Some(dir.path().join("Button.tsx").as_path())
        );
    }

    #[test]
    fn subsequence_scores_and_reports_positions() {
        let (s, idx) = score("gll", "src/gallery.component.ts").unwrap();
        assert!(s > 0);
        // g and both l's must appear in order.
        let text: Vec<char> = "src/gallery.component.ts".chars().collect();
        assert_eq!(text[idx[0]], 'g');
        assert_eq!(text[idx[1]], 'l');
        assert_eq!(text[idx[2]], 'l');
    }

    #[test]
    fn non_subsequence_fails() {
        assert!(score("xyz", "gallery.component.ts").is_none());
    }

    #[test]
    fn smart_case_requires_exact_case_for_uppercase_queries() {
        // Query has uppercase → case-sensitive: 'G' does not match lowercase.
        assert!(score("G", "gallery.component.ts").is_none());
        assert!(score("G", "Gallery.component.ts").is_some());
        // All-lowercase query matches case-insensitively.
        assert!(score("g", "Gallery.component.ts").is_some());
    }

    #[test]
    fn basename_and_boundaries_rank_file_matches_higher() {
        // Same query, same length: matching inside the basename must outrank
        // matching inside a directory name.
        let (_, dirname) = score("gal", "assets/gallery/icons.ts").unwrap();
        let (_, filename) = score("gal", "assets/icons/gallery.ts").unwrap();
        assert!(filename > dirname, "basename match must outrank dirname match");

        let (_, boundary) = score("comp", "my-component.ts").unwrap();
        let (_, mid) = score("comp", "xcomponent.ts").unwrap();
        assert!(
            boundary > mid,
            "word-boundary match must outrank mid-word match"
        );
    }

    #[test]
    fn search_orders_by_score_and_keeps_empty_query_order() {
        let dir = fixture();
        let mut finder = Finder::open(dir.path());

        finder.push('g');
        finder.push('a');
        finder.push('l');
        let top = finder
            .results
            .first()
            .map(|r| finder.items[r.index].display.clone())
            .unwrap();
        assert_eq!(top, "gallery/gallery.component.ts", "got: {top}");
        assert!(finder.result_count() >= 2);

        // Query "ga": both gallery files.
        finder.pop();
        assert_eq!(finder.result_count(), 2);
        // Query "g": still only the gallery subtree.
        finder.pop();
        assert_eq!(finder.result_count(), 2);
        // Empty query: the full listing back in index (alphabetical) order.
        finder.pop();
        assert_eq!(finder.result_count(), 4);
        assert_eq!(finder.results[0].score, 0);
    }

    #[test]
    fn cursor_navigation_clamps_at_bounds() {
        let dir = fixture();
        let mut finder = Finder::open(dir.path());
        finder.move_down();
        finder.move_down();
        finder.move_down();
        assert_eq!(finder.cursor(), 3);
        finder.move_down(); // clamped
        assert_eq!(finder.cursor(), 3);
        finder.move_up();
        assert_eq!(finder.cursor(), 2);
        finder.move_up();
        finder.move_up();
        finder.move_up(); // clamped at 0
        assert_eq!(finder.cursor(), 0);
    }

    #[test]
    fn selected_path_follows_query_results() {
        let dir = fixture();
        let mut finder = Finder::open(dir.path());
        finder.push('p');
        finder.push('h');
        let selected = finder.selected_path().unwrap().to_path_buf();
        assert!(
            selected.ends_with("photo-card.component.ts"),
            "got: {selected:?}"
        );
        finder.clear();
        assert_eq!(finder.cursor(), 0);
    }

    #[test]
    fn result_line_groups_highlighted_chars() {
        let line = result_line("gallery.ts", &[0, 1, 2], true);
        // Selected base + cyan highlights: first span has the bg + highlight.
        let spans = &line.spans;
        assert_eq!(spans.len(), 2, "one highlighted chunk + one plain chunk");
        assert!(spans[0].style.bg.is_some(), "selection background present");
        assert_eq!(spans[0].style.fg, Some(Color::Cyan));
        assert!(spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(spans[0].content, "gal");
        assert_eq!(spans[1].content, "lery.ts");
    }
}
