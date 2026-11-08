//! `.Jobs`-shaped job facade.
//!
//! Engine jobs are Burst-compiled parallel tasks completed explicitly through a
//! `JobHandle`'s `Complete()`. The Rust mirror uses standard library threads to
//! reproduce the same **usage shape**: `Schedule` returns a [`JobHandle`]
//! immediately, and `Complete()` waits for all worker threads to finish.
//!
//! Naming (Rust uses snake_case; the original name is in parentheses):
//! [`IJob::execute`] (`IJob.Execute`), [`IJobParallelFor::execute`]
//! (`IJobParallelFor.Execute(int)`), [`JobHandle::complete`]
//! (`JobHandle.Complete`), [`schedule`] (`IJobExtensions.Schedule`),
//! [`schedule_parallel_for`] (`IJobParallelForExtensions.Schedule`).
//!
//! Differences from the engine:
//! - No Burst, `JobRanges`, or `AtomicSafetyHandle`; parallel jobs run through
//!   `&self`, and shared mutable state must use `Atomic*` / `Mutex`
//!   (`NativeArray` + safety attributes in the engine).
//! - `IJobParallelFor.Execute` takes a `usize` index, not the engine's `int`.
//! - Dropping a [`JobHandle`] without `complete()` detaches the threads (the
//!   job still runs to completion); it does not wait automatically.

use std::panic::{self, AssertUnwindSafe};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;

/// `IJob`: a job that runs exactly once.
pub trait IJob {
    /// Runs the job (`IJob.Execute`).
    fn execute(&self);
}

/// `IJobParallelFor`: a job run in parallel per index.
pub trait IJobParallelFor {
    /// Runs a single index (`IJobParallelFor.Execute(int index)`).
    fn execute(&self, index: usize);
}

/// `IJobFor`: a per-index job that does not require a fixed batch.
///
/// It is an extension trait of [`IJobParallelFor`]; implementors must implement
/// both.
pub trait IJobFor: IJobParallelFor {}

/// `JobHandle`: completion handle for a job.
///
/// Scheduling functions return it immediately; [`complete`](Self::complete)
/// waits for all worker threads. Dropping the handle without `complete()`
/// detaches the threads; the job is not cancelled.
#[derive(Debug, Default)]
pub struct JobHandle {
    threads: Vec<JoinHandle<()>>,
}

impl JobHandle {
    /// An empty handle that is already complete (`default(JobHandle)`, but in
    /// the completed state).
    pub fn completed() -> Self {
        Self::default()
    }

    /// Waits for all associated jobs to finish (`JobHandle.Complete`).
    ///
    /// If any job thread panics, the remaining threads are joined first and the
    /// panic is then resumed on the caller.
    pub fn complete(mut self) {
        let mut panic = None;
        for thread in self.threads.drain(..) {
            if let Err(payload) = thread.join() {
                panic.get_or_insert(payload);
            }
        }
        if let Some(payload) = panic {
            std::panic::resume_unwind(payload);
        }
    }

    /// Combines two handles; completing the returned handle waits for both
    /// (`JobHandle.CombineDependencies`).
    pub fn combine(mut self, other: JobHandle) -> Self {
        self.threads.extend(other.threads);
        self
    }

    /// Combines a set of handles
    /// (`JobHandle.CombineDependencies(NativeArray<JobHandle>)`).
    pub fn combine_dependencies(handles: impl IntoIterator<Item = JobHandle>) -> Self {
        let mut combined = Self::completed();
        for handle in handles {
            combined = combined.combine(handle);
        }
        combined
    }

    fn from_threads(threads: Vec<JoinHandle<()>>) -> Self {
        Self { threads }
    }
}

/// Schedules an [`IJob`] on a worker thread (`IJobExtensions.Schedule`).
pub fn schedule<J>(job: J) -> JobHandle
where
    J: IJob + Send + 'static,
{
    JobHandle::from_threads(vec![std::thread::spawn(move || job.execute())])
}

/// Schedules an [`IJobParallelFor`] in batches of `batch_size`
/// (`IJobParallelForExtensions.Schedule(job, length, batchSize)`).
///
/// Each batch is the smallest unit of work distribution; the actual worker
/// count is `min(batch count, available parallelism)`.
///
/// # Panics
///
/// Panics when `batch_size == 0` (matching the engine's exception).
pub fn schedule_parallel_for<J>(job: J, length: usize, batch_size: usize) -> JobHandle
where
    J: IJobParallelFor + Send + Sync + 'static,
{
    assert!(batch_size > 0, "batch_size must be greater than zero");
    if length == 0 {
        return JobHandle::completed();
    }

    let job = Arc::new(job);
    let batches = length.div_ceil(batch_size);
    let workers = batches.min(available_parallelism());
    let mut threads = Vec::with_capacity(workers);
    for worker in 0..workers {
        let job = Arc::clone(&job);
        threads.push(std::thread::spawn(move || {
            for batch in (worker..batches).step_by(workers) {
                let start = batch * batch_size;
                let end = (start + batch_size).min(length);
                for index in start..end {
                    job.execute(index);
                }
            }
        }));
    }
    JobHandle::from_threads(threads)
}

/// Schedules an [`IJobFor`], with each index run independently
/// (`IJobForExtensions.Schedule`).
///
/// Equivalent to [`schedule_parallel_for`] with `batch_size = 1`, but keeps the
/// engine's `IJobFor` entry shape.
pub fn schedule_for<J>(job: J, length: usize) -> JobHandle
where
    J: IJobFor + Send + Sync + 'static,
{
    schedule_parallel_for(job, length, 1)
}

fn available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

/// One chunk handed to a pool thread.
struct Chunk<'a, T, F> {
    start: *mut T,
    len: usize,
    work: &'a F,
    completion: Arc<Completion>,
}

/// A type-erased task in the job pool queue.
///
/// The payload borrows caller-owned memory, which is sound because the caller
/// does not return from [`scoped_parallel_chunks`] until every task reported
/// completion. `Send` is asserted manually: the pointee is `Send` and the work
/// closure is `Sync`, both enforced by the caller's bounds.
struct Task {
    run: unsafe fn(*mut ()),
    data: *mut (),
}

// SAFETY: `scoped_parallel_chunks` keeps every referenced value alive and
// immovable until `Completion::wait` observes all tasks finished.
unsafe impl Send for Task {}

/// Shared completion state for one [`scoped_parallel_chunks`] call.
#[derive(Default)]
struct Completion {
    state: Mutex<CompletionState>,
    idle: Condvar,
}

#[derive(Default)]
struct CompletionState {
    remaining: usize,
    panics: Vec<Box<dyn std::any::Any + Send>>,
}

impl Completion {
    fn new(remaining: usize) -> Self {
        Self {
            state: Mutex::new(CompletionState {
                remaining,
                panics: Vec::new(),
            }),
            idle: Condvar::new(),
        }
    }

    fn finish(&self, result: std::thread::Result<()>) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(payload) = result {
            state.panics.push(payload);
        }
        state.remaining -= 1;
        let done = state.remaining == 0;
        drop(state);
        if done {
            self.idle.notify_all();
        }
    }

    fn wait(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        while state.remaining > 0 {
            state = self
                .idle
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
        let panics = std::mem::take(&mut state.panics);
        drop(state);
        if let Some(payload) = panics.into_iter().next() {
            panic::resume_unwind(payload);
        }
    }
}

/// Persistent worker threads, created once per process.
///
/// Creating threads per frame is measurable next to skinning work, and the
/// players must not grow a scheduler dependency (DESIGN §12 rules out rayon), so
/// this is the smallest pool that keeps [`scoped_parallel_chunks`]'s API.
struct Pool {
    sender: mpsc::Sender<Task>,
}

#[cfg(test)]
static POOL_INITIALIZATIONS: AtomicUsize = AtomicUsize::new(0);

fn pool() -> &'static Pool {
    static POOL: OnceLock<Pool> = OnceLock::new();
    POOL.get_or_init(|| {
        #[cfg(test)]
        POOL_INITIALIZATIONS.fetch_add(1, Ordering::SeqCst);
        let (sender, receiver) = mpsc::channel::<Task>();
        let receiver = Arc::new(Mutex::new(receiver));
        for index in 0..available_parallelism() {
            let receiver = Arc::clone(&receiver);
            std::thread::Builder::new()
                .name(format!("prism-job-{index}"))
                .spawn(move || worker(receiver))
                .expect("spawn job worker");
        }
        Pool { sender }
    })
}

fn worker(receiver: Arc<Mutex<mpsc::Receiver<Task>>>) {
    loop {
        let task = {
            let queue = receiver
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            queue.recv()
        };
        match task {
            // SAFETY: the payload stays valid until its completion is reported,
            // and `run_chunk` reports completion even when the work panics.
            Ok(task) => unsafe { (task.run)(task.data) },
            Err(_) => return,
        }
    }
}

unsafe fn run_chunk<T, F>(data: *mut ())
where
    T: Send,
    F: Fn(&mut T) + Sync,
{
    // SAFETY: the pointer comes from a `Chunk<T, F>` that outlives this call.
    let chunk = unsafe { &*(data as *const Chunk<'_, T, F>) };
    let result = panic::catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: the chunk owns an exclusive sub-slice of the caller's items.
        let items = unsafe { std::slice::from_raw_parts_mut(chunk.start, chunk.len) };
        for item in items {
            (chunk.work)(item);
        }
    }));
    chunk.completion.finish(result);
}

/// Splits `items` into chunks processed in parallel by the job pool.
///
/// Unlike [`schedule_parallel_for`], this has **exclusive** semantics: each
/// chunk gets its own `&mut [T]`, so no `Arc<Mutex<_>>` or atomics are needed
/// and two threads can never touch the same element. It fits batch work where
/// each element already owns mutable state (for example, per-character
/// animation evaluators). `max_threads` is an upper bound; the actual count is
/// also limited by available parallelism and the length of `items`. With a
/// single thread it runs sequentially without touching the pool.
///
/// The worker threads are created once and reused, so calling this every frame
/// does not spawn threads (#1031). It is **not reentrant**: a work closure must
/// not call it again, because the caller blocks until every chunk finishes.
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
    if threads <= 1 {
        for item in items {
            work(item);
        }
        return;
    }

    let work = &work;
    let chunk_len = items.len().div_ceil(threads);
    // Fewer chunks than threads is normal (10 items over 6 threads gives 5 chunks), so the
    // completion counter must match the chunks actually queued.
    let chunk_count = items.len().div_ceil(chunk_len);
    let completion = Arc::new(Completion::new(chunk_count));

    // Split first and keep the chunks in a fixed allocation: the tasks below
    // borrow these addresses, so the vector must not move afterwards.
    let mut chunks: Vec<Chunk<'_, T, F>> = Vec::with_capacity(threads);
    let mut rest = items;
    while !rest.is_empty() {
        let take = chunk_len.min(rest.len());
        let (head, tail) = rest.split_at_mut(take);
        chunks.push(Chunk {
            start: head.as_mut_ptr(),
            len: head.len(),
            work,
            completion: Arc::clone(&completion),
        });
        rest = tail;
    }

    let pool = pool();
    for chunk in &chunks {
        let task = Task {
            run: run_chunk::<T, F> as unsafe fn(*mut ()),
            data: chunk as *const Chunk<'_, T, F> as *mut (),
        };
        if let Err(task) = pool.sender.send(task) {
            // The pool is gone (for example, a caller shut it down); the chunk
            // still has to run, otherwise `wait` never completes.
            // SAFETY: the chunk is alive for the whole call, as for pooled tasks.
            unsafe { (task.0.run)(task.0.data) };
        }
    }
    completion.wait();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    struct CountJob(Arc<AtomicUsize>);

    impl IJob for CountJob {
        fn execute(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    struct CollectJob(Arc<Mutex<Vec<usize>>>);

    impl IJobParallelFor for CollectJob {
        fn execute(&self, index: usize) {
            self.0.lock().expect("collect mutex poisoned").push(index);
        }
    }

    impl IJobFor for CollectJob {}

    fn collected_indices(length: usize, batch_size: usize) -> Vec<usize> {
        let seen = Arc::new(Mutex::new(Vec::new()));
        schedule_parallel_for(CollectJob(Arc::clone(&seen)), length, batch_size).complete();
        let mut indices = seen.lock().expect("collect mutex poisoned").clone();
        indices.sort_unstable();
        indices
    }

    #[test]
    fn schedule_runs_job_once() {
        let count = Arc::new(AtomicUsize::new(0));
        schedule(CountJob(Arc::clone(&count))).complete();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn parallel_for_visits_every_index_once() {
        let length = 37;
        assert_eq!(
            collected_indices(length, 5),
            (0..length).collect::<Vec<_>>()
        );
    }

    #[test]
    fn parallel_for_with_zero_length_is_already_complete() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        schedule_parallel_for(CollectJob(Arc::clone(&seen)), 0, 8).complete();
        assert!(seen.lock().expect("collect mutex poisoned").is_empty());
    }

    #[test]
    fn job_for_visits_all_indices() {
        let length = 11;
        let seen = Arc::new(Mutex::new(Vec::new()));
        schedule_for(CollectJob(Arc::clone(&seen)), length).complete();
        let mut indices = seen.lock().expect("collect mutex poisoned").clone();
        indices.sort_unstable();
        assert_eq!(indices, (0..length).collect::<Vec<_>>());
    }

    #[test]
    fn combine_dependencies_waits_for_both() {
        let count = Arc::new(AtomicUsize::new(0));
        let first = schedule(CountJob(Arc::clone(&count)));
        let second = schedule(CountJob(Arc::clone(&count)));
        JobHandle::combine_dependencies([first, second]).complete();
        assert_eq!(count.load(Ordering::SeqCst), 2);
    }

    #[test]
    #[should_panic(expected = "batch_size")]
    fn zero_batch_size_panics() {
        schedule_parallel_for(CollectJob(Arc::new(Mutex::new(Vec::new()))), 0, 0);
    }

    #[test]
    fn scoped_chunks_visit_every_item_once() {
        let mut items: Vec<usize> = (0..37).collect();
        scoped_parallel_chunks(&mut items, 4, |item| *item *= 2);
        assert_eq!(items, (0..37).map(|item| item * 2).collect::<Vec<_>>());
    }

    #[test]
    fn scoped_chunks_with_fewer_chunks_than_threads_complete() {
        let mut items: Vec<usize> = (0..10).collect();
        scoped_parallel_chunks(&mut items, 6, |item| *item += 1);
        assert_eq!(items, (1..=10).collect::<Vec<_>>());
    }

    #[test]
    fn scoped_chunks_reuse_one_worker_pool() {
        for round in 0..8u32 {
            let mut items: Vec<u32> = (0..64).collect();
            scoped_parallel_chunks(&mut items, 4, |item| *item += round);
        }
        assert_eq!(
            POOL_INITIALIZATIONS.load(Ordering::SeqCst),
            1,
            "the pool must be created once, not per call"
        );
    }

    #[test]
    #[should_panic(expected = "chunk worker panic")]
    fn scoped_chunks_propagate_a_worker_panic() {
        let mut items: Vec<usize> = (0..64).collect();
        scoped_parallel_chunks(&mut items, 2, |item| {
            if *item == 7 {
                panic!("chunk worker panic");
            }
        });
    }

    #[test]
    fn scoped_chunks_handle_empty_and_single_thread_inputs() {
        let mut empty: Vec<usize> = Vec::new();
        scoped_parallel_chunks(&mut empty, 4, |_| panic!("must not run"));

        let mut one = vec![1usize, 2, 3];
        scoped_parallel_chunks(&mut one, 1, |item| *item += 1);
        assert_eq!(one, [2, 3, 4]);
    }
}
