//! RAEA_GPU_SPLIT: a native frame's GPU path step by step, printed per frame at the end of a shot ("raea native: gpu
//! split"). Off, nothing is recorded or waited for.
//! - `RAEA_GPU_SPLIT=ts`: a GPU timestamp is written on the queue after each step (no wait inside the frame, so the GPU
//!   runs the frame as it would and keeps its clocks), and the frame's marks are read back at its end. A step's time is
//!   the GPU's progress between its mark and the one before: its own work plus whatever earlier work was still queued.
//! - `RAEA_GPU_SPLIT=1`: the GPU waited for after each step (its CPU time and GPU time apart, without overlap; an idle
//!   GPU between steps clocks down, so these read high).

use std::sync::Mutex;
use std::time::Instant;

static STEPS: Mutex<Vec<(&'static str, f64, f64, u32)>> = Mutex::new(Vec::new());

#[derive(PartialEq, Clone, Copy)]
enum Mode {
    Off,
    Wait,
    Stamps,
}

fn mode() -> Mode {
    static M: std::sync::OnceLock<Mode> = std::sync::OnceLock::new();
    *M.get_or_init(|| match std::env::var("RAEA_GPU_SPLIT").as_deref() {
        Ok("ts") => Mode::Stamps,
        Ok("0") | Err(_) => Mode::Off,
        Ok(_) => Mode::Wait,
    })
}

pub(crate) fn on() -> bool {
    mode() != Mode::Off
}

const MAX_MARKS: u32 = 64;

struct Stamps {
    set: wgpu::QuerySet,
    resolve: wgpu::Buffer,
    read: wgpu::Buffer,
    names: Vec<&'static str>,
    period: f32,
}

static STAMPS: Mutex<Option<Stamps>> = Mutex::new(None);

fn add(name: &'static str, cpu: f64, gpu: f64) {
    let mut s = STEPS.lock().unwrap();
    match s.iter_mut().find(|(k, ..)| *k == name) {
        Some(e) => {
            e.1 += cpu;
            e.2 += gpu;
            e.3 += 1;
        }
        None => s.push((name, cpu, gpu, 1)),
    }
}

fn stamp(name: &'static str, device: &wgpu::Device, queue: &wgpu::Queue) {
    let mut g = STAMPS.lock().unwrap();
    if g.is_none() {
        if !device.features().contains(wgpu::Features::TIMESTAMP_QUERY | wgpu::Features::TIMESTAMP_QUERY_INSIDE_ENCODERS) {
            return;
        }
        let set = device.create_query_set(&wgpu::QuerySetDescriptor { label: Some("raea gpu split"), ty: wgpu::QueryType::Timestamp, count: MAX_MARKS });
        let size = MAX_MARKS as u64 * 8;
        let resolve = device.create_buffer(&wgpu::BufferDescriptor { label: Some("raea gpu split resolve"), size, usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC, mapped_at_creation: false });
        let read = device.create_buffer(&wgpu::BufferDescriptor { label: Some("raea gpu split read"), size, usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ, mapped_at_creation: false });
        *g = Some(Stamps { set, resolve, read, names: Vec::new(), period: queue.get_timestamp_period() });
    }
    let st = g.as_mut().unwrap();
    if st.names.len() as u32 >= MAX_MARKS {
        return;
    }
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raea gpu split mark") });
    enc.write_timestamp(&st.set, st.names.len() as u32);
    queue.submit([enc.finish()]);
    st.names.push(name);
}

/// The frame begins (timestamp mode): a mark the steps are measured from.
pub(crate) fn begin(device: &wgpu::Device, queue: &wgpu::Queue) {
    if mode() == Mode::Stamps {
        if let Some(st) = STAMPS.lock().unwrap().as_mut() {
            st.names.clear();
        }
        stamp("begin", device, queue);
    }
}

/// The step `name`, begun at `t` on the CPU. Wait mode: its CPU time to now, then the GPU waited for (its GPU time).
/// Timestamp mode: its CPU time, and a mark on the queue. The clock for the next step starts when this returns.
pub(crate) fn step(name: &'static str, t: Instant, device: &wgpu::Device, queue: &wgpu::Queue) -> Instant {
    match mode() {
        Mode::Off => t,
        Mode::Wait => {
            let cpu = t.elapsed().as_secs_f64() * 1000.0;
            let w = Instant::now();
            let _ = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(60)) });
            add(name, cpu, w.elapsed().as_secs_f64() * 1000.0);
            Instant::now()
        }
        Mode::Stamps => {
            add(name, t.elapsed().as_secs_f64() * 1000.0, 0.0);
            stamp(name, device, queue);
            Instant::now()
        }
    }
}

/// The frame ends (timestamp mode): its marks resolved and read back (waiting for the GPU here, at the frame's end
/// only), each step's GPU time added.
pub(crate) fn end(device: &wgpu::Device, queue: &wgpu::Queue) {
    if mode() != Mode::Stamps {
        return;
    }
    let mut g = STAMPS.lock().unwrap();
    let Some(st) = g.as_mut() else { return };
    let n = st.names.len() as u32;
    if n < 2 {
        return;
    }
    let mut enc = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("raea gpu split resolve") });
    enc.resolve_query_set(&st.set, 0..n, &st.resolve, 0);
    enc.copy_buffer_to_buffer(&st.resolve, 0, &st.read, 0, n as u64 * 8);
    queue.submit([enc.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    st.read.slice(..n as u64 * 8).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    let _ = device.poll(wgpu::PollType::Wait { submission_index: None, timeout: Some(std::time::Duration::from_secs(60)) });
    if rx.recv().is_ok_and(|r| r.is_ok()) {
        let ts: Vec<u64> = {
            let d = st.read.slice(..n as u64 * 8).get_mapped_range().expect("mapped");
            d.chunks_exact(8).map(|c| u64::from_le_bytes(c.try_into().unwrap())).collect()
        };
        st.read.unmap();
        let names = std::mem::take(&mut st.names);
        drop(g);
        for i in 1..names.len() {
            let ms = ts[i].saturating_sub(ts[i - 1]) as f64 * STAMPS.lock().unwrap().as_ref().map_or(1.0, |s| s.period as f64) / 1e6;
            add(names[i], 0.0, ms);
        }
        add("frame (begin to last mark)", 0.0, ts[names.len() - 1].saturating_sub(ts[0]) as f64 * STAMPS.lock().unwrap().as_ref().map_or(1.0, |s| s.period as f64) / 1e6);
    }
}

/// The steps' times per drawn frame (`n` frames), and the record cleared.
pub(crate) fn report(n: usize) {
    if !on() {
        return;
    }
    let n = n.max(1) as f64;
    let mut s = STEPS.lock().unwrap();
    let line: Vec<String> = s.iter().map(|(k, c, g, calls)| format!("{k} cpu {:.2} gpu {:.2} (x{:.2})", c / n, g / n, *calls as f64 / n)).collect();
    eprintln!("raea native: gpu split per frame (ms): {}", line.join("; "));
    s.clear();
}
