use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::detect::{self, ComponentKind};

#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub path: PathBuf,
    pub is_dir: bool,
    pub kind: ComponentKind,
}

/// One visible line of the tree. `depth` drives indentation; `expanded` is
/// only meaningful for directories (C17: in-place expand/collapse).
#[derive(Debug, Clone)]
pub struct Row {
    pub entry: Entry,
    pub depth: usize,
    pub expanded: bool,
}

/// Tree navigation model: directories expand/collapse in place (like VS Code's
/// explorer). Directory children are loaded lazily on first expand. The model
/// is pure and unit-testable (C02/C17); the TUI layer renders `rows` and
/// forwards key events here.
pub struct Nav {
    root: PathBuf,
    rows: Vec<Row>,
    expanded: HashSet<PathBuf>,
    children: HashMap<PathBuf, Vec<Entry>>,
    cursor: usize,
}

fn load_children(dir: &Path) -> std::io::Result<Vec<Entry>> {
    let mut dirs: Vec<Entry> = Vec::new();
    let mut files: Vec<Entry> = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let is_dir = path.is_dir();
        // Skip noise that is never part of the flow.
        if !is_dir
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'))
        {
            continue;
        }
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let kind = if is_dir {
            ComponentKind::Other
        } else {
            detect::classify(&path)
        };
        let item = Entry {
            name,
            path,
            is_dir,
            kind,
        };
        if is_dir {
            dirs.push(item)
        } else {
            files.push(item)
        }
    }
    dirs.sort_by(|a, b| a.name.cmp(&b.name));
    files.sort_by(|a, b| a.name.cmp(&b.name));
    dirs.extend(files);
    Ok(dirs)
}

impl Nav {
    pub fn open(root: &Path) -> std::io::Result<Nav> {
        let root = root.canonicalize()?;
        let mut nav = Nav {
            root,
            rows: Vec::new(),
            expanded: HashSet::new(),
            children: HashMap::new(),
            cursor: 0,
        };
        nav.reload_root()?;
        Ok(nav)
    }

    fn reload_root(&mut self) -> std::io::Result<()> {
        let children = load_children(&self.root)?;
        self.children.insert(self.root.clone(), children);
        self.rebuild();
        Ok(())
    }

    /// Recompute visible rows: DFS from root children, descending into
    /// expanded directories only.
    fn rebuild(&mut self) {
        self.rows.clear();
        let Some(roots) = self.children.get(&self.root).cloned() else {
            return;
        };
        let mut stack: Vec<(Entry, usize)> = roots.into_iter().rev().map(|e| (e, 0)).collect();
        while let Some((entry, depth)) = stack.pop() {
            let expanded = entry.is_dir && self.expanded.contains(&entry.path);
            self.rows.push(Row {
                entry: entry.clone(),
                depth,
                expanded,
            });
            if expanded {
                if let Some(kids) = self.children.get(&entry.path) {
                    for child in kids.iter().rev() {
                        stack.push((child.clone(), depth + 1));
                    }
                }
            }
        }
        if self.cursor >= self.rows.len() {
            self.cursor = self.rows.len().saturating_sub(1);
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn cursor(&self) -> usize {
        self.cursor
    }

    #[allow(dead_code)] // exercised by unit tests; part of the nav model API
    pub fn selected(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    pub fn move_down(&mut self) {
        if self.cursor + 1 < self.rows.len() {
            self.cursor += 1;
        }
    }

    pub fn move_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// Jump the cursor to a visible row index (mouse click).
    pub fn set_cursor(&mut self, index: usize) {
        if index < self.rows.len() {
            self.cursor = index;
        }
    }

    /// Enter on the current row: toggle a directory in place, return a file to
    /// the caller without side effects (C08: file selection itself is inert).
    pub fn toggle(&mut self) -> std::io::Result<Option<Entry>> {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return Ok(None);
        };
        if !row.entry.is_dir {
            return Ok(Some(row.entry));
        }
        if row.expanded {
            self.expanded.remove(&row.entry.path);
        } else {
            self.children
                .entry(row.entry.path.clone())
                .or_insert_with(|| load_children(&row.entry.path).unwrap_or_default());
            self.expanded.insert(row.entry.path.clone());
        }
        self.rebuild();
        Ok(None)
    }

    /// → / expand: expand a collapsed directory (no-op on files/expanded dirs).
    pub fn expand(&mut self) -> std::io::Result<()> {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return Ok(());
        };
        if row.entry.is_dir && !row.expanded {
            self.children
                .entry(row.entry.path.clone())
                .or_insert_with(|| load_children(&row.entry.path).unwrap_or_default());
            self.expanded.insert(row.entry.path.clone());
            self.rebuild();
        }
        Ok(())
    }

    /// ← / collapse: collapse the selected directory if expanded; otherwise
    /// jump the cursor to its parent directory row.
    pub fn collapse_or_parent(&mut self) {
        let Some(row) = self.rows.get(self.cursor).cloned() else {
            return;
        };
        if row.entry.is_dir && row.expanded {
            self.expanded.remove(&row.entry.path);
            self.rebuild();
            return;
        }
        if let Some(parent) = row.entry.path.parent() {
            if let Some(index) = self.rows.iter().position(|r| r.entry.path == parent) {
                self.cursor = index;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        let mut f = fs::File::create(path).unwrap();
        f.write_all(content.as_bytes()).unwrap();
    }

    /// root/
    ///   gallery/
    ///     gallery.component.ts
    ///     photo-card/
    ///       photo-card.component.ts
    ///   badges/
    ///     badge.component.ts
    ///   helper.ts
    ///   Button.tsx
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("gallery/gallery.component.ts"),
            "@Component({}) export class A {}",
        );
        write(
            &root.join("gallery/photo-card/photo-card.component.ts"),
            "@Component({}) export class B {}",
        );
        write(
            &root.join("badges/badge.component.ts"),
            "@Component({}) export class C {}",
        );
        write(&root.join("helper.ts"), "export const x = 1;");
        write(
            &root.join("Button.tsx"),
            "export const Button = () => null;",
        );
        dir
    }

    fn names(nav: &Nav) -> Vec<(String, usize)> {
        nav.rows()
            .iter()
            .map(|r| (r.entry.name.clone(), r.depth))
            .collect()
    }

    #[test]
    fn initial_rows_are_root_children_at_depth_zero() {
        let dir = fixture();
        let nav = Nav::open(dir.path()).unwrap();
        // dirs first (badges, gallery), then files (Button.tsx, helper.ts)
        let got = names(&nav);
        assert_eq!(
            got,
            vec![
                ("badges".into(), 0),
                ("gallery".into(), 0),
                ("Button.tsx".into(), 0),
                ("helper.ts".into(), 0),
            ]
        );
    }

    #[test]
    fn enter_expands_directory_in_place() {
        let dir = fixture();
        let mut nav = Nav::open(dir.path()).unwrap();
        nav.move_down(); // cursor on gallery/
        let selected = nav.toggle().unwrap();
        assert!(selected.is_none(), "entering a folder selects nothing");
        assert_eq!(
            names(&nav),
            vec![
                ("badges".into(), 0),
                ("gallery".into(), 0),
                ("photo-card".into(), 1),
                ("gallery.component.ts".into(), 1),
                ("Button.tsx".into(), 0),
                ("helper.ts".into(), 0),
            ]
        );
    }

    #[test]
    fn enter_again_collapses_in_place() {
        let dir = fixture();
        let mut nav = Nav::open(dir.path()).unwrap();
        nav.move_down();
        nav.toggle().unwrap();
        nav.toggle().unwrap();
        assert_eq!(names(&nav).len(), 4, "collapsed back to root children");
    }

    #[test]
    fn nested_expansion_shows_depth() {
        let dir = fixture();
        let mut nav = Nav::open(dir.path()).unwrap();
        nav.move_down(); // gallery/
        nav.toggle().unwrap(); // expand gallery → cursor stays on gallery
        nav.move_down(); // photo-card/ (dirs first)
        nav.toggle().unwrap(); // expand photo-card
        assert_eq!(
            names(&nav),
            vec![
                // DFS tree order: photo-card's whole subtree precedes the
                // sibling file that follows it (standard tree/VS Code order).
                ("badges".into(), 0),
                ("gallery".into(), 0),
                ("photo-card".into(), 1),
                ("photo-card.component.ts".into(), 2),
                ("gallery.component.ts".into(), 1),
                ("Button.tsx".into(), 0),
                ("helper.ts".into(), 0),
            ]
        );
    }

    #[test]
    fn selecting_a_file_returns_it_without_side_effects() {
        let dir = fixture();
        let mut nav = Nav::open(dir.path()).unwrap();
        nav.move_down(); // gallery/
        nav.toggle().unwrap();
        nav.move_down(); // photo-card/ (dirs first)
        nav.move_down(); // gallery.component.ts
        let entry = nav.toggle().unwrap().expect("file returned");
        assert_eq!(entry.name, "gallery.component.ts");
        assert_eq!(entry.kind, ComponentKind::AngularStandalone);
        assert_eq!(names(&nav).len(), 6, "tree unchanged by file selection");
    }

    #[test]
    fn left_on_child_jumps_to_parent_row() {
        let dir = fixture();
        let mut nav = Nav::open(dir.path()).unwrap();
        nav.move_down(); // gallery/
        nav.toggle().unwrap(); // expanded
        nav.move_down(); // photo-card/ (dirs first)
        nav.toggle().unwrap(); // expand photo-card
        nav.move_down(); // photo-card.component.ts
        nav.collapse_or_parent();
        assert_eq!(
            nav.selected().unwrap().entry.name,
            "photo-card",
            "cursor on parent dir row"
        );
    }

    #[test]
    fn left_on_expanded_dir_collapses_it() {
        let dir = fixture();
        let mut nav = Nav::open(dir.path()).unwrap();
        nav.move_down(); // gallery/
        nav.toggle().unwrap(); // expanded
        nav.collapse_or_parent();
        assert_eq!(names(&nav).len(), 4, "collapsed");
    }

    #[test]
    fn right_expands_collapsed_dir() {
        let dir = fixture();
        let mut nav = Nav::open(dir.path()).unwrap();
        nav.move_down(); // gallery/
        nav.expand().unwrap();
        assert!(names(&nav).iter().any(|(n, _)| n == "gallery.component.ts"));
    }

    #[test]
    fn cursor_clamps_after_collapse_below_cursor() {
        let dir = fixture();
        let mut nav = Nav::open(dir.path()).unwrap();
        nav.move_down(); // gallery/
        nav.toggle().unwrap(); // expand → 6 rows
        nav.move_down();
        nav.move_down();
        nav.move_down();
        nav.move_down(); // cursor near the end (Button.tsx at index 4)
        assert!(nav.cursor() < nav.rows().len());
        nav.move_up();
        nav.move_up();
        // cursor on gallery.component.ts (index 2); collapsing gallery removes rows below.
        nav.move_up();
        nav.toggle().unwrap(); // collapse gallery
        assert!(
            nav.cursor() < nav.rows().len(),
            "cursor clamped after collapse"
        );
    }

    #[test]
    fn component_kinds_are_preserved_in_tree_rows() {
        let dir = fixture();
        let mut nav = Nav::open(dir.path()).unwrap();
        nav.move_down(); // gallery/
        nav.toggle().unwrap();
        let row = nav
            .rows()
            .iter()
            .find(|r| r.entry.name == "gallery.component.ts")
            .unwrap();
        assert_eq!(row.entry.kind, ComponentKind::AngularStandalone);
        assert_eq!(row.depth, 1);
    }
}
