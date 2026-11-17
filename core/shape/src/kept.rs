//! Solids kept on disk between runs. An expression's mesh is the same every time this build makes it, and making it
//! is the cost: the boolean cuts of a furnished set were a quarter of the seconds its scene took to open, every time
//! it opened (a launch-film set, 11-10). A kept mesh is named by the expression and by the source that makes meshes
//! (build.rs RAEA_SHAPE_SRC_HASH over core/ and Cargo.lock; RAEA_SHAPE_KEY=binary: the running program's size and
//! time, as before), and read back as the bytes that were written, so what is drawn from it is what would have been
//! made. RAEA_SHAPE_CACHE=0 turns it off, =DIR moves it from raea's caches folder (shapes/ in it).
//!
//! Bounded (Lane E S50: keyed by the executable and never evicted, one bench member's folder grew 8.7 GB in 4 h, 42
//! files a second, as each new binary wrote every solid again): the folder is held under RAEA_SHAPE_CACHE_MAX_MB
//! (2048), oldest first, when a process first uses it; a process writes at most 4096 solids and 512 MB; lib.rs's
//! `cached` writes a solid only once it's asked for twice, or took 50 ms or more to make (a one-off expression made
//! every frame never reaches the disk).
//!
//! Checked (security review): a file is named by a blake3 of the length-prefixed expression and the made_by() tag
//! (`key`), and carries the whole key and a blake3 of its mesh in its header; `read` refuses a file kept for another
//! expression or whose mesh doesn't match (a miss, made again, counted in REFUSED and said once).

use crate::Tris;
use std::path::PathBuf;

/// raea's caches folder, as raea-io-home names it (this crate is below it): ~/Library/Caches/raea on macOS,
/// %LOCALAPPDATA%\raea\cache on Windows, $XDG_CACHE_HOME/raea or ~/.cache/raea elsewhere.
fn caches() -> Option<PathBuf> {
    let base = PathBuf::from(std::env::var_os(if cfg!(windows) { "LOCALAPPDATA" } else { "HOME" })?);
    if base.as_os_str().is_empty() {
        return None;
    }
    if cfg!(windows) {
        return Some(base.join("raea").join("cache"));
    }
    if cfg!(target_os = "macos") {
        return Some(base.join("Library/Caches/raea"));
    }
    let xdg = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from);
    Some(match xdg {
        Some(d) if d.is_absolute() => d,
        _ => base.join(".cache"),
    }
    .join("raea"))
}

fn this_build() -> (u64, u128) {
    static BUILD: std::sync::OnceLock<(u64, u128)> = std::sync::OnceLock::new();
    *BUILD.get_or_init(|| {
        let m = std::env::current_exe().ok().and_then(|p| std::fs::metadata(p).ok());
        let t = m.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).unwrap_or(0);
        (m.map(|m| m.len()).unwrap_or(0), t)
    })
}

/// The kept-solids folder, or None when nothing is kept (off, or no caches folder here). Held under its size cap the
/// first time a process asks (prune).
fn root() -> Option<PathBuf> {
    static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let root = match std::env::var("RAEA_SHAPE_CACHE") {
            Ok(v) if v == "0" => return None,
            Ok(v) if !v.is_empty() => PathBuf::from(v),
            _ => caches()?.join("shapes"),
        };
        let cap = std::env::var("RAEA_SHAPE_CACHE_MAX_MB").ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(2048) << 20;
        prune(&root, cap);
        Some(root)
    })
    .clone()
}

/// The folder's kept solids held under `cap` bytes: past it, the oldest go (by modification time; a read touches
/// nothing, so a solid in use is the one written last) until it's under three quarters of the cap. Said once.
pub fn prune(root: &std::path::Path, cap: u64) -> (usize, u64, usize) {
    let Ok(rd) = std::fs::read_dir(root) else { return (0, 0, 0) };
    let mut files: Vec<(std::time::SystemTime, u64, PathBuf)> = rd
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().ends_with(".tris"))
        .filter_map(|e| {
            let m = e.metadata().ok()?;
            Some((m.modified().unwrap_or(std::time::UNIX_EPOCH), m.len(), e.path()))
        })
        .collect();
    let mut total: u64 = files.iter().map(|f| f.1).sum();
    let (n, mut gone) = (files.len(), 0);
    if total > cap {
        files.sort_by_key(|f| f.0);
        for (_, len, p) in &files {
            if total <= cap / 4 * 3 {
                break;
            }
            if std::fs::remove_file(p).is_ok() {
                total = total.saturating_sub(*len);
                gone += 1;
            }
        }
        eprintln!("raea: kept solids: {n} in {} ({} MB over the {} MB cap): {gone} oldest let go, {} MB left", root.display(), (files.iter().map(|f| f.1).sum::<u64>()) >> 20, cap >> 20, total >> 20);
    }
    (n, total, gone)
}

/// What a kept solid is named by besides its expression: the source that makes meshes, or the executable.
fn made_by() -> Vec<u8> {
    if std::env::var("RAEA_SHAPE_KEY").as_deref() == Ok("binary") {
        let (len, time) = this_build();
        return [len.to_le_bytes().as_slice(), time.to_le_bytes().as_slice()].concat();
    }
    format!("src:{}", env!("RAEA_SHAPE_SRC_HASH")).into_bytes()
}

/// The key a kept solid is named and checked by: a blake3 over the length-prefixed expression and the length-prefixed
/// made_by() tag (security review: an FNV-1a 64 name could collide, by accident or by design, and serve the wrong mesh
/// silently). The file is named by its first 16 bytes and carries all 32 in its header, which `read` checks.
pub fn key(expr: &str) -> [u8; 32] {
    let by = made_by();
    let mut h = blake3::Hasher::new();
    h.update(b"raea-tris-2\0");
    h.update(&(expr.len() as u64).to_le_bytes());
    h.update(expr.as_bytes());
    h.update(&(by.len() as u64).to_le_bytes());
    h.update(&by);
    *h.finalize().as_bytes()
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Where the mesh for `expr` is kept, or None when nothing is (off, or no caches folder here).
pub fn place(expr: &str) -> Option<PathBuf> {
    Some(root()?.join(format!("{}.tris", hex(&key(expr)[..16]))))
}

/// Solids and bytes this process has written; past 4096 or 512 MB it writes no more (said once): a scene whose
/// solids change every frame fills no folder.
static WROTE: std::sync::Mutex<(usize, u64, bool)> = std::sync::Mutex::new((0, 0, false));

/// "raeatri2", the expression's key (32 bytes, `key`), a blake3 of the mesh bytes (32), the vertex count (u64), then
/// the positions and the normals (f32 x 3 each). raeatri1 files (no key) are never read: a miss, made again.
const MAGIC: &[u8; 8] = b"raeatri2";
const HEAD: usize = 8 + 32 + 32 + 8;

/// Kept solids refused on read because their key or bytes didn't match (a name collision, a damaged or planted file):
/// each a miss, made again; the first is said.
pub static REFUSED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn refused(p: &std::path::Path, why: &str) {
    if REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed) == 0 {
        eprintln!("raea: kept solids: {} refused ({why}); made again (further refusals counted in raea_core_shape::kept::REFUSED)", p.display());
    }
}

/// The mesh kept at `p` for `expr`: None when there is none, it isn't whole (a write that was cut short), or it was
/// kept for another expression or its bytes don't match their hash (refused, counted).
pub fn read(p: &std::path::Path, expr: &str) -> Option<Tris> {
    let b = std::fs::read(p).ok()?;
    if b.len() < HEAD || &b[..8] != MAGIC {
        return None;
    }
    if b[8..40] != key(expr) {
        refused(p, "kept for another expression");
        return None;
    }
    let n = u64::from_le_bytes(b[72..80].try_into().ok()?) as usize;
    if n.checked_mul(24).and_then(|x| x.checked_add(HEAD)) != Some(b.len()) {
        return None;
    }
    if b[40..72] != *blake3::hash(&b[HEAD..]).as_bytes() {
        refused(p, "its mesh doesn't match its hash");
        return None;
    }
    let f = |at: usize| f32::from_le_bytes(b[at..at + 4].try_into().unwrap());
    let run = |from: usize| (0..n).map(|i| [f(from + i * 12), f(from + i * 12 + 4), f(from + i * 12 + 8)]).collect::<Vec<_>>();
    Some(Tris { positions: run(HEAD), normals: run(HEAD + n * 12) })
}

/// `t`, the mesh of `expr`, kept at `p`: written beside it and moved into place so a reader never sees half of it.
/// Failing is fine: the mesh is made again next time.
pub fn write(p: &std::path::Path, expr: &str, t: &Tris) {
    if t.positions.len() != t.normals.len() {
        return;
    }
    let mut body = Vec::with_capacity(t.positions.len() * 24);
    for v in t.positions.iter().chain(&t.normals) {
        for x in v {
            body.extend_from_slice(&x.to_le_bytes());
        }
    }
    let mut b = Vec::with_capacity(HEAD + body.len());
    b.extend_from_slice(MAGIC);
    b.extend_from_slice(&key(expr));
    b.extend_from_slice(blake3::hash(&body).as_bytes());
    b.extend_from_slice(&(t.positions.len() as u64).to_le_bytes());
    b.extend_from_slice(&body);
    {
        let mut w = WROTE.lock().unwrap();
        if w.0 >= 4096 || w.1 + b.len() as u64 > 512 << 20 {
            if !w.2 {
                w.2 = true;
                eprintln!("raea: kept solids: this process wrote {} ({} MB); no more this run", w.0, w.1 >> 20);
            }
            return;
        }
        w.0 += 1;
        w.1 += b.len() as u64;
    }
    let Some(dir) = p.parent() else { return };
    let part = dir.join(format!(".{}.{}.part", p.file_name().unwrap_or_default().to_string_lossy(), std::process::id()));
    if std::fs::create_dir_all(dir).is_ok() && std::fs::write(&part, b).is_ok() && std::fs::rename(&part, p).is_err() {
        let _ = std::fs::remove_file(&part);
    }
}
