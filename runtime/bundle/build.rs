//! The bundle cache's source hash (cache.rs bundle_cache, RAEA_BUNDLER_SRC_HASH): a blake3 over the source that turns
//! an edit into a bundle (this crate and the trees it reads through: runtime/cssurls, core, engine/native, io,
//! systems/layers) and Cargo.lock, so a raea rebuilt or re-pinned without touching the bundler keeps its bundles; keyed
//! by the executable, every pin bundled every chapter again (Lane E S50). RAEA_BUNDLE_KEY=binary keys by the
//! executable as before. Conservative: whole trees count.
use std::path::{Path, PathBuf};

const ROOTS: &[&str] = &["runtime/bundle", "runtime/cssurls", "core", "engine/native", "io", "systems/layers"];

fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root.join(rel)) else { return };
    let mut es: Vec<_> = rd.flatten().collect();
    es.sort_by_key(|e| e.file_name());
    for e in es {
        let name = e.file_name();
        let r = rel.join(&name);
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if ["target", "node_modules", ".git"].iter().any(|s| name == *s) {
                continue;
            }
            walk(root, &r, out);
        } else if ft.is_file() {
            out.push(r);
        }
    }
}

fn main() {
    let ws = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let mut files = vec![PathBuf::from("Cargo.lock")];
    for r in ROOTS {
        walk(&ws, Path::new(r), &mut files);
        println!("cargo:rerun-if-changed={}", ws.join(r).display());
    }
    println!("cargo:rerun-if-changed={}", ws.join("Cargo.lock").display());
    let mut h = blake3::Hasher::new();
    h.update(b"raea-bundler-src-1\0");
    for f in &files {
        let Ok(bytes) = std::fs::read(ws.join(f)) else { continue };
        h.update(f.to_string_lossy().replace('\\', "/").as_bytes());
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    println!("cargo:rustc-env=RAEA_BUNDLER_SRC_HASH={}", h.finalize().to_hex());
}
