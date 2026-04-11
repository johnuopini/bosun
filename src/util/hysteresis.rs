//! Status transition smoothing.
//!
//! Problem: raw detector output can flip-flop between `Running` and
//! `Waiting` once per poll as the pane's last line toggles. The UI
//! needs to stay calm.
//!
//! Rule:
//!   * Running → Waiting: **instant**. The user just hit a prompt;
//!     they want the glyph to change right away.
//!   * Anything else: require `STREAK` consecutive polls showing the
//!     new status before we actually transition.
//!
//! Pure, per-session state is just a `u8` streak counter.

use crate::tmux::detector::Status;

pub const STREAK: u8 = 2;

#[derive(Debug, Default, Clone)]
pub struct Smoother {
    current: Status,
    candidate: Status,
    streak: u8,
}

impl Smoother {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a raw detector result. Returns the smoothed status that
    /// the UI should actually render.
    pub fn observe(&mut self, raw: Status) -> Status {
        // First observation ever — trust it.
        if self.current == Status::Unknown {
            self.current = raw;
            self.candidate = raw;
            self.streak = STREAK;
            return self.current;
        }

        // Instant transitions (user-visible changes).
        if self.current == Status::Running && raw == Status::Waiting {
            self.current = Status::Waiting;
            self.candidate = Status::Waiting;
            self.streak = STREAK;
            return self.current;
        }

        // No change — reset any pending transition.
        if raw == self.current {
            self.candidate = self.current;
            self.streak = STREAK;
            return self.current;
        }

        // Unknown from a raw poll should not disturb the current state.
        if raw == Status::Unknown {
            return self.current;
        }

        // New candidate — require a streak.
        if raw != self.candidate {
            self.candidate = raw;
            self.streak = 1;
        } else {
            self.streak = self.streak.saturating_add(1);
        }

        if self.streak >= STREAK {
            self.current = self.candidate;
        }
        self.current
    }

    pub fn current(&self) -> Status {
        self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_observation_sticks() {
        let mut s = Smoother::new();
        assert_eq!(s.observe(Status::Idle), Status::Idle);
    }

    #[test]
    fn running_to_waiting_is_instant() {
        let mut s = Smoother::new();
        s.observe(Status::Running);
        assert_eq!(s.observe(Status::Waiting), Status::Waiting);
    }

    #[test]
    fn idle_to_running_needs_streak() {
        let mut s = Smoother::new();
        s.observe(Status::Idle);
        assert_eq!(s.observe(Status::Running), Status::Idle); // 1st, not enough
        assert_eq!(s.observe(Status::Running), Status::Running); // 2nd, commits
    }

    #[test]
    fn flipflop_resets_streak() {
        let mut s = Smoother::new();
        s.observe(Status::Idle);
        s.observe(Status::Running); // streak=1 for Running
        assert_eq!(s.observe(Status::Idle), Status::Idle); // reset to current
        assert_eq!(s.observe(Status::Running), Status::Idle); // streak=1 again
        assert_eq!(s.observe(Status::Running), Status::Running); // streak=2 commits
    }

    #[test]
    fn unknown_does_not_disturb_state() {
        let mut s = Smoother::new();
        s.observe(Status::Running);
        assert_eq!(s.observe(Status::Unknown), Status::Running);
        assert_eq!(s.observe(Status::Unknown), Status::Running);
    }
}
