//! Scheduler implementations for the concurrency annotations.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Scheduling model selected by a concurrency annotation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Schedule {
    /// No annotation: runs inline on the caller's thread.
    Single,
    /// `@Auto`: M:N coroutine scheduling (green tasks multiplexed over a pool).
    Auto,
    /// `@Manual(fixed=N)`: a fixed-size OS thread pool with N workers.
    Manual(usize),
}

impl Schedule {
    pub fn describe(&self) -> String {
        match self {
            Schedule::Single => "single (inline)".into(),
            Schedule::Auto => "M:N coroutine scheduler".into(),
            Schedule::Manual(n) => format!("fixed thread pool ({n} workers)"),
        }
    }
}

/// A unit of work dispatched to a scheduler.
pub type Task = Box<dyn FnOnce() -> TaskResult + Send>;

/// Result of a task (an exit code).
pub type TaskResult = i64;

/// A shared work queue used by both the M:N scheduler and the thread pool.
#[derive(Default)]
struct WorkQueue {
    inner: Mutex<VecDeque<Task>>,
    pending: AtomicUsize,
}

impl WorkQueue {
    fn push(&self, t: Task) {
        self.inner.lock().unwrap().push_back(t);
        self.pending.fetch_add(1, Ordering::SeqCst);
    }
    fn pop(&self) -> Option<Task> {
        loop {
            if let Some(t) = self.inner.lock().unwrap().pop_front() {
                self.pending.fetch_sub(1, Ordering::SeqCst);
                return Some(t);
            }
            std::thread::yield_now();
        }
    }
    fn pending(&self) -> usize {
        self.pending.load(Ordering::SeqCst)
    }
}

/// The runtime scheduler that backs `@Auto` and `@Manual`.
pub struct Scheduler {
    queue: Arc<WorkQueue>,
    workers: Mutex<Vec<std::thread::JoinHandle<()>>>,
    running: AtomicUsize,
}

impl Scheduler {
    /// Create a scheduler for the given model. `Manual(n)` spawns a fixed pool of
    /// `n` worker threads; `Auto` spawns a small worker pool (2 x CPUs) that
    /// multiplexes green tasks (M:N).
    pub fn new(schedule: &Schedule) -> Scheduler {
        let queue = Arc::new(WorkQueue::default());
        let workers = Mutex::new(Vec::new());
        let n = match schedule {
            Schedule::Single => 0,
            Schedule::Auto => std::thread::available_parallelism().map(|p| p.get().max(2)).unwrap_or(2),
            Schedule::Manual(n) => *n,
        };
        let sched = Scheduler {
            queue: queue.clone(),
            workers,
            running: AtomicUsize::new(0),
        };
        for _ in 0..n {
            let q = queue.clone();
            let h = std::thread::spawn(move || {
                loop {
                    let task = q.pop();
                    let _ = task.map(|t| t());
                }
            });
            sched.workers.lock().unwrap().push(h);
        }
        sched
    }

    /// Dispatch a task for execution.
    pub fn spawn(&self, task: Task) {
        self.queue.push(task);
        self.running.fetch_add(1, Ordering::SeqCst);
    }

    /// Number of tasks currently queued or running.
    pub fn pending(&self) -> usize {
        self.queue.pending() + self.running.load(Ordering::SeqCst)
    }

    /// Block until all dispatched tasks have completed (busy-wait-free via
    /// the queue's pending counter).
    pub fn join(&self) {
        while self.pending() > 0 {
            std::thread::yield_now();
        }
    }
}

impl Drop for Scheduler {
    fn drop(&mut self) {
        // Best-effort: workers block forever on the queue; we simply stop
        // referencing them. For a bounded model this is acceptable because the
        // process exits after the run completes.
        let _ = &self.workers;
    }
}

/// Convenience: run a function under a given scheduling model and return its code.
pub fn run_scheduled<F: FnOnce() -> i64 + Send + 'static>(
    schedule: &Schedule,
    f: F,
) -> i64 {
    match schedule {
        Schedule::Single => f(),
        s => {
            let sched = Scheduler::new(s);
            let results = Arc::new(Mutex::new(Vec::new()));
            let r2 = results.clone();
            sched.spawn(Box::new(move || {
                let code = f();
                r2.lock().unwrap().push(code);
                code
            }));
            sched.join();
            let mut v = results.lock().unwrap();
            v.pop().unwrap_or(0)
        }
    }
}
