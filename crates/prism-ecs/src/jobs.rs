//! Scoped parallel chunk helper.
//!
//! Batch systems (per-character animation evaluation, for example) need to
//! split a slice into disjoint mutable chunks and process them on worker
//! threads. [`scoped_parallel_chunks`] gives each chunk exclusive access, so no
//! `Arc<Mutex<_>>` or atomics are needed and two threads can never touch the
//! same element.

fn available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
}

/// Splits `items` into chunks processed in parallel by scoped threads.
///
/// Unlike a parallel-for over indices, this has **exclusive** semantics: each
/// chunk gets its own `&mut [T]`, which fits batch work where each element
/// already owns mutable state. `max_threads` is an upper bound; the actual
/// count is also limited by available parallelism and the length of `items`.
/// With a single thread it runs sequentially without spawning.
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
