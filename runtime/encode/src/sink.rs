//! Where a folder render's frames go once drawn: PNG files (`frame-NNNNN.png`), written on threads of their own so the
//! next frame draws while the last one is encoded; or, with `RAEA_FRAME_PIPE=PATH`, sent in order and uncompressed to
//! PATH (a FIFO an encoder reads, or a file), with no file per frame.
//!
//! The pipe carries PAM images (P7, `TUPLTYPE RGB_ALPHA`, one after another): ffmpeg reads them with `-f pam_pipe` and
//! makes the same video, byte for byte, as from the PNGs (both decode to the same RGBA). A PNG of a 1080p frame was
//! 24 ms of CPU to write and about as much again for ffmpeg to read; the pipe is a copy. The render's EXPOSURE is
//! measured on the frames as they pass (the frames of_pngs would sample from the folder), since there are no files.
//!
//! Each frame is a [Frame]: its bytes owned, or held where they were made (a mapped readback buffer, given back when
//! written), so no copy is made on the draw's thread.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

type R<T> = Result<T, String>;

/// A drawn frame's pixels, straight RGBA8, row after row with no padding.
pub trait Frame: Send {
    /// `f` given the pixels (they may be borrowed from where they were made for as long as `f` runs)
    fn pixels(&self, f: &mut dyn FnMut(&[u8]) -> R<()>) -> R<()>;
}

impl Frame for Vec<u8> {
    fn pixels(&self, f: &mut dyn FnMut(&[u8]) -> R<()>) -> R<()> {
        f(self)
    }
}

/// The env var naming the pipe (or file) a folder render's frames go to instead of PNG files.
pub const PIPE_VAR: &str = "RAEA_FRAME_PIPE";

/// The pipe a folder render at `out` sends its frames to, if one is asked for (read at every render: a served process
/// takes each request's own environment).
pub fn pipe_asked() -> Option<PathBuf> {
    std::env::var_os(PIPE_VAR).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// The last piped render's exposure (of_pngs's result for the frames it sent), for the render's message.
static PIPED_EXPOSURE: Mutex<Option<R<(raea_sys_light_meter::exposure::Exposure, serde_json::Value)>>> = Mutex::new(None);

/// The exposure a piped render measured on its frames, taken (None when the last folder render wrote files).
pub fn take_piped_exposure() -> Option<R<(raea_sys_light_meter::exposure::Exposure, serde_json::Value)>> {
    PIPED_EXPOSURE.lock().unwrap().take()
}

/// A folder render's frames on their way out: `put` each as it's drawn (the `i` its file would be named by, in
/// order), then `finish`. Dropped unfinished (a render stopped by an error), the frames already put are still written.
pub struct Sink {
    tx: Option<SyncSender<(usize, Box<dyn Frame>)>>,
    threads: Vec<JoinHandle<R<()>>>,
    piped: bool,
    meter: Option<Arc<Mutex<Option<raea_sys_light_meter::exposure::Meter>>>>,
}

impl Sink {
    /// The frames of a render into the folder `out` (made already), `total` of them, `w` x `h`: PNG files on `writers`
    /// threads with `queue` frames waiting at most, or the pipe when one is asked for.
    pub fn open(out: &Path, w: u32, h: u32, total: usize, writers: usize, queue: usize) -> R<Sink> {
        *PIPED_EXPOSURE.lock().unwrap() = None;
        match pipe_asked() {
            Some(pipe) => Sink::piped(&pipe, w, h, total, queue),
            None => Ok(Sink::pngs(out, w, h, writers, queue)),
        }
    }

    fn pngs(out: &Path, w: u32, h: u32, writers: usize, queue: usize) -> Sink {
        let (tx, rx) = sync_channel::<(usize, Box<dyn Frame>)>(queue);
        let rx = Arc::new(Mutex::new(rx));
        let threads = (0..writers.max(1))
            .map(|_| {
                let (rx, out) = (rx.clone(), out.to_path_buf());
                std::thread::Builder::new()
                    .name("raea frame writer".into())
                    .spawn(move || -> R<()> {
                        loop {
                            let Ok((i, frame)) = rx.lock().unwrap().recv() else { return Ok(()) };
                            let dest = out.join(format!("frame-{i:05}.png"));
                            frame.pixels(&mut |px| raea_render::save_png(&dest, px, w, h))?;
                        }
                    })
                    .expect("a frame writer's thread")
            })
            .collect();
        Sink { tx: Some(tx), threads, piped: false, meter: None }
    }

    fn piped(pipe: &Path, w: u32, h: u32, total: usize, queue: usize) -> R<Sink> {
        // appended to: a FIFO's reader sees one stream however many renders write into it, and a file grows
        let file = std::fs::OpenOptions::new().append(true).create(true).open(pipe).map_err(|e| format!("{PIPE_VAR} {}: {e}", pipe.display()))?;
        let (tx, rx) = sync_channel::<(usize, Box<dyn Frame>)>(queue);
        let meter = Arc::new(Mutex::new(Some(raea_sys_light_meter::exposure::Meter::new(total))));
        let m = meter.clone();
        let pipe = pipe.to_path_buf();
        let t = std::thread::Builder::new().name("raea frame pipe".into()).spawn(move || send_all(file, &pipe, rx, w, h, &m)).map_err(|e| e.to_string())?;
        Ok(Sink { tx: Some(tx), threads: vec![t], piped: true, meter: Some(meter) })
    }

    /// Whether the frames go to a pipe (no files are written).
    pub fn piped_out(&self) -> bool {
        self.piped
    }

    /// The `i`th frame (its file's number), drawn; waits while `queue` frames are already waiting.
    pub fn put(&self, i: usize, frame: Box<dyn Frame>) -> R<()> {
        match self.tx.as_ref().map(|tx| tx.send((i, frame))) {
            Some(Ok(())) => Ok(()),
            // the writer stopped: its own error says why
            _ => Err("the frame writer stopped".into()),
        }
    }

    /// Every frame put written (or sent, and the pipe closed by this render), and the first error any writer met.
    pub fn finish(mut self) -> R<()> {
        self.join()
    }

    fn join(&mut self) -> R<()> {
        drop(self.tx.take());
        let mut first = Ok(());
        for t in self.threads.drain(..) {
            let r = t.join().map_err(|_| "a frame writer panicked".to_string()).and_then(|r| r);
            if first.is_ok() {
                first = r;
            }
        }
        if let Some(m) = self.meter.take().and_then(|m| m.lock().unwrap().take()) {
            *PIPED_EXPOSURE.lock().unwrap() = Some(m.finish());
        }
        first
    }
}

impl Drop for Sink {
    fn drop(&mut self) {
        let _ = self.join();
    }
}

/// The pipe's thread: each frame as one PAM image, in the order put (the draw's order), checked; each frame of_pngs
/// would sample measured once it's sent.
fn send_all(mut file: std::fs::File, pipe: &Path, rx: Receiver<(usize, Box<dyn Frame>)>, w: u32, h: u32, meter: &Mutex<Option<raea_sys_light_meter::exposure::Meter>>) -> R<()> {
    let head = format!("P7\nWIDTH {w}\nHEIGHT {h}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n");
    let size = w as usize * h as usize * 4;
    let mut last: Option<usize> = None;
    for (n, (i, frame)) in rx.into_iter().enumerate() {
        if last.is_some_and(|l| i <= l) {
            return Err(format!("{PIPE_VAR}: frame {i} came after frame {}; a pipe takes frames in order", last.unwrap_or(0)));
        }
        last = Some(i);
        frame.pixels(&mut |px| {
            if px.len() != size {
                return Err(format!("{PIPE_VAR}: frame {i} has {} bytes, not {w}x{h}x4", px.len()));
            }
            // header and pixels in one write where the platform allows (a reader never sees a header alone)
            file.write_all(head.as_bytes()).and_then(|()| file.write_all(px)).map_err(|e| format!("{PIPE_VAR} {}: frame {i}: {e}", pipe.display()))?;
            let mut m = meter.lock().unwrap();
            if let Some(m) = m.as_mut().filter(|m| m.wants(n)) {
                m.add(&format!("frame-{i:05}.png"), px);
            }
            Ok(())
        })?;
    }
    file.flush().map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frames(n: usize, w: u32, h: u32) -> Vec<Vec<u8>> {
        (0..n).map(|f| (0..w * h).flat_map(|p| [(f * 7) as u8, (p % 251) as u8, 200, 255]).collect()).collect()
    }

    #[test]
    fn pngs_and_pipe_carry_the_same_pixels() {
        // one test, not two: the pipe is chosen by an env var, which tests running at once would share
        let dir = std::env::temp_dir().join(format!("raea-sink-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let (w, h) = (6u32, 4u32);
        let fs = frames(30, w, h);
        // PNG files
        let s = Sink::open(&dir, w, h, fs.len(), 2, 4).unwrap();
        assert!(!s.piped_out());
        for (i, px) in fs.iter().enumerate() {
            s.put(i, Box::new(px.clone())).unwrap();
        }
        s.finish().unwrap();
        assert!(take_piped_exposure().is_none());
        let files: Vec<PathBuf> = (0..fs.len()).map(|i| dir.join(format!("frame-{i:05}.png"))).collect();
        for (f, px) in files.iter().zip(&fs) {
            assert_eq!(&raea_render::image::open(f).unwrap().to_rgba8().into_raw(), px);
        }
        let from_pngs = raea_sys_light_meter::exposure::of_pngs(&files).unwrap();
        // the pipe: PAM images in order, nothing written to the folder, the same exposure as the files'
        let pipe = dir.join("frames.pam");
        unsafe { std::env::set_var(PIPE_VAR, &pipe) };
        let out = dir.join("piped");
        std::fs::create_dir_all(&out).unwrap();
        let s = Sink::open(&out, w, h, fs.len(), 2, 4).unwrap();
        assert!(s.piped_out());
        for (i, px) in fs.iter().enumerate() {
            s.put(i, Box::new(px.clone())).unwrap();
        }
        s.finish().unwrap();
        unsafe { std::env::remove_var(PIPE_VAR) };
        assert_eq!(std::fs::read_dir(&out).unwrap().count(), 0);
        let bytes = std::fs::read(&pipe).unwrap();
        let head = format!("P7\nWIDTH {w}\nHEIGHT {h}\nDEPTH 4\nMAXVAL 255\nTUPLTYPE RGB_ALPHA\nENDHDR\n");
        let want: Vec<u8> = fs.iter().flat_map(|px| head.as_bytes().iter().chain(px.iter()).copied()).collect();
        assert_eq!(bytes, want);
        assert_eq!(take_piped_exposure().unwrap().unwrap(), from_pngs);
        // out of order is refused, not written out of order
        unsafe { std::env::set_var(PIPE_VAR, dir.join("bad.pam")) };
        let s = Sink::open(&out, w, h, 2, 1, 4).unwrap();
        unsafe { std::env::remove_var(PIPE_VAR) };
        s.put(1, Box::new(fs[1].clone())).unwrap();
        let _ = s.put(0, Box::new(fs[0].clone()));
        assert!(s.finish().unwrap_err().contains("in order"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
