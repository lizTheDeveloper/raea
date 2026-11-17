//! The bake key's source hash (native.rs key_of, RAEA_BAKE_SRC_HASH): a blake3 over the source that can change what a
//! native shot's bake holds, so a pin that only touches drawing, capture or the GPU keeps every kept bake, where a hash
//! of the whole binary threw them all away. Conservative by design (a key too narrow serves stale bakes): every
//! workspace source tree is in except the ones that only draw or encode (EXCLUDE), plus Cargo.lock, the workspace
//! Cargo.toml and the toolchain. When unsure whether a tree affects a bake, it stays in. RAEA_BAKE_KEY=binary at run
//! time keys by the whole binary as before. Checked on every pin: a cold bake and a warm one of Ch1, Ch2 and Ch5 must
//! draw the same (design/review-checklists.md A).
use std::path::{Path, PathBuf};

const ROOTS: &[&str] = &["core", "runtime", "engine", "systems", "io", "sdk", "vendor"];
const FILES: &[&str] = &["Cargo.lock", "Cargo.toml", "rust-toolchain.toml"];
/// Trees that only draw, encode or play sound: a bake never runs them (MinimalPlugins + React; its natives come from
/// runtime/*, engine/bevy-plugins and systems/*, all kept).
const EXCLUDE: &[&str] = &[
    "gpu", "engine/render", "runtime/encode", "runtime/gpuframe",
    "vendor/vello_encoding", "vendor/vello_shaders", "vendor/anyrender", "vendor/anyrender_svg", "vendor/blitz-paint",
    "vendor/usvg", "vendor/bevy_solari", "vendor/bevy_hanabi", "vendor/bevy_seedling", "vendor/muxide",
    "vendor/firewheel", "vendor/firewheel-core", "vendor/firewheel-graph", "vendor/firewheel-macros", "vendor/firewheel-nodes",
];
const SKIP_DIRS: &[&str] = &["target", "node_modules", ".git", "dist"];

fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(root.join(rel)) else { return };
    let mut es: Vec<_> = rd.flatten().collect();
    es.sort_by_key(|e| e.file_name());
    for e in es {
        let name = e.file_name();
        let r = rel.join(&name);
        let rs = r.to_string_lossy().replace('\\', "/");
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            if SKIP_DIRS.iter().any(|s| name == *s) || EXCLUDE.contains(&rs.as_str()) {
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
    let mut files: Vec<PathBuf> = FILES.iter().map(PathBuf::from).collect();
    for r in ROOTS {
        walk(&ws, Path::new(r), &mut files);
        println!("cargo:rerun-if-changed={}", ws.join(r).display());
    }
    for f in FILES {
        println!("cargo:rerun-if-changed={}", ws.join(f).display());
    }
    let mut h = blake3::Hasher::new();
    h.update(b"raea-bake-src-1\0");
    for f in &files {
        let Ok(bytes) = std::fs::read(ws.join(f)) else { continue };
        h.update(f.to_string_lossy().replace('\\', "/").as_bytes());
        h.update(&(bytes.len() as u64).to_le_bytes());
        h.update(&bytes);
    }
    println!("cargo:rustc-env=RAEA_BAKE_SRC_HASH={}", h.finalize().to_hex());
    println!("cargo:rustc-env=RAEA_BAKE_SRC_FILES={}", files.len());
}
