use std::ops::Range;
use std::sync::LazyLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

// A search job hands its worker back after this long so requests interleave.
const SEARCH_SLICE: Duration = Duration::from_millis(10);

/// Dedicated thread pool for background work (scan, warmup, bigram build).
pub static BACKGROUND_THREAD_POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    let total = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4);

    // Background work is mostly syscall-bound; halving parallelism leaves
    // cores for search/UI at negligible throughput cost.
    let bg_threads = (total / 2).max(2);
    rayon::ThreadPoolBuilder::new()
        .num_threads(bg_threads)
        .thread_name(|i| format!("fff-bg-{i}"))
        .start_handler(|_| pin_user_initiated())
        .build()
        .expect("failed to create background rayon pool")
});

/// Search pool sized to non-efficiency cores on macOS and full parallelism elsewhere.
pub static SEARCH_THREAD_POOL: LazyLock<rayon::ThreadPool> = LazyLock::new(|| {
    #[cfg(target_os = "macos")]
    let threads = non_efficiency_core_count();
    #[cfg(not(target_os = "macos"))]
    let threads = std::thread::available_parallelism()
        .map(|p| p.get())
        .unwrap_or(4);

    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .thread_name(|i| format!("fff-search-{i}"))
        .start_handler(|_| pin_user_initiated())
        .build()
        .expect("failed to create search rayon pool")
});

/// Splits one request's work into ordered blocks run on [`SEARCH_THREAD_POOL`].
///
/// The calling thread coordinates outside the pool and works on its own blocks;
/// jobs never nest, so a request waits only for its own blocks. A job hands its
/// worker back after [`SEARCH_SLICE`], and the next wave queues behind requests
/// that arrived meanwhile.
#[derive(Clone, Copy)]
pub(crate) struct SearchBlocks {
    serial_below: usize,
    max_len: usize,
    max_jobs: usize,
}

impl SearchBlocks {
    // Inputs shorter than `serial_below` stay on the caller; blocks hold at most `max_len` items.
    pub(crate) const fn new(serial_below: usize, max_len: usize) -> Self {
        Self {
            serial_below,
            max_len,
            max_jobs: usize::MAX,
        }
    }

    // 0 and 1 both keep the work on the calling thread.
    pub(crate) const fn max_jobs(self, max_jobs: usize) -> Self {
        Self { max_jobs, ..self }
    }

    pub(crate) fn filter_map<'a, T: Sync, S: Send, R: Send>(
        self,
        items: &'a [T],
        init: impl Fn() -> S + Sync,
        map: impl Fn(&mut S, usize, &'a T) -> Option<R> + Sync,
    ) -> Vec<R> {
        let mut out = Vec::new();
        self.extend_filter_map(&mut out, items, init, map);
        out
    }

    pub(crate) fn extend_filter_map<'a, T: Sync, S: Send, R: Send>(
        self,
        out: &mut Vec<R>,
        items: &'a [T],
        init: impl Fn() -> S + Sync,
        map: impl Fn(&mut S, usize, &'a T) -> Option<R> + Sync,
    ) {
        self.extend_in(
            &SEARCH_THREAD_POOL,
            out,
            items.len(),
            init,
            |state, range, out| {
                for index in range {
                    if let Some(result) = map(state, index, &items[index]) {
                        out.push(result);
                    }
                }
            },
        );
    }

    /// Runs `run` over consecutive index ranges covering `0..len` and returns
    /// the outputs concatenated in index order.
    pub(crate) fn map_ranges<S: Send, R: Send>(
        self,
        len: usize,
        init: impl Fn() -> S + Sync,
        run: impl Fn(&mut S, Range<usize>, &mut Vec<R>) + Sync,
    ) -> Vec<R> {
        let mut out = Vec::new();
        self.extend_in(&SEARCH_THREAD_POOL, &mut out, len, init, run);
        out
    }

    fn extend_in<S: Send, R: Send>(
        self,
        pool: &rayon::ThreadPool,
        out: &mut Vec<R>,
        len: usize,
        init: impl Fn() -> S + Sync,
        run: impl Fn(&mut S, Range<usize>, &mut Vec<R>) + Sync,
    ) {
        let jobs = pool.current_num_threads().min(self.max_jobs).max(1);
        // About 4 blocks per job lets jobs that start early or hit light ranges take more.
        let block_len = len.div_ceil(jobs * 4).clamp(1, self.max_len.max(1));
        let blocks = len.div_ceil(block_len);
        if jobs == 1 || blocks <= 1 || len < self.serial_below {
            if len > 0 {
                run(&mut init(), 0..len, out);
            }
            return;
        }

        // A worker waiting on a scope runs unrelated jobs, which is the coupling this avoids.
        debug_assert!(
            rayon::current_thread_index().is_none(),
            "search work must be coordinated outside Rayon pools"
        );
        let next = AtomicUsize::new(0);
        // Claims blocks until none remain or the slice ends; each call covers at least one.
        let work = |slot: &mut Slot<S, R>| {
            let Slot { state, output } = slot;
            let started = Instant::now();
            loop {
                let block = next.fetch_add(1, Ordering::Relaxed);
                if block >= blocks {
                    break;
                }
                let start = block * block_len;
                let mut segment = Vec::new();
                run(
                    state.get_or_insert_with(&init),
                    start..(start + block_len).min(len),
                    &mut segment,
                );
                if !segment.is_empty() {
                    output.push((block, segment));
                }
                if started.elapsed() >= SEARCH_SLICE {
                    break;
                }
            }
        };

        // The caller takes a share of every wave, so it starts at once and keeps
        // progressing while other requests occupy the pool.
        let mut slots: Vec<Slot<S, R>> = (0..jobs.min(blocks))
            .map(|_| Slot {
                state: None,
                output: Vec::new(),
            })
            .collect();
        let mut segments: Vec<(usize, Vec<R>)> = Vec::new();
        loop {
            let remaining = blocks.saturating_sub(next.load(Ordering::Relaxed));
            if remaining == 0 {
                break;
            }
            let wave = &mut slots[..remaining.min(jobs)];
            let (own, spawned) = wave.split_first_mut().unwrap();
            pool.in_place_scope_fifo(|scope| {
                for slot in spawned {
                    let work = &work;
                    scope.spawn_fifo(move |_| work(slot));
                }
                work(own);
            });
            for slot in wave {
                segments.append(&mut slot.output);
            }
        }

        segments.sort_unstable_by_key(|&(block, _)| block);
        out.reserve(segments.iter().map(|(_, segment)| segment.len()).sum());
        for (_, segment) in segments {
            out.extend(segment);
        }
    }
}

/// Runs `run` over consecutive ranges covering `0..len` with the dispatch core searches
/// use, and returns the outputs concatenated in range order. `init` creates the state a job
/// keeps across its ranges. Call it outside Rayon pools.
pub fn map_search_ranges<S: Send, R: Send>(
    len: usize,
    init: impl Fn() -> S + Sync,
    run: impl Fn(&mut S, Range<usize>, &mut Vec<R>) + Sync,
) -> Vec<R> {
    const BLOCKS: SearchBlocks = SearchBlocks::new(2048, 16_384);
    BLOCKS.map_ranges(len, init, run)
}

struct Slot<S, R> {
    state: Option<S>,
    output: Vec<(usize, Vec<R>)>,
}

static FULL_PARALLEL_INDEXING: AtomicBool = AtomicBool::new(false);

/// Index on the search pool (all cores, performance cores on macOS) instead of half the
/// cores. Faster on large repos, but searches queue behind indexing. Applies to later scans.
pub fn set_full_parallel_indexing(enabled: bool) {
    FULL_PARALLEL_INDEXING.store(enabled, Ordering::Relaxed);
}

pub(crate) fn indexing_pool() -> &'static rayon::ThreadPool {
    if FULL_PARALLEL_INDEXING.load(Ordering::Relaxed) {
        &SEARCH_THREAD_POOL
    } else {
        &BACKGROUND_THREAD_POOL
    }
}

#[cfg(target_os = "macos")]
fn non_efficiency_core_count() -> usize {
    let detected = sysctl_count(c"hw.nperflevels").and_then(|levels| {
        count_non_efficiency_cores((0..levels).map(|level| {
            let name = std::ffi::CString::new(format!("hw.perflevel{level}.name")).ok()?;
            let count = std::ffi::CString::new(format!("hw.perflevel{level}.physicalcpu")).ok()?;
            Some((sysctl_name(&name)?, sysctl_count(&count)?))
        }))
    });

    // Unknown topology falls back to all logical cores, never to a single thread.
    detected
        .or_else(|| sysctl_count(c"hw.perflevel0.physicalcpu").filter(|&count| count > 0))
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|p| p.get())
                .unwrap_or(4)
        })
}

#[cfg(any(target_os = "macos", test))]
fn count_non_efficiency_cores(
    levels: impl IntoIterator<Item = Option<(String, usize)>>,
) -> Option<usize> {
    let count = levels.into_iter().try_fold(0usize, |total, level| {
        let (name, count) = level?;
        if name.eq_ignore_ascii_case("Efficiency") {
            Some(total)
        } else if ["Super", "Performance", "Standard"]
            .iter()
            .any(|known| name.eq_ignore_ascii_case(known))
        {
            total.checked_add(count)
        } else {
            None
        }
    })?;
    (count > 0).then_some(count)
}

#[cfg(target_os = "macos")]
fn sysctl_count(name: &std::ffi::CStr) -> Option<usize> {
    let mut value: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&value);
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            &mut value as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || size != std::mem::size_of_val(&value) {
        return None;
    }
    usize::try_from(value).ok()
}

#[cfg(target_os = "macos")]
fn sysctl_name(name: &std::ffi::CStr) -> Option<String> {
    let mut value = [0u8; 64];
    let mut size = value.len();
    let status = unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            value.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 || size > value.len() {
        return None;
    }
    std::ffi::CStr::from_bytes_with_nul(&value[..size])
        .ok()?
        .to_str()
        .ok()
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
}

// Requests user-initiated scheduling priority for the calling thread on macOS, so it and the
// threads it starts (walkers, FSEvents run loops) are not demoted onto efficiency cores.
pub(crate) fn pin_user_initiated() {
    #[cfg(target_os = "macos")]
    unsafe {
        let _ = libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INITIATED, 0);
    }
}

#[cfg(test)]
mod tests {
    use super::{SearchBlocks, count_non_efficiency_cores};
    use std::ops::Range;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, mpsc};
    use std::time::Duration;

    fn run_in(
        blocks: SearchBlocks,
        pool: &rayon::ThreadPool,
        len: usize,
        run: impl Fn(Range<usize>, &mut Vec<usize>) + Sync,
    ) -> Vec<usize> {
        let mut out = Vec::new();
        blocks.extend_in(pool, &mut out, len, || (), |_, range, out| run(range, out));
        out
    }

    fn pool(threads: usize) -> rayon::ThreadPool {
        rayon::ThreadPoolBuilder::new()
            .num_threads(threads)
            .build()
            .unwrap()
    }

    #[test]
    fn blocks_preserve_order_and_offsets() {
        let pool = pool(3);
        for len in [1, 2, 7, 64, 65, 10_007] {
            let expected: Vec<_> = (0..len).filter(|index| index % 3 != 0).collect();
            for blocks in [
                SearchBlocks::new(1, 1),
                SearchBlocks::new(1, 8),
                SearchBlocks::new(64, 4096),
            ] {
                for max_jobs in [1, 2, usize::MAX] {
                    let actual = run_in(blocks.max_jobs(max_jobs), &pool, len, |range, out| {
                        out.extend(range.filter(|index| index % 3 != 0))
                    });
                    assert_eq!(actual, expected, "{len} {max_jobs}");
                }
            }
        }

        let input: Vec<usize> = (0..10_007).collect();
        let mut out = vec![usize::MAX];
        SearchBlocks::new(1, 8).extend_filter_map(
            &mut out,
            &input,
            || (),
            |_, index, &item| {
                assert_eq!(index, item);
                (index % 3 != 0).then_some(item)
            },
        );
        assert_eq!(out[0], usize::MAX);
        assert!(
            out[1..]
                .iter()
                .copied()
                .eq(input.iter().copied().filter(|i| i % 3 != 0))
        );
    }

    #[test]
    fn empty_input_does_not_initialize_state() {
        let actual = SearchBlocks::new(1, 1).filter_map(
            &[] as &[usize],
            || -> () { panic!("empty input initialized scratch state") },
            |_, _, &item| Some(item),
        );
        assert!(actual.is_empty());
    }

    #[test]
    fn panic_propagates_after_other_blocks_finish() {
        let finished = AtomicBool::new(false);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_in(SearchBlocks::new(1, 1), &pool(2), 2, |range, out| {
                assert_ne!(range.start, 0, "block failed");
                finished.store(true, Ordering::SeqCst);
                out.push(range.start);
            })
        }));
        assert!(result.is_err());
        assert!(finished.load(Ordering::SeqCst));
    }

    // A's blocks all run, then B takes every pool worker, including the ones A
    // frees. A must still return: pool workers never run B inside a wait for A,
    // as a worker blocked in `install` + nested parallel iterators would.
    #[test]
    fn finished_request_does_not_wait_for_unrelated_request() {
        const TIMEOUT: Duration = Duration::from_secs(10);
        let pool = pool(3);
        let blocks = SearchBlocks::new(1, 1);
        let (started, starts) = mpsc::channel::<char>();
        let (release_a, gate_a) = mpsc::channel::<()>();
        let (release_b, gate_b) = mpsc::channel::<()>();
        let (gate_a, gate_b) = (Mutex::new(gate_a), Mutex::new(gate_b));
        let (done_a, a_done) = mpsc::channel();
        let wait = |request: char| {
            for _ in 0..3 {
                assert_eq!(starts.recv_timeout(TIMEOUT), Ok(request));
            }
        };

        std::thread::scope(|scope| {
            scope.spawn(|| {
                let result = run_in(blocks, &pool, 3, |range, out| {
                    started.send('a').unwrap();
                    gate_a.lock().unwrap().recv_timeout(TIMEOUT).unwrap();
                    out.push(range.start);
                });
                done_a.send(result).unwrap();
            });
            wait('a');
            let b = scope.spawn(|| {
                run_in(blocks, &pool, 3, |range, out| {
                    started.send('b').unwrap();
                    gate_b.lock().unwrap().recv_timeout(3 * TIMEOUT).unwrap();
                    out.push(range.start);
                })
            });
            // B's caller and one job start at once; its last job needs a worker A frees.
            assert_eq!(starts.recv_timeout(TIMEOUT), Ok('b'));
            assert_eq!(starts.recv_timeout(TIMEOUT), Ok('b'));
            for _ in 0..3 {
                release_a.send(()).unwrap();
            }
            assert_eq!(starts.recv_timeout(TIMEOUT), Ok('b'));
            let a = a_done.recv_timeout(TIMEOUT);
            for _ in 0..3 {
                release_b.send(()).unwrap();
            }
            assert_eq!(b.join().unwrap(), vec![0, 1, 2]);
            assert_eq!(
                a.expect("finished request waited for an unrelated request"),
                vec![0, 1, 2]
            );
        });
    }

    #[test]
    fn includes_super_and_performance_cores() {
        assert_eq!(count(&[("Super", 6), ("Performance", 12)]), Some(18));
        assert_eq!(count(&[("Performance", 12), ("Super", 6)]), Some(18));
    }

    #[test]
    fn excludes_efficiency_cores_in_every_position() {
        assert_eq!(count(&[("Performance", 12), ("Efficiency", 4)]), Some(12));
        assert_eq!(count(&[("Efficiency", 4), ("Performance", 4)]), Some(4));
        assert_eq!(
            count(&[("Super", 6), ("Efficiency", 4), ("Performance", 12)]),
            Some(18)
        );
        assert_eq!(count(&[("Performance", 8), ("efficiency", 4)]), Some(8));
    }

    #[test]
    fn accepts_standard_cores() {
        assert_eq!(count(&[("Performance", 8)]), Some(8));
        assert_eq!(count(&[("Standard", 8)]), Some(8));
    }

    #[test]
    fn unknown_core_types_fall_back() {
        assert_eq!(count(&[("Faster", 6), ("Performance", 12)]), None);
        assert_eq!(count(&[("Super", 6), ("PowerSaving", 4)]), None);
    }

    #[test]
    fn incomplete_or_unusable_topology_falls_back() {
        assert_eq!(count(&[]), None);
        assert_eq!(count(&[("Efficiency", 4)]), None);
        assert_eq!(count(&[("Performance", 0)]), None);
        assert_eq!(
            count_non_efficiency_cores([Some(("Super".into(), 6)), None]),
            None
        );
        assert_eq!(count(&[("Super", usize::MAX), ("Performance", 1)]), None);
    }

    fn count(levels: &[(&str, usize)]) -> Option<usize> {
        count_non_efficiency_cores(
            levels
                .iter()
                .map(|&(name, count)| Some((name.to_owned(), count))),
        )
    }
}
