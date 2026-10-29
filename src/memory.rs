//! glibc's allocator, set for how Maia uses memory: worker threads decode pictures, read
//! folders and count sizes, and the frame keeps what they hand over and frees the rest.
//!
//! Left as it is, glibc gives each thread that allocates an arena of its own (up to eight
//! per core) and, after a large block is freed, raises the size it serves from `mmap` to
//! that block's, up to 32 MB. A decoded photo's buffer then comes from a worker's arena,
//! and once freed stays in it: a few such arenas held hundreds of megabytes that nothing
//! used. Here blocks of 256 KB and more always come from `mmap` (and go straight back when
//! freed), and what is freed in the arenas is handed back when Maia goes idle. The arenas
//! are left as many as glibc makes: capped at two or four, the frame's thread waited on
//! busy workers for the shared one, and the slowest frames took 2 to 4 ms longer.

/// Whether `tune` ran; `MAIA_MALLOC_DEFAULT` leaves glibc as it comes, to compare against.
static TUNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Set before any thread starts.
pub fn tune() {
    if std::env::var_os("MAIA_MALLOC_DEFAULT").is_some() {
        return;
    }
    TUNED.store(true, std::sync::atomic::Ordering::Relaxed);
    // A fixed threshold also stops glibc from raising it.
    unsafe { libc::mallopt(libc::M_MMAP_THRESHOLD, 256 * 1024) };
}

/// Free bytes in the arenas at the last trim.
static TRIMMED_AT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Called when nothing is due to be drawn: hand the arenas' free pages back to the system,
/// once at least 8 MB more lies free than after the last time. Counting is cheap; a trim
/// takes a few milliseconds and happens while nothing is on screen to wait for it.
pub fn idle() {
    use std::sync::atomic::Ordering::Relaxed;
    if !TUNED.load(Relaxed) {
        return;
    }
    let free = free_bytes();
    if free > TRIMMED_AT.load(Relaxed) + (8 << 20) {
        unsafe { libc::malloc_trim(0) };
        TRIMMED_AT.store(free_bytes(), Relaxed);
    } else if free < TRIMMED_AT.load(Relaxed) {
        // Reused since: count from here.
        TRIMMED_AT.store(free, Relaxed);
    }
}

/// Free bytes in the arenas. `mallinfo`, not `mallinfo2`: the release built for older
/// systems runs on a glibc before 2.33, which has only this one (its counters are 32 bits,
/// read here as unsigned, which holds up to 4 GB).
#[allow(deprecated)]
fn free_bytes() -> usize {
    unsafe { libc::mallinfo() }.fordblks as u32 as usize
}

/// Resident and peak memory of this process, in KB (test builds measure with it).
#[cfg(test)]
pub fn resident_kb() -> (u64, u64) {
    let s = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
    let field = |k: &str| s.lines().find_map(|l| l.strip_prefix(k)).and_then(|v| v.split_whitespace().next()?.parse().ok()).unwrap_or(0);
    (field("VmRSS:"), field("VmHWM:"))
}
