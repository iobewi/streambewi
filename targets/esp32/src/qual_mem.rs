//! QUALIFICATION ONLY (never merge into firmware): heap statistics and cpu0 main-stack
//! high-water mark by painting. Covers the main stack (`_stack_end_cpu0` ..
//! `_stack_start_cpu0`), where the esp-rtos main task, the Embassy executor and
//! interrupts taken on it run. Radio task stacks live in the heap: not covered.
use core::sync::atomic::{AtomicUsize, Ordering};

unsafe extern "C" {
    static _stack_end_cpu0: u32;
    static _stack_start_cpu0: u32;
}

/// Lowest free bytes seen per heap region (index 0: main heap, 1: DRAM2), since boot and since the last probe.
const REGIONS: usize = 2;
static MIN_FREE: [AtomicUsize; REGIONS] = [const { AtomicUsize::new(usize::MAX) }; REGIONS];
static WINDOW_MIN_FREE: [AtomicUsize; REGIONS] = [const { AtomicUsize::new(usize::MAX) }; REGIONS];

/// Read-only sample of the per-region free bytes; called often so a short peak (a TLS handshake) is seen.
pub fn sample() {
    let h = esp_alloc::HEAP.stats();
    for (i, region) in h.region_stats.iter().flatten().take(REGIONS).enumerate() {
        MIN_FREE[i].fetch_min(region.free, Ordering::Relaxed);
        WINDOW_MIN_FREE[i].fetch_min(region.free, Ordering::Relaxed);
    }
}

const PATTERN: u32 = 0xA5A5_A5A5;
/// Leave the bottom alone: __stack_chk_guard sits at _stack_end + 60.
const SKIP: usize = 256;
/// Margin below the painter's own frame.
const MARGIN: usize = 1024;
static PAINT_TOP: AtomicUsize = AtomicUsize::new(0);

fn bounds() -> (usize, usize) {
    (
        core::ptr::addr_of!(_stack_end_cpu0) as usize,
        core::ptr::addr_of!(_stack_start_cpu0) as usize,
    )
}

/// Call first in main, before esp_hal::init.
#[inline(never)]
pub fn paint() {
    let marker = 0u32;
    let sp = core::ptr::addr_of!(marker) as usize;
    let (end, _) = bounds();
    let top = (sp - MARGIN) & !3;
    let mut p = end + SKIP;
    while p < top {
        unsafe { core::ptr::write_volatile(p as *mut u32, PATTERN) };
        p += 4;
    }
    PAINT_TOP.store(top, Ordering::Relaxed);
}

pub fn probe(target: &'static str, label: &str) {
    let (end, start) = bounds();
    let top = PAINT_TOP.load(Ordering::Relaxed);
    let lo = end + SKIP;
    let mut p = lo;
    while p < top && unsafe { core::ptr::read_volatile(p as *const u32) } == PATTERN {
        p += 4;
    }
    let reserved = start - end;
    let used = start - p;
    let note = if p == lo {
        " (paint exhausted: >= value)"
    } else {
        ""
    };
    let h = esp_alloc::HEAP.stats();
    for (i, region) in h.region_stats.iter().flatten().enumerate() {
        log::info!(
            target: target,
            "qual-mem {label}: region{i} size {} used {} free {} min_free {} window_min_free {}",
            region.size,
            region.used,
            region.free,
            MIN_FREE.get(i).map_or(0, |m| m.load(Ordering::Relaxed)),
            WINDOW_MIN_FREE
                .get(i)
                .map_or(0, |m| m.swap(usize::MAX, Ordering::Relaxed)),
        );
    }
    log::info!(
        target: target,
        "qual-mem {label}: heap size {} used {} free {} max_used {} min_free {}; \
         stack reserved {reserved} max_used {used} margin {}{note}; painted {} B",
        h.size,
        h.current_usage,
        esp_alloc::HEAP.free(),
        h.max_usage,
        h.size - h.max_usage,
        reserved - used,
        top - lo,
    );
}
