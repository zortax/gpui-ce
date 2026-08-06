//! Running gpui's tasks: a worker pool, a timer thread, and the winit event loop as the main
//! thread.
//!
//! gpui hands the platform two kinds of work. Background runnables may run anywhere and go to a
//! pool of worker threads. Main-thread runnables must run where the windows live, which here is
//! inside the winit event loop — so they are queued and the loop is woken through its proxy, and
//! the application handler drains them.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::sync::Arc;
use std::thread::{self, ThreadId};
use std::time::{Duration, Instant};

use gpui::{
    PlatformDispatcher, Priority, PriorityQueueReceiver, PriorityQueueSender, RunnableVariant,
};
use parking_lot::{Condvar, Mutex};
use winit::event_loop::EventLoopProxy;

use crate::platform::UserEvent;

/// The fewest worker threads to run, however few cores are reported.
///
/// One worker can deadlock: a background task that blocks on another background task has nobody
/// to run it.
const MIN_WORKERS: usize = 2;

/// Work waiting to run on the thread that owns the windows.
#[derive(Default)]
pub struct MainQueue {
    /// Higher-priority work runs first; within a class, work runs in the order it arrived.
    high: Vec<RunnableVariant>,
    rest: Vec<RunnableVariant>,
}

impl MainQueue {
    fn push(&mut self, runnable: RunnableVariant, priority: Priority) {
        match priority {
            Priority::RealtimeAudio | Priority::High => self.high.push(runnable),
            _ => self.rest.push(runnable),
        }
    }

    /// Everything queued so far, in the order it should run.
    ///
    /// Draining rather than running in place matters: a runnable is free to queue more work, and
    /// running under the lock would deadlock the moment one did.
    pub fn drain(&mut self) -> Vec<RunnableVariant> {
        let mut taken = std::mem::take(&mut self.high);
        taken.append(&mut self.rest);
        taken
    }

    fn is_empty(&self) -> bool {
        self.high.is_empty() && self.rest.is_empty()
    }
}

/// A timer that has not fired yet.
struct Pending {
    at: Instant,
    runnable: RunnableVariant,
}

impl PartialEq for Pending {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at
    }
}
impl Eq for Pending {}
impl PartialOrd for Pending {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Pending {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at.cmp(&other.at)
    }
}

#[derive(Default)]
struct Timers {
    /// Soonest first, which is what `Reverse` on a max-heap buys.
    pending: BinaryHeap<Reverse<Pending>>,
    stopped: bool,
}

/// gpui's [`PlatformDispatcher`], over winit.
pub struct ZguiDispatcher {
    main: Arc<Mutex<MainQueue>>,
    proxy: EventLoopProxy<UserEvent>,
    background: PriorityQueueSender<RunnableVariant>,
    timers: Arc<(Mutex<Timers>, Condvar)>,
    main_thread: ThreadId,
    _workers: Vec<thread::JoinHandle<()>>,
    _timer_thread: thread::JoinHandle<()>,
}

impl ZguiDispatcher {
    /// A dispatcher whose main thread is the one this is called on.
    pub fn new(proxy: EventLoopProxy<UserEvent>, main: Arc<Mutex<MainQueue>>) -> Self {
        let (background, receiver) = PriorityQueueReceiver::new();
        let workers = (0..worker_count())
            .map(|index| {
                let receiver: PriorityQueueReceiver<RunnableVariant> = receiver.clone();
                thread::Builder::new()
                    .name(format!("gpui-zgui-worker-{index}"))
                    .spawn(move || {
                        for runnable in receiver.iter() {
                            runnable.run();
                        }
                    })
                    .expect("spawning a worker thread")
            })
            .collect();
        drop(receiver);

        let timers = Arc::new((Mutex::new(Timers::default()), Condvar::new()));
        let timer_thread = {
            let timers = timers.clone();
            thread::Builder::new()
                .name("gpui-zgui-timer".to_owned())
                .spawn(move || run_timers(&timers))
                .expect("spawning the timer thread")
        };

        Self {
            main,
            proxy,
            background,
            timers,
            main_thread: thread::current().id(),
            _workers: workers,
            _timer_thread: timer_thread,
        }
    }

    /// Queues main-thread work and wakes the event loop to run it.
    fn queue_main(&self, runnable: RunnableVariant, priority: Priority) {
        let was_empty = {
            let mut main = self.main.lock();
            let was_empty = main.is_empty();
            main.push(runnable, priority);
            was_empty
        };
        // Waking on every push would flood the loop's user-event channel under a burst of task
        // wake-ups; one wake per non-empty transition is enough, because the drain takes
        // everything queued by the time it runs.
        if was_empty {
            let _ = self.proxy.send_event(UserEvent::RunMainTasks);
        }
    }
}

impl PlatformDispatcher for ZguiDispatcher {
    fn is_main_thread(&self) -> bool {
        thread::current().id() == self.main_thread
    }

    fn dispatch(&self, runnable: RunnableVariant, priority: Priority) {
        if self.background.send(priority, runnable).is_err() {
            log::error!("gpui_zgui: the worker pool is gone; a background task was dropped");
        }
    }

    fn dispatch_on_main_thread(&self, runnable: RunnableVariant, priority: Priority) {
        self.queue_main(runnable, priority);
    }

    fn dispatch_after(&self, duration: Duration, runnable: RunnableVariant) {
        let (lock, condvar) = &*self.timers;
        let mut timers = lock.lock();
        timers.pending.push(Reverse(Pending {
            at: Instant::now() + duration,
            runnable,
        }));
        // The new timer may be sooner than the one the thread is currently sleeping on.
        condvar.notify_one();
    }

    fn spawn_realtime(&self, f: Box<dyn FnOnce() + Send>) {
        // Realtime work gets a thread of its own rather than a slot in the shared pool, so it is
        // never queued behind ordinary background work.
        if let Err(error) = thread::Builder::new()
            .name("gpui-zgui-realtime".to_owned())
            .spawn(f)
        {
            log::error!("gpui_zgui: could not spawn a realtime thread: {error}");
        }
    }
}

impl Drop for ZguiDispatcher {
    fn drop(&mut self) {
        let (lock, condvar) = &*self.timers;
        lock.lock().stopped = true;
        condvar.notify_all();
    }
}

fn worker_count() -> usize {
    thread::available_parallelism().map_or(MIN_WORKERS, |count| count.get().max(MIN_WORKERS))
}

/// Sleeps until the soonest timer is due, runs everything that has come due, and repeats.
fn run_timers(timers: &(Mutex<Timers>, Condvar)) {
    let (lock, condvar) = timers;
    loop {
        let mut due = Vec::new();
        {
            let mut state = lock.lock();
            loop {
                if state.stopped {
                    return;
                }
                let now = Instant::now();
                while state
                    .pending
                    .peek()
                    .is_some_and(|Reverse(next)| next.at <= now)
                {
                    due.push(state.pending.pop().expect("just peeked").0.runnable);
                }
                if !due.is_empty() {
                    break;
                }
                match state.pending.peek() {
                    Some(Reverse(next)) => {
                        let wait = next.at.saturating_duration_since(now);
                        condvar.wait_for(&mut state, wait);
                    }
                    None => condvar.wait(&mut state),
                }
            }
        }
        // Outside the lock: a timer's runnable is free to schedule another timer.
        for runnable in due {
            runnable.run();
        }
    }
}
