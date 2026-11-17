//! `track.at`: one frame of a shot's per-body tracks from an RTK1 sidecar (Lane E for Lane H, 2026-11-17), so cloth,
//! unpen and other per-body bakes never enter a composition's JS bundle and a runtime never holds a whole file in JS.
//! The format (design/engine-native-coverage.md §2b), little-endian:
//!   "RTK1", u32 header_len, header (UTF-8 JSON, padded so the data starts 8-byte aligned):
//!     {"fps", "frame0", "frames", "dtype": "f64"|"f32", "bodies": [{"name", "channels": [...], "offset"}]}
//!   data: each body's frames x channels values, frame-major, `offset` bytes from the data section's start.
//! A frame outside [frame0, frame0 + frames) holds the nearest stored one. Files are read once per process and kept
//! by path, size and modification time; every offset and length is checked against the file.
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value, json};

pub(crate) struct Body {
    name: String,
    channels: usize,
    /// index of its first value in `values`
    first: usize,
}

pub(crate) struct Track {
    fps: f64,
    frame0: i64,
    frames: usize,
    bodies: Vec<Body>,
    values: Vec<f64>,
}

type Cached = (u64, Option<std::time::SystemTime>, Arc<Track>);
static CACHE: Mutex<Option<HashMap<PathBuf, Cached>>> = Mutex::new(None);

/// An RTK1 file's bytes parsed, every offset and length checked.
pub(crate) fn parse(b: &[u8]) -> Result<Track, String> {
    if b.len() < 8 || &b[..4] != b"RTK1" {
        return Err("not an RTK1 track file".into());
    }
    let hl = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    let data_at = 8usize.checked_add(hl).filter(|&e| e <= b.len()).ok_or("its header runs past its end")?;
    let h: Value = serde_json::from_slice(&b[8..data_at]).map_err(|e| format!("its header: {e}"))?;
    let el = match h["dtype"].as_str() {
        Some("f64") => 8,
        Some("f32") => 4,
        other => return Err(format!("dtype {other:?}: f64 or f32")),
    };
    let frames = h["frames"].as_u64().filter(|&n| n > 0).ok_or("frames: a count above 0")? as usize;
    let frame0 = h["frame0"].as_i64().ok_or("frame0: a frame")?;
    let fps = h["fps"].as_f64().unwrap_or(30.0);
    let data = &b[data_at..];
    let mut bodies = Vec::new();
    let mut values = Vec::new();
    for x in h["bodies"].as_array().ok_or("bodies: a list")? {
        let name = x["name"].as_str().ok_or("a body without a name")?.to_string();
        let channels = x["channels"].as_array().map(Vec::len).filter(|&n| n > 0).ok_or_else(|| format!("{name}: channels"))?;
        let off = x["offset"].as_u64().ok_or_else(|| format!("{name}: offset"))? as usize;
        let n = frames.checked_mul(channels).ok_or_else(|| format!("{name}: too many values"))?;
        let end = n.checked_mul(el).and_then(|len| off.checked_add(len)).filter(|&e| e <= data.len()).ok_or_else(|| format!("{name}: its values run past the file's end"))?;
        let first = values.len();
        values.reserve(n);
        for c in data[off..end].chunks_exact(el) {
            values.push(if el == 8 { f64::from_le_bytes(c.try_into().unwrap()) } else { f32::from_le_bytes(c.try_into().unwrap()) as f64 });
        }
        bodies.push(Body { name, channels, first });
    }
    Ok(Track { fps, frame0, frames, bodies, values })
}

/// The track file at `path`, read once and kept while its size and modification time hold.
pub(crate) fn load(path: &Path) -> Result<Arc<Track>, String> {
    let md = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let (len, modified) = (md.len(), md.modified().ok());
    let mut g = CACHE.lock().unwrap();
    let cache = g.get_or_insert_with(HashMap::new);
    if let Some((l, m, t)) = cache.get(path) {
        if *l == len && *m == modified {
            return Ok(t.clone());
        }
    }
    let t = Arc::new(parse(&std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?).map_err(|e| format!("{}: {e}", path.display()))?);
    if cache.len() >= 256 {
        cache.clear();
    }
    cache.insert(path.to_path_buf(), (len, modified, t.clone()));
    Ok(t)
}

/// Frame `f`'s values for each body (or the ones named), the frame held at the track's ends.
pub(crate) fn at(t: &Track, f: i64, only: Option<&[String]>) -> Value {
    let i = (f - t.frame0).clamp(0, t.frames as i64 - 1) as usize;
    let mut out = Map::new();
    for b in t.bodies.iter().filter(|b| only.is_none_or(|o| o.iter().any(|n| *n == b.name))) {
        let s = b.first + i * b.channels;
        out.insert(b.name.clone(), Value::Array(t.values[s..s + b.channels].iter().map(|v| json!(v)).collect()));
    }
    json!({"fps": t.fps, "frame0": t.frame0, "frames": t.frames, "frame": t.frame0 + i as i64, "bodies": out})
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An RTK1 file: bodies as (name, channels, values frame-major).
    fn rtk1(dtype: &str, frame0: i64, frames: usize, bodies: &[(&str, usize, Vec<f64>)]) -> Vec<u8> {
        let el = if dtype == "f64" { 8 } else { 4 };
        let mut data = Vec::new();
        let mut hb = Vec::new();
        for (name, ch, vals) in bodies {
            hb.push(json!({"name": name, "channels": (0..*ch).map(|c| format!("c{c}")).collect::<Vec<_>>(), "offset": data.len()}));
            for v in vals {
                if el == 8 { data.extend_from_slice(&v.to_le_bytes()) } else { data.extend_from_slice(&(*v as f32).to_le_bytes()) }
            }
        }
        let mut head = json!({"fps": 30, "frame0": frame0, "frames": frames, "dtype": dtype, "bodies": hb}).to_string().into_bytes();
        while (8 + head.len()) % 8 != 0 {
            head.push(b' ');
        }
        let mut out = b"RTK1".to_vec();
        out.extend_from_slice(&(head.len() as u32).to_le_bytes());
        out.extend_from_slice(&head);
        out.extend_from_slice(&data);
        out
    }

    #[test]
    fn frames_read_back_exactly_and_hold_at_the_ends() {
        let f = rtk1("f64", 100, 3, &[("noys", 2, vec![0.1, 0.2, 1.1, 1.2, 2.1, 2.2]), ("child", 1, vec![7.0, 8.0, 9.0])]);
        let t = parse(&f).unwrap();
        assert_eq!(at(&t, 101, None)["bodies"]["noys"], json!([1.1, 1.2]));
        assert_eq!(at(&t, 99, None)["bodies"]["child"], json!([7.0]));
        assert_eq!(at(&t, 500, None)["bodies"]["child"], json!([9.0]));
        assert_eq!(at(&t, 102, Some(&["child".to_string()]))["bodies"].as_object().unwrap().len(), 1);
        let t32 = parse(&rtk1("f32", 0, 1, &[("a", 1, vec![0.5])])).unwrap();
        assert_eq!(at(&t32, 0, None)["bodies"]["a"], json!([0.5]));
    }

    #[test]
    fn a_damaged_file_is_refused() {
        let good = rtk1("f64", 0, 2, &[("a", 2, vec![1.0, 2.0, 3.0, 4.0])]);
        assert!(parse(&good[..good.len() - 1]).is_err(), "values cut short");
        assert!(parse(&good[..10]).is_err(), "header cut short");
        assert!(parse(b"RTK2xxxx").is_err());
        let mut lie = good.clone();
        lie[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse(&lie).is_err(), "a header longer than the file");
        assert!(parse(&rtk1("f16", 0, 1, &[("a", 1, vec![1.0])])).is_err());
    }
}
