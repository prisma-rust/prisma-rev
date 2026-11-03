//! 数据导向作业形状的作业门面。
//!
//! 引擎的 job 是 Burst 编译、由 `JobHandle` 显式 `Complete()` 的并行任务。
//! Rust 端用标准库线程实现同样的**使用形状**：`Schedule` 立即返回
//! [`JobHandle`]，`Complete()` 等待所有工作线程结束。
//!
//! 命名对应（Rust 用 snake_case，括号内为 原名）：
//! [`IJob::execute`]（`IJob.Execute`）、[`IJobParallelFor::execute`]
//! （`IJobParallelFor.Execute(int)`）、[`JobHandle::complete`]（`JobHandle.Complete`）、
//! [`schedule`]（`IJobExtensions.Schedule`）、
//! [`schedule_parallel_for`]（`IJobParallelForExtensions.Schedule`）。
//!
//! 与引擎的差异：
//! - 没有 Burst、`JobRanges`、`AtomicSafetyHandle`；并行 job 通过 `&self` 执行，
//! 需要共享可写状态时用 `Atomic*` / `Mutex`（`NativeArray` + 安全属性）。
//! - `IJobParallelFor.Execute` 的下标是 `usize`，不是引擎的 `int`。
//! - 丢弃未 `complete()` 的 [`JobHandle`] 会分离线程（job 仍会跑完），不会自动等待。

use std::sync::Arc;
use std::thread::JoinHandle;

/// `IJob`：只执行一次的作业。
pub trait IJob {
    /// 执行作业（`IJob.Execute`）。
    fn execute(&self);
}

/// `IJobParallelFor`：按下标并行执行的作业。
pub trait IJobParallelFor {
    /// 执行单个下标（`IJobParallelFor.Execute(int index)`）。
    fn execute(&self, index: usize);
}

/// `IJobFor`：按下标执行、但不要求固定 batch 的作业。
///
/// 它是 [`IJobParallelFor`] 的扩展 trait；实现者需要同时实现两者。
pub trait IJobFor: IJobParallelFor {}

/// `JobHandle`：作业完成句柄。
///
/// 调度函数立即返回该句柄；[`complete`](Self::complete) 等待所有工作线程结束。
/// 句柄被丢弃而没有 `complete()` 时线程会分离，作业不会取消。
#[derive(Debug, Default)]
pub struct JobHandle {
    threads: Vec<JoinHandle<()>>,
}

impl JobHandle {
    /// 一个已经完成的空句柄（`default(JobHandle)`，但已处于完成态）。
    pub fn completed() -> Self {
        Self::default()
    }

    /// 等待所有关联作业结束（`JobHandle.Complete`）。
    ///
    /// 任一作业线程 panic 时，等其余线程结束后把该 panic 继续抛给调用方。
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

    /// 合并两个句柄，完成返回的句柄会等待两者（
    /// `JobHandle.CombineDependencies`）。
    pub fn combine(mut self, other: JobHandle) -> Self {
        self.threads.extend(other.threads);
        self
    }

    /// 合并一组句柄（`JobHandle.CombineDependencies(NativeArray<JobHandle>)`）。
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

/// 调度一个 [`IJob`] 到工作线程（`IJobExtensions.Schedule`）。
pub fn schedule<J>(job: J) -> JobHandle
where
    J: IJob + Send + 'static,
{
    JobHandle::from_threads(vec![std::thread::spawn(move || job.execute())])
}

/// 按 `batch_size` 分批并行调度 [`IJobParallelFor`]
/// （`IJobParallelForExtensions.Schedule(job, length, batchSize)`）。
///
/// 每个 batch 是分配工作的最小单位；实际工作线程数取
/// `min(batch 数, 可用并行度)`。
///
/// # Panics
///
/// `batch_size == 0` 时 panic（与 抛异常一致）。
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

/// 调度一个 [`IJobFor`]，每个下标独立执行
/// （`IJobForExtensions.Schedule`）。
///
/// 等价于 `batch_size = 1` 的 [`schedule_parallel_for`]，但保留引擎的
/// `IJobFor` 入口形状。
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

/// 用作用域线程把 `items` 按块分给多个线程并行处理。
///
/// 与 [`schedule_parallel_for`] 的区别是**独占**语义：每块拿到自己的
/// `&mut [T]`，不需要 `Arc<Mutex<_>>` 或原子量，也不可能有两个线程同时碰同一个
/// 元素。适合「每个元素本来就有独立可变状态」的批处理（例如每个角色自己的
/// 动画求值器）。`max_threads` 是上限，实际线程数还会被可用并行度和
/// `items` 长度压住；只有一个线程时直接顺序执行，不建线程。
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
