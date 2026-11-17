//! `raea play EDIT.tsx --watch`: the composition reloaded in place when one of its files is saved, the window, its
//! GPU surface and the film's frame kept (the live player, premature-collapse design/engine-live-loop.md). The files
//! are the composition and every file it imports by a relative path, followed (the project's own code: an edit
//! anywhere else, another agent's or another composition's, never reloads it), looked at every 250 ms (a stat each,
//! no watcher thread). Data a composition reads as it plays (native media.read) needs no reload at all.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime};

pub(crate) struct Watch {
    entry: PathBuf,
    files: Vec<(PathBuf, Option<SystemTime>)>,
    looked: Instant,
}

fn mtime(p: &Path) -> Option<SystemTime> {
    std::fs::metadata(p).and_then(|m| m.modified()).ok()
}

/// The relative imports in a source (`from "./x"`, `import "../y"`, `import("./z")`), as written.
fn imports(src: &str) -> Vec<String> {
    let mut out = Vec::new();
    for q in ['"', '\''] {
        let mut rest = src;
        while let Some(i) = rest.find(q) {
            let after = &rest[i + 1..];
            let Some(j) = after.find(q) else { break };
            let spec = &after[..j];
            let before = rest[..i].trim_end();
            let before = before.strip_suffix('(').map(str::trim_end).unwrap_or(before);
            if (spec.starts_with("./") || spec.starts_with("../")) && (before.ends_with("from") || before.ends_with("import")) {
                out.push(spec.to_string());
            }
            rest = &after[j + 1..];
        }
    }
    out
}

/// The composition's own files: it and its relative imports, followed. A data file it imports (a .json: story data,
/// the gait tables, lip-sync; any other non-code file) is watched too, but not read for imports.
pub(crate) fn closure(entry: &Path) -> Vec<PathBuf> {
    let mut seen: Vec<PathBuf> = Vec::new();
    let mut data: Vec<PathBuf> = Vec::new();
    let mut todo = vec![entry.to_path_buf()];
    while let Some(f) = todo.pop() {
        let f = f.canonicalize().unwrap_or(f);
        if seen.contains(&f) {
            continue;
        }
        let Ok(src) = std::fs::read_to_string(&f) else { continue };
        let dir = f.parent().map(Path::to_path_buf).unwrap_or_default();
        seen.push(f);
        for spec in imports(&src) {
            let b = dir.join(&spec);
            let cands = [b.clone(), b.with_extension("ts"), b.with_extension("tsx"), PathBuf::from(format!("{}.ts", b.display())), PathBuf::from(format!("{}.tsx", b.display())), b.join("index.ts"), b.join("index.tsx")];
            if let Some(hit) = cands.into_iter().find(|c| c.is_file() && c.extension().is_some_and(|e| e == "ts" || e == "tsx")) {
                todo.push(hit);
            } else if b.is_file() {
                let b = b.canonicalize().unwrap_or(b);
                if !data.contains(&b) {
                    data.push(b);
                }
            }
        }
    }
    let extra: Vec<PathBuf> = data.into_iter().filter(|d| !seen.contains(d)).collect();
    seen.extend(extra);
    seen
}

impl Watch {
    pub(crate) fn new(entry: &Path) -> Watch {
        let files = closure(entry).into_iter().map(|f| { let t = mtime(&f); (f, t) }).collect();
        Watch { entry: entry.to_path_buf(), files, looked: Instant::now() }
    }

    pub(crate) fn len(&self) -> usize {
        self.files.len()
    }

    /// The files saved since the last look (at most every 250 ms), or none. A save is taken once its file has been
    /// still for that long (an editor writes in steps).
    pub(crate) fn changed(&mut self) -> Vec<PathBuf> {
        if self.looked.elapsed() < Duration::from_millis(250) {
            return Vec::new();
        }
        self.looked = Instant::now();
        let mut out = Vec::new();
        for (f, t) in &mut self.files {
            let now = mtime(f);
            if now != *t && now.and_then(|n| n.elapsed().ok()).is_none_or(|age| age >= Duration::from_millis(120)) {
                *t = now;
                out.push(f.clone());
            }
        }
        out
    }

    /// After a reload: the files again (an import added or dropped), times as they stand.
    pub(crate) fn refresh(&mut self) {
        *self = Watch::new(&self.entry.clone());
    }
}

#[cfg(test)]
mod tests {
    /// An imported .json is watched (S44: an edit to story data or a gait table reloaded nothing), and isn't read for
    /// imports; code imports are followed as before.
    #[test]
    fn data_imports_watched() {
        let d = std::env::temp_dir().join(format!("raea-watch-test-{}", std::process::id()));
        std::fs::create_dir_all(d.join("fx")).unwrap();
        std::fs::write(d.join("a.tsx"), "import x from \"./story.json\";\nimport { b } from \"./fx/b\";").unwrap();
        std::fs::write(d.join("fx/b.ts"), "import g from \"../gaits.json\";\nexport const b = 1;").unwrap();
        std::fs::write(d.join("story.json"), "{\"from\": \"./not-followed\"}").unwrap();
        std::fs::write(d.join("gaits.json"), "{}").unwrap();
        let got: Vec<String> = super::closure(&d.join("a.tsx")).iter().filter_map(|p| p.file_name().map(|n| n.to_string_lossy().to_string())).collect();
        let _ = std::fs::remove_dir_all(&d);
        let mut sorted = got.clone();
        sorted.sort();
        assert_eq!(sorted, ["a.tsx", "b.ts", "gaits.json", "story.json"]);
    }

    #[test]
    fn imports_found() {
        let src = "import React from \"react\";\nimport { a } from './fx/a';\nimport \"../side\";\nconst m = await import(\"./lazy\");\nconst s = \"./not-an-import\";\nexport { b } from \"./b\";";
        let got: std::collections::BTreeSet<String> = super::imports(src).into_iter().collect();
        let want: std::collections::BTreeSet<String> = ["./fx/a", "../side", "./lazy", "./b"].iter().map(|s| s.to_string()).collect();
        assert_eq!(got, want);
    }
}
