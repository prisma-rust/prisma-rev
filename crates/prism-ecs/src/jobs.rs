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

use std::sync::Arc;
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

/// Splits `items` into chunks processed in parallel by scoped threads.
///
/// Unlike [`schedule_parallel_for`], this has **exclusive** semantics: each
/// chunk gets its own `&mut [T]`, so no `Arc<Mutex<_>>` or atomics are needed
/// and two threads can never touch the same element. It fits batch work where
/// each element already owns mutable state (for example, per-character
/// animation evaluators). `max_threads` is an upper bound; the actual count is
/// also limited by available parallelism and the length of `items`. With a
/// single thread it runs sequentially without spawning.
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
    let chunk = items.len().div_ceil(threads);
    std::thread::scope(|scope| {
        for group in items.chunks_mut(chunk) {
            scope.spawn(|| {
                for item in group {
                    work(item);
                }
            });
        }
    });
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
    fn scoped_chunks_handle_empty_and_single_thread_inputs() {
        let mut empty: Vec<usize> = Vec::new();
        scoped_parallel_chunks(&mut empty, 4, |_| panic!("must not run"));

        let mut one = vec![1usize, 2, 3];
        scoped_parallel_chunks(&mut one, 1, |item| *item += 1);
        assert_eq!(one, [2, 3, 4]);
    }
}
