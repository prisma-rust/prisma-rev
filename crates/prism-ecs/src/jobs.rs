//! Background worker pool for batch systems.
//!
//! Batch systems (per-character animation evaluation, for example) need to
//! split a slice into disjoint mutable chunks and process them on worker
//! threads. [`scoped_parallel_chunks`] gives each chunk exclusive access, so no
//! `Arc<Mutex<_>>` or atomics are needed and two threads can never touch the
//! same element.
//!
//! The workers are created once per process and reused (#1031): spawning
//! `min(cores, jobs)` threads per frame put thread creation and teardown on the
//! frame budget exactly when an animated character needed them. Callers still
//! see scoped semantics — [`scoped_parallel_chunks`] returns only after every
//! chunk has been processed.

use std::any::Any;
use std::collections::VecDeque;
use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, PoisonError};
use std::thread::Result as ThreadResult;

fn available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

/// Upper bound on the number of pool threads.
///
/// Animation graph evaluation (the only caller today) caps itself at 8 threads;
/// keeping the pool at that size means an idle pool does not park one thread per
/// core on machines with many cores. Extra jobs simply queue, and the calling
/// thread helps drain them, so a larger `max_threads` still makes progress.
const MAX_POOL_THREADS: usize = 8;

/// A queued unit of work. The `'static` bound is a lie for callers that borrow
/// stack data; see [`erase_job_lifetime`] for why that is sound here.
type Job = Box<dyn FnOnce() + Send + 'static>;

/// Completion state shared by one [`Pool::run_all`] call.
///
/// It is per call, not per pool: two callers can be in flight at the same time
/// (tests do exactly that), and each must observe its own jobs and its own
/// panic.
struct Call {
    state: Mutex<CallState>,
    finished: Condvar,
}

#[derive(Default)]
struct CallState {
    /// Jobs from this call that are queued or running.
    pending: usize,
    /// Payload of the first job that panicked, re-raised on the caller.
    panic: Option<Box<dyn Any + Send>>,
}

impl Call {
    fn new(pending: usize) -> Self {
        Self {
            state: Mutex::new(CallState {
                pending,
                panic: None,
            }),
            finished: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, CallState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn complete(&self, outcome: ThreadResult<()>) {
        let mut state = self.lock();
        if let Err(payload) = outcome {
            state.panic.get_or_insert(payload);
        }
        state.pending -= 1;
        if state.pending == 0 {
            self.finished.notify_all();
        }
    }

    /// Blocks until every job of this call finished, then re-raises its panic.
    fn wait(&self) {
        let mut state = self.lock();
        while state.pending > 0 {
            state = self
                .finished
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner);
        }
        if let Some(payload) = state.panic.take() {
            drop(state);
            panic::resume_unwind(payload);
        }
    }
}

/// Worker pool shared by every [`scoped_parallel_chunks`] call.
struct Pool {
    jobs: Mutex<VecDeque<Job>>,
    work_available: Condvar,
}

impl Pool {
    fn new(threads: usize) -> Arc<Self> {
        let pool = Arc::new(Self {
            jobs: Mutex::new(VecDeque::new()),
            work_available: Condvar::new(),
        });
        for index in 0..threads {
            let worker = Arc::clone(&pool);
            let spawned = std::thread::Builder::new()
                .name(format!("prism-worker-{index}"))
                .spawn(move || worker.work());
            if spawned.is_err() {
                // A partial pool is still correct: the remaining workers plus
                // the calling threads drain the queue.
                break;
            }
        }
        pool
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<Job>> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Worker loop: take a job (waiting when the queue is empty) and run it.
    fn work(&self) {
        loop {
            let job = {
                let mut jobs = self.lock();
                loop {
                    if let Some(job) = jobs.pop_front() {
                        break job;
                    }
                    jobs = self
                        .work_available
                        .wait(jobs)
                        .unwrap_or_else(PoisonError::into_inner);
                }
            };
            job();
        }
    }

    /// Runs `jobs`, helping from the calling thread, and re-raises the first
    /// panic among them. Returns only once every job has been dropped.
    fn run_all(&self, jobs: Vec<Job>) {
        let call = Arc::new(Call::new(jobs.len()));
        let jobs: Vec<Job> = jobs
            .into_iter()
            .map(|job| {
                let call = Arc::clone(&call);
                let job: Job = Box::new(move || {
                    let outcome = panic::catch_unwind(AssertUnwindSafe(job));
                    call.complete(outcome);
                });
                job
            })
            .collect();
        self.lock().extend(jobs);
        self.work_available.notify_all();

        // Help instead of idling: a nested `scoped_parallel_chunks` call (or a
        // pool that failed to spawn its workers) would otherwise deadlock.
        while let Some(job) = self.lock().pop_front() {
            job();
        }
        call.wait();
    }
}

/// The process-wide pool, created on first use.
///
/// `None` when no worker can be spawned (single-core hosts, thread limits):
/// callers then fall back to running the chunks sequentially.
fn worker_pool() -> Option<&'static Arc<Pool>> {
    static POOL: OnceLock<Option<Arc<Pool>>> = OnceLock::new();
    POOL.get_or_init(|| {
        let threads = available_parallelism().min(MAX_POOL_THREADS);
        if threads <= 1 {
            return None;
        }
        Some(Pool::new(threads))
    })
    .as_ref()
}

/// Erases the borrow of a chunk so the job can sit in the pool's queue.
///
/// # Safety
///
/// The job must run (or be dropped) before the data it borrowed goes away.
/// [`Pool::run_all`] waits for the call's `pending` count to reach zero before
/// returning, and every job decrements it after the closure is consumed, so the
/// borrow never escapes [`scoped_parallel_chunks`].
unsafe fn erase_job_lifetime<'a>(job: Box<dyn FnOnce() + Send + 'a>) -> Job {
    // SAFETY: both trait objects are `FnOnce` boxes with the same vtable; only
    // the borrow-checker lifetime is widened.
    unsafe { std::mem::transmute(job) }
}

/// Splits `items` into chunks processed in parallel by worker threads.
///
/// Unlike a parallel-for over indices, this has **exclusive** semantics: each
/// chunk gets its own `&mut [T]`, which fits batch work where each element
/// already owns mutable state. `max_threads` is an upper bound; the actual
/// count is also limited by available parallelism and the length of `items`.
/// With a single thread it runs sequentially without spawning.
///
/// The workers are reused across calls. A panic in `work` is re-raised on the
/// caller after the remaining chunks finish, matching `std::thread::scope`.
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
    let Some(pool) = worker_pool().filter(|_| threads > 1) else {
        for item in items {
            work(item);
        }
        return;
    };
    let chunk = items.len().div_ceil(threads);
    let work = &work;
    let jobs = items
        .chunks_mut(chunk)
        .map(|group| {
            let job: Box<dyn FnOnce() + Send + '_> = Box::new(move || {
                for item in group {
                    work(item);
                }
            });
            // SAFETY: `run_all` below returns only after every job ran or was
            // dropped, so `group` never outlives this call.
            unsafe { erase_job_lifetime(job) }
        })
        .collect();
    pool.run_all(jobs);
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

    fn record_thread(ids: &Mutex<Vec<std::thread::ThreadId>>) -> impl Fn(&mut usize) + Sync + '_ {
        move |_: &mut usize| {
            ids.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(std::thread::current().id());
        }
    }

    /// Repeated calls must reuse one bounded set of worker threads (#1031).
    ///
    /// Which pool thread picks a chunk depends on wake-up order, and the calling
    /// thread helps drain the queue, so comparing two calls id by id is racy.
    /// The stable property is the bound: however many calls run, the ids stay
    /// within the pool size plus the caller. The old implementation spawned
    /// fresh scoped threads per call and blows past that immediately.
    #[test]
    fn parallel_chunks_reuse_worker_threads() {
        if available_parallelism() < 2 {
            return;
        }
        let mut seen = std::collections::HashSet::new();
        for _ in 0..3 {
            let batch = Mutex::new(Vec::new());
            let mut items = vec![0usize; 512];
            scoped_parallel_chunks(&mut items, MAX_POOL_THREADS, record_thread(&batch));
            let ids = batch.into_inner().unwrap_or_else(PoisonError::into_inner);
            assert!(!ids.is_empty());
            seen.extend(ids);
        }
        assert!(
            seen.len() <= MAX_POOL_THREADS + 1,
            "three calls used {} threads: {seen:?}",
            seen.len()
        );
    }

    /// A panicking chunk must not wedge the pool, and the panic has to reach
    /// the caller once the other chunks are done.
    #[test]
    fn panic_in_a_chunk_is_re_raised_and_the_pool_survives() {
        let mut items = vec![0usize; 64];
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            scoped_parallel_chunks(&mut items, MAX_POOL_THREADS, |_| panic!("chunk failed"));
        }));
        assert!(outcome.is_err(), "panic must reach the caller");

        let mut again = vec![0usize; 64];
        scoped_parallel_chunks(&mut again, MAX_POOL_THREADS, |item| *item += 1);
        assert!(again.iter().all(|item| *item == 1), "pool must stay usable");
    }
}
