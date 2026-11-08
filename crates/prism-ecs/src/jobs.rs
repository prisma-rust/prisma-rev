//! Scoped parallel chunk helper.
//!
//! Batch systems (per-character animation evaluation, for example) need to
//! split a slice into disjoint mutable chunks and process them on worker
//! threads. [`scoped_parallel_chunks`] gives each chunk exclusive access, so no
//! `Arc<Mutex<_>>` or atomics are needed and two threads can never touch the
//! same element.

use std::any::Any;
use std::cell::Cell;
use std::collections::VecDeque;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};

fn available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

/// Splits `items` into chunks processed in parallel by the shared worker pool.
///
/// Unlike a parallel-for over indices, this has **exclusive** semantics: each
/// chunk gets its own `&mut [T]`, so no `Arc<Mutex<_>>` or atomics are needed
/// and two threads can never touch the same element. It fits batch work where
/// each element already owns mutable state (for example, per-character
/// animation evaluators).
///
/// The pool is created once and reused for the whole process, so per-frame
/// callers do not pay thread creation costs. `max_threads` is an upper bound;
/// the actual count is also limited by available parallelism and the length of
/// `items`. With a single thread it runs sequentially without dispatching.
///
/// # Panics
///
/// A panic inside `work` is re-raised on the calling thread after every chunk
/// has finished; the pool itself survives it.
pub fn scoped_parallel_chunks<T, F>(items: &mut [T], max_threads: usize, work: F)
where
    T: Send,
    F: Fn(&mut T) + Sync,
{
    if items.is_empty() {
        return;
    }
    let threads = available_parallelism()
        .min(max_threads.max(1))
        .min(items.len());
    // A worker that calls this again would wait for itself; run inline instead
    // of deadlocking.
    if threads <= 1 || ON_WORKER.with(Cell::get) {
        for item in items {
            work(item);
        }
        return;
    }
    let pool = WorkerPool::global();
    if pool.worker_count == 0 {
        for item in items {
            work(item);
        }
        return;
    }

    let chunk = items.len().div_ceil(threads);
    let pending = Pending::new(items.chunks_mut(chunk).count());
    for group in items.chunks_mut(chunk) {
        pending.dispatch(pool, group, &work);
    }
    pending.wait();
}

thread_local! {
    /// Set while a pool worker runs a job, so nested calls stay inline.
    static ON_WORKER: Cell<bool> = const { Cell::new(false) };
}

type Job = Box<dyn FnOnce() + Send + 'static>;

/// Shared queue plus a fixed set of long-lived workers.
struct WorkerPool {
    queue: Arc<JobQueue>,
    worker_count: usize,
}

struct JobQueue {
    jobs: Mutex<VecDeque<Job>>,
    ready: Condvar,
    shutdown: AtomicBool,
}

impl WorkerPool {
    /// The process-wide pool; workers are started on first use.
    fn global() -> &'static Self {
        static POOL: OnceLock<WorkerPool> = OnceLock::new();
        POOL.get_or_init(|| {
            let queue = Arc::new(JobQueue {
                jobs: Mutex::new(VecDeque::new()),
                ready: Condvar::new(),
                shutdown: AtomicBool::new(false),
            });
            let mut worker_count = 0;
            for worker in 0..available_parallelism() {
                let queue = Arc::clone(&queue);
                let spawn = std::thread::Builder::new()
                    .name(format!("prism-ecs-job-{worker}"))
                    .spawn(move || worker_loop(&queue));
                if spawn.is_ok() {
                    worker_count += 1;
                }
            }
            #[cfg(test)]
            WORKERS_SPAWNED.fetch_add(worker_count, Ordering::SeqCst);
            Self {
                queue,
                worker_count,
            }
        })
    }
}

/// Number of workers the process-wide pool ever started (test only).
#[cfg(test)]
static WORKERS_SPAWNED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn worker_loop(queue: &JobQueue) {
    ON_WORKER.with(|flag| flag.set(true));
    loop {
        let job = {
            let mut jobs = queue
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            loop {
                if let Some(job) = jobs.pop_front() {
                    break job;
                }
                if queue.shutdown.load(Ordering::SeqCst) {
                    return;
                }
                jobs = queue
                    .ready
                    .wait(jobs)
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
            }
        };
        // Keep the worker alive even when one job panics; the panic is
        // re-raised by the dispatcher.
        let _ = catch_unwind(AssertUnwindSafe(job));
    }
}

/// Bookkeeping shared by the chunks of one [`scoped_parallel_chunks`] call.
struct Pending {
    remaining: Arc<(Mutex<usize>, Condvar)>,
    panic: Arc<Mutex<Option<Box<dyn Any + Send>>>>,
}

impl Pending {
    fn new(chunks: usize) -> Self {
        Self {
            remaining: Arc::new((Mutex::new(chunks), Condvar::new())),
            panic: Arc::new(Mutex::new(None)),
        }
    }

    fn dispatch<T, F>(&self, pool: &WorkerPool, chunk: &mut [T], work: &F)
    where
        T: Send,
        F: Fn(&mut T) + Sync,
    {
        let items = RawSlice(chunk.as_mut_ptr().cast(), chunk.len());
        let work = RawRef((work as *const F).cast());
        let remaining = Arc::clone(&self.remaining);
        let panic = Arc::clone(&self.panic);
        let job: Job = Box::new(move || {
            // SAFETY: the dispatcher blocks in `wait` until every chunk has
            // run, so the slice and the closure outlive this job.
            let items = unsafe { items.into_mut_slice() };
            let work: &F = unsafe { work.get() };
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                for item in items {
                    work(item);
                }
            }));
            if let Err(payload) = outcome {
                let mut slot = panic
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                slot.get_or_insert(payload);
            }
            let (count, ready) = &*remaining;
            let mut count = count
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *count = count.saturating_sub(1);
            ready.notify_all();
        });
        {
            let mut jobs = pool
                .queue
                .jobs
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            jobs.push_back(job);
        }
        pool.queue.ready.notify_one();
    }

    fn wait(self) {
        let (count, ready) = &*self.remaining;
        let mut count = count
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while *count > 0 {
            count = ready
                .wait(count)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        drop(count);
        let payload = self
            .panic
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(payload) = payload {
            std::panic::resume_unwind(payload);
        }
    }
}

/// A `*mut [T]` that may cross thread boundaries; see [`Pending::dispatch`].
///
/// The pointer is erased to `*mut ()` so the job closure is `'static`; the
/// dispatcher's `wait` is what keeps the pointee alive.
struct RawSlice(*mut (), usize);

// SAFETY: `scoped_parallel_chunks` hands each pointer to exactly one job and
// does not touch the slice again until that job reports completion.
unsafe impl Send for RawSlice {}

impl RawSlice {
    unsafe fn into_mut_slice<'a, T>(self) -> &'a mut [T] {
        // SAFETY: guaranteed by the caller; see the `Send` impl above.
        unsafe { std::slice::from_raw_parts_mut(self.0.cast::<T>(), self.1) }
    }
}

/// A `*const F` shared by every chunk of one call, erasing `F`'s lifetime so
/// the job closure stays `'static`.
struct RawRef(*const ());

// SAFETY: the pointee is only read through `&F`, and `F: Sync` means shared
// references may cross threads.
unsafe impl Send for RawRef {}

impl RawRef {
    unsafe fn get<'a, F>(self) -> &'a F {
        // SAFETY: guaranteed by the caller; see the `Send` impl above.
        unsafe { &*self.0.cast::<F>() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scoped_chunks_visit_every_item_once() {
        let mut items: Vec<usize> = (0..37).collect();
        scoped_parallel_chunks(&mut items, 4, |item| *item *= 2);
        assert_eq!(items, (0..37).map(|item| item * 2).collect::<Vec<_>>());
    }

    #[test]
    fn scoped_chunks_handle_empty_and_single_thread_inputs() {
        let mut empty: Vec<usize> = Vec::new();
        scoped_parallel_chunks(&mut empty, 4, |_| panic!("must not run"));

        let mut one = vec![1usize, 2, 3];
        scoped_parallel_chunks(&mut one, 1, |item| *item += 1);
        assert_eq!(one, [2, 3, 4]);
    }

    #[test]
    fn the_worker_pool_is_reused_between_calls() {
        let mut items = vec![0u32; 16];
        scoped_parallel_chunks(&mut items, 4, |item| *item += 1);
        let spawned = WORKERS_SPAWNED.load(Ordering::SeqCst);
        assert!(spawned > 0, "the pool must start at least one worker");

        for _ in 0..32 {
            scoped_parallel_chunks(&mut items, 4, |item| *item += 1);
        }
        assert_eq!(
            WORKERS_SPAWNED.load(Ordering::SeqCst),
            spawned,
            "later calls must not spawn new workers"
        );
        assert!(items.iter().all(|item| *item == 33));
    }

    #[test]
    fn nested_calls_stay_inline_on_a_worker() {
        let mut outer: Vec<u32> = (0..64).collect();
        scoped_parallel_chunks(&mut outer, 4, |item| {
            let mut inner = vec![*item; 3];
            scoped_parallel_chunks(&mut inner, 2, |value| *value += 1);
            *item = inner[0];
        });
        assert_eq!(outer, (1..65).collect::<Vec<_>>());
    }

    #[test]
    fn panics_reach_the_caller_and_leave_the_pool_usable() {
        let mut items = vec![0u32; 32];
        let caught = catch_unwind(AssertUnwindSafe(|| {
            scoped_parallel_chunks(&mut items, 4, |_| panic!("chunk failure"));
        }));
        assert!(caught.is_err());

        let mut after = vec![0u32; 32];
        scoped_parallel_chunks(&mut after, 4, |item| *item += 1);
        assert!(after.iter().all(|item| *item == 1));
    }
}
