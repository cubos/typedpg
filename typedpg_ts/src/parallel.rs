//! Independent analyses run across the machine's cores.

use std::sync::atomic::{AtomicUsize, Ordering};

/// The stack each worker starts with: the main thread's. The analyzer grows
/// its own for a deep query (see `typedpg_analyzer`'s `stack` module).
pub const STACK: usize = 8 << 20;

/// `f` of each item, in the items' order, computed on up to as many threads
/// as the machine has cores. Each item is computed exactly once, by one
/// thread, so `f` must not depend on what else the thread computed. A
/// panic in `f` is resumed here.
pub fn map<T: Sync, R: Send>(items: &[T], f: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let threads = std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(items.len());
    if threads <= 1 {
        return items.iter().map(f).collect();
    }
    let next = AtomicUsize::new(0);
    let mut out: Vec<Option<R>> = std::iter::repeat_with(|| None).take(items.len()).collect();
    std::thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                std::thread::Builder::new()
                    .stack_size(STACK)
                    .spawn_scoped(scope, || {
                        let mut done = Vec::new();
                        loop {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            let Some(item) = items.get(i) else {
                                return done;
                            };
                            done.push((i, f(item)));
                        }
                    })
                    .expect("failed to spawn a worker thread")
            })
            .collect();
        // Every worker is joined before a panic is resumed: a scoped
        // thread still running would be joined by the scope, and a second
        // panic while unwinding aborts.
        let mut panicked = None;
        for worker in workers {
            match worker.join() {
                Ok(done) => {
                    for (i, r) in done {
                        out[i] = Some(r);
                    }
                }
                Err(panic) => panicked = panicked.or(Some(panic)),
            }
        }
        if let Some(panic) = panicked {
            std::panic::resume_unwind(panic);
        }
    });
    out.into_iter()
        .map(|r| r.expect("every item is computed"))
        .collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn keeps_the_order() {
        let items: Vec<usize> = (0..1000).collect();
        assert_eq!(
            super::map(&items, |i| i * 2),
            (0..1000).map(|i| i * 2).collect::<Vec<_>>()
        );
        assert!(super::map(&[] as &[usize], |i| *i).is_empty());
    }
}
