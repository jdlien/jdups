//! A stop signal a thread can sleep against.
//!
//! The `AtomicBool` this replaces could only be *polled*, so every sleep in
//! the three long-running loops was sliced into 100 ms naps to notice a stop
//! promptly -- ten scheduler wakeups a second, per sleeper, spent observing
//! that nothing had happened. The service went one further and kept a whole
//! thread doing nothing else. A condvar carries the same one-way "stop now"
//! bit and lets a sleeper park for its full duration while still waking
//! within a scheduler quantum of the signal.
//!
//! The signal is sticky and idempotent: once stopped, always stopped, and a
//! second `stop()` is harmless. Both mattered to the callers this was built
//! for -- a console control handler and the SCM handler can race the loop's
//! own teardown freely.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct Stop {
    stopped: Mutex<bool>,
    cv: Condvar,
}

impl Stop {
    pub fn new() -> Stop {
        Stop::default()
    }

    /// Signal the stop. Callable from any thread; console control handlers
    /// and the SCM control handler both run on ordinary threads, so taking a
    /// mutex here is fine.
    pub fn stop(&self) {
        *self.stopped.lock().unwrap() = true;
        self.cv.notify_all();
    }

    pub fn is_stopped(&self) -> bool {
        *self.stopped.lock().unwrap()
    }

    /// Sleep for `d`, or until stopped, whichever comes first. Returns whether
    /// the stop has been signalled.
    ///
    /// Spurious condvar wakeups re-wait for the remainder rather than
    /// returning early: a caller pacing a loop off this must get its full
    /// sleep, or the loop runs hot exactly the way the 100 ms slices did.
    pub fn wait_for(&self, d: Duration) -> bool {
        let deadline = Instant::now().checked_add(d);
        let mut stopped = self.stopped.lock().unwrap();
        while !*stopped {
            let left = match deadline {
                Some(t) => t.saturating_duration_since(Instant::now()),
                // A duration too large for the clock is "forever": keep
                // waiting in bounded slices until stopped.
                None => Duration::from_secs(3600),
            };
            if left.is_zero() {
                return false;
            }
            let (guard, _) = self.cv.wait_timeout(stopped, left).unwrap();
            stopped = guard;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn a_stop_interrupts_a_sleep_promptly() {
        let s = Arc::new(Stop::new());
        let signaller = Arc::clone(&s);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            signaller.stop();
        });
        let began = Instant::now();
        let stopped = s.wait_for(Duration::from_secs(30));
        assert!(stopped, "the signal was missed");
        assert!(
            began.elapsed() < Duration::from_secs(5),
            "a 30 ms stop took {:?} to land",
            began.elapsed()
        );
        t.join().unwrap();
    }

    #[test]
    fn an_unstopped_wait_sleeps_its_full_duration() {
        let s = Stop::new();
        let began = Instant::now();
        let stopped = s.wait_for(Duration::from_millis(60));
        assert!(!stopped);
        assert!(
            began.elapsed() >= Duration::from_millis(55),
            "woke after only {:?}",
            began.elapsed()
        );
    }

    #[test]
    fn a_stop_is_sticky_and_idempotent() {
        let s = Stop::new();
        s.stop();
        s.stop();
        assert!(s.is_stopped());
        let began = Instant::now();
        assert!(s.wait_for(Duration::from_secs(30)), "stopped but slept anyway");
        assert!(began.elapsed() < Duration::from_secs(1));
    }
}
