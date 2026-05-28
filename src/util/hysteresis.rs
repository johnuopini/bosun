//! Status transition smoothing.
//!
//! Problem: raw detector output can flip-flop between `Running` and
//! `Waiting` once per poll as the pane's last line toggles. The UI
//! needs to stay calm.
//!
//! Rules:
//!   * → Running or → Waiting: **instant**. These are high-signal
//!     events — the user wants to see "agent is working" or "agent
//!     wants my input" the moment it happens. Tuned for the 200ms
//!     fast-tick cadence where instant + accurate detectors is more
//!     valuable than additional latency-trading hysteresis.
//!   * → Idle: require `STREAK` consecutive polls. Filters out the
//!     brief quiet windows that show up between an agent's bursts of
//!     output — without this the Running glyph would flicker off
//!     every time the model paused to think.
//!   * Unknown: ignored — never disturbs the current state.
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

        // Unknown from a raw poll should not disturb the current state.
        if raw == Status::Unknown {
            return self.current;
        }

        // No change — reset any pending transition.
        if raw == self.current {
            self.candidate = self.current;
            self.streak = STREAK;
            return self.current;
        }

        // Instant promotion to an "active" state. The user wants to
        // see Running / Waiting the moment a detector is confident,
        // not after N polls of confirmation — latency here is the
        // whole point of the live-status push.
        if raw == Status::Running || raw == Status::Waiting {
            self.current = raw;
            self.candidate = raw;
            self.streak = STREAK;
            return self.current;
        }

        // Demotion (typically → Idle) needs a streak so a brief quiet
        // window between agent bursts doesn't toggle the glyph off.
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
    fn idle_to_running_is_instant() {
        // Promotions to active states must be instant at fast-tick
        // cadence — the user is watching for "agent woke up".
        let mut s = Smoother::new();
        s.observe(Status::Idle);
        assert_eq!(s.observe(Status::Running), Status::Running);
    }

    #[test]
    fn idle_to_waiting_is_instant() {
        let mut s = Smoother::new();
        s.observe(Status::Idle);
        assert_eq!(s.observe(Status::Waiting), Status::Waiting);
    }

    #[test]
    fn waiting_to_running_is_instant() {
        let mut s = Smoother::new();
        s.observe(Status::Waiting);
        assert_eq!(s.observe(Status::Running), Status::Running);
    }

    #[test]
    fn running_to_idle_needs_streak() {
        // Demotion to Idle keeps hysteresis so a brief quiet window
        // mid-burst doesn't flicker the Running glyph off.
        let mut s = Smoother::new();
        s.observe(Status::Running);
        assert_eq!(s.observe(Status::Idle), Status::Running); // 1st: hold
        assert_eq!(s.observe(Status::Idle), Status::Idle); // 2nd: commit
    }

    #[test]
    fn idle_flipflop_during_demotion_resets_streak() {
        let mut s = Smoother::new();
        s.observe(Status::Running);
        assert_eq!(s.observe(Status::Idle), Status::Running); // streak=1
                                                              // A reading agreeing with current resets the pending demotion.
        assert_eq!(s.observe(Status::Running), Status::Running);
        assert_eq!(s.observe(Status::Idle), Status::Running); // streak=1 again
        assert_eq!(s.observe(Status::Idle), Status::Idle); // streak=2 commits
    }

    #[test]
    fn unknown_does_not_disturb_state() {
        let mut s = Smoother::new();
        s.observe(Status::Running);
        assert_eq!(s.observe(Status::Unknown), Status::Running);
        assert_eq!(s.observe(Status::Unknown), Status::Running);
    }
}
