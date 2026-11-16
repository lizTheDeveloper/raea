//! Work across this process's share of the cores, never failing when the system won't give it a thread.
//!
//! A render queue's member runs several raea processes at once (its lanes) on a share of a big host, and each sized
//! its threads by the host (`available_parallelism`, 96 on a 48-core EPYC where the container had 13.7). Under a
//! member's memory and thread limits, a spawn failed ("failed to spawn thread: Resource temporarily unavailable",
//! "failed to allocate an alternative stack: Cannot allocate memory") and std's scoped spawn panics on that, so the
//! process aborted (exit 134) mid-batch, read in rq's log as a stack overflow (Ch1 and Ch5 batches, 2026-11-16).

/// This process's share of the cores: RAEA_THREADS, else the host's threads over the raea processes running here
/// (its lanes, which start together), at least 1. Read once a process.
pub fn share() -> usize {
    static SHARE: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *SHARE.get_or_init(|| {
        if let Some(n) = std::env::var("RAEA_THREADS").ok().and_then(|v| v.parse::<usize>().ok()).filter(|n| *n > 0) {
            return n;
        }
        (std::thread::available_parallelism().map_or(4, |n| n.get()) / raea_processes()).max(1)
    })
}

/// raea processes running on this machine now, this one included (1 where /proc can't be read).
pub fn raea_processes() -> usize {
    let Ok(d) = std::fs::read_dir("/proc") else { return 1 };
    d.flatten()
        .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
        .filter(|e| std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim().starts_with("raea")))
        .count()
        .max(1)
}

/// `f(i)` for every `i` in `0..items`, on up to `cap` threads of this process's share, the caller's thread one of
/// them. A thread the system refuses is left out (the others, and the caller, take its items): this never panics for
/// want of a thread, and every item is done once.
pub fn each(items: usize, cap: usize, f: impl Fn(usize) + Sync) {
    if items == 0 {
        return;
    }
    let lanes = share().min(cap).min(items).max(1);
    let next = std::sync::atomic::AtomicUsize::new(0);
    let work = || loop {
        let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if i >= items {
            break;
        }
        f(i);
    };
    if lanes == 1 {
        work();
        return;
    }
    std::thread::scope(|s| {
        for _ in 1..lanes {
            if std::thread::Builder::new().spawn_scoped(s, &work).is_err() {
                break;
            }
        }
        work();
    });
}

/// `each` collecting each item's result in order.
pub fn map<T: Send>(items: usize, cap: usize, f: impl Fn(usize) -> T + Sync) -> Vec<T> {
    let slots: Vec<std::sync::Mutex<Option<T>>> = (0..items).map(|_| std::sync::Mutex::new(None)).collect();
    each(items, cap, |i| *slots[i].lock().unwrap() = Some(f(i)));
    slots.into_iter().map(|s| s.into_inner().unwrap().expect("par::each did every item")).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn every_item_once_in_order() {
        let v = super::map(1000, 16, |i| i * 2);
        assert_eq!(v, (0..1000).map(|i| i * 2).collect::<Vec<_>>());
        assert!(super::map(0, 4, |i| i).is_empty());
    }
}
