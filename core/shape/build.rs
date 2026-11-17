//! The kept-solid key's source hash (kept.rs, RAEA_SHAPE_SRC_HASH): a blake3 over the source that can change a solid's
//! mesh (all of core/: this crate and its path dependencies, fonts, sdf, grow) and Cargo.lock (manifold-rust's version),
//! so a rebuilt or re-pinned raea that doesn't touch shapes keeps every kept solid; keyed by the executable, each new
//! binary started an empty folder and wrote every solid again (Lane E S50: 8.7 GB in 4 h on a bench member).
//! Conservative: anything under core/ counts. RAEA_SHAPE_KEY=binary keys by the executable as before.
use std::path::{Path, PathBuf};

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
    walk(&ws, Path::new("core"), &mut files);
    println!("cargo:rerun-if-changed={}", ws.join("core").display());
    println!("cargo:rerun-if-changed={}", ws.join("Cargo.lock").display());
    let mut h = blake3::Hasher::new();
    h.update(b"raea-shape-src-1\0");
    for f in &files {
        let Ok(bytes) = std::fs::read(ws.join(f)) else { continue };
        h.update(f.to_string_lossy().replace('\\', "/").as_bytes());
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    println!("cargo:rustc-env=RAEA_SHAPE_SRC_HASH={}", h.finalize().to_hex());
}
