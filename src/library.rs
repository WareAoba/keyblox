#![cfg_attr(not(windows), allow(dead_code))]
//! Song list for the sidebar: files added one by one and linked folders
//! (scanned for .mid/.midi, subfolders included). Saved as a plain text file,
//! one entry per line: `F<TAB>path` or `D<TAB>path`. OS-independent.

use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: PathBuf,
    pub folder: bool,
}

/// One line in the sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    pub label: String,
    /// file to open (None for a folder header)
    pub file: Option<PathBuf>,
    /// index into `Library::entries` this row belongs to
    pub entry: usize,
    /// folder header, or a file inside a folder
    pub kind: RowKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    File,
    Folder,
    InFolder,
}

#[derive(Default)]
pub struct Library {
    pub entries: Vec<Entry>,
}

pub fn is_midi(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("mid") || e.eq_ignore_ascii_case("midi")).unwrap_or(false)
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.to_string_lossy().into_owned())
}

/// All MIDI files under `dir`, sorted by path (case-insensitive).
fn scan(dir: &Path) -> Vec<PathBuf> {
    let mut out = vec![];
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(t) if t.is_dir() => stack.push(p),
                Ok(_) if is_midi(&p) => out.push(p),
                _ => {}
            }
        }
    }
    out.sort_by_key(|p| p.to_string_lossy().to_lowercase());
    out
}

impl Library {
    pub fn load(file: &Path) -> Library {
        let text = std::fs::read_to_string(file).unwrap_or_default();
        let mut lib = Library::default();
        for line in text.lines() {
            let (folder, path) = match line.split_once('\t') {
                Some(("F", p)) => (false, p),
                Some(("D", p)) => (true, p),
                _ => continue,
            };
            lib.add(PathBuf::from(path), folder);
        }
        lib
    }

    pub fn save(&self, file: &Path) -> std::io::Result<()> {
        let mut s = String::new();
        for e in &self.entries {
            s += if e.folder { "D\t" } else { "F\t" };
            s += &e.path.to_string_lossy();
            s += "\n";
        }
        std::fs::write(file, s)
    }

    /// Add a file or folder. Returns false if it was already there.
    pub fn add(&mut self, path: PathBuf, folder: bool) -> bool {
        if self.entries.iter().any(|e| e.path == path) {
            return false;
        }
        self.entries.push(Entry { path, folder });
        true
    }

    pub fn remove(&mut self, i: usize) {
        if i < self.entries.len() {
            self.entries.remove(i);
        }
    }

    /// Sidebar rows: folders expanded (scanned now), files as they are.
    pub fn rows(&self) -> Vec<Row> {
        let mut rows = vec![];
        for (i, e) in self.entries.iter().enumerate() {
            if e.folder {
                rows.push(Row { label: name_of(&e.path), file: None, entry: i, kind: RowKind::Folder });
                for f in scan(&e.path) {
                    let label = f.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                    rows.push(Row { label, file: Some(f), entry: i, kind: RowKind::InFolder });
                }
            } else {
                let label = e.path.file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| name_of(&e.path));
                rows.push(Row { label, file: Some(e.path.clone()), entry: i, kind: RowKind::File });
            }
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("keyblox-lib-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_load_roundtrip_keeps_order_and_kind() {
        let d = tmp("rt");
        let mut lib = Library::default();
        lib.add(d.join("b song.mid"), false);
        lib.add(d.join("곡 폴더"), true);
        lib.add(d.join("a.midi"), false);
        let f = d.join("list.txt");
        lib.save(&f).unwrap();
        let back = Library::load(&f);
        assert_eq!(back.entries, lib.entries);
    }

    #[test]
    fn missing_or_garbled_list_loads_empty_lines_skipped() {
        let d = tmp("bad");
        assert!(Library::load(&d.join("nope.txt")).entries.is_empty());
        let f = d.join("list.txt");
        std::fs::write(&f, "garbage\nF\tC:\\x.mid\r\nX\ty\n").unwrap();
        let lib = Library::load(&f);
        assert_eq!(lib.entries.len(), 1);
        assert_eq!(lib.entries[0].path, PathBuf::from("C:\\x.mid"));
    }

    #[test]
    fn add_ignores_duplicates() {
        let mut lib = Library::default();
        assert!(lib.add("a.mid".into(), false));
        assert!(!lib.add("a.mid".into(), false));
        assert_eq!(lib.entries.len(), 1);
    }

    #[test]
    fn folder_rows_list_midi_files_recursively_sorted() {
        let d = tmp("scan");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        for f in ["b.mid", "A.MID", "notes.txt", "sub/c.midi"] {
            std::fs::write(d.join(f), b"x").unwrap();
        }
        let mut lib = Library::default();
        lib.add(d.join("solo.mid"), false);
        lib.add(d.clone(), true);
        let rows = lib.rows();
        let labels: Vec<_> = rows.iter().map(|r| (r.kind, r.label.as_str(), r.entry)).collect();
        assert_eq!(
            labels,
            vec![
                (RowKind::File, "solo", 0),
                (RowKind::Folder, d.file_name().unwrap().to_str().unwrap(), 1),
                (RowKind::InFolder, "A", 1),
                (RowKind::InFolder, "b", 1),
                (RowKind::InFolder, "c", 1),
            ]
        );
        assert_eq!(rows[0].file, Some(d.join("solo.mid")));
        assert_eq!(rows[1].file, None);
        assert_eq!(rows[4].file, Some(d.join("sub").join("c.midi")));
    }

    #[test]
    fn missing_folder_shows_header_only() {
        let mut lib = Library::default();
        lib.add("/definitely/not/here".into(), true);
        assert_eq!(lib.rows().len(), 1);
    }
}
