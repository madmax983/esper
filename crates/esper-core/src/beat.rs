//! The heartbeat: a durable, monotonic beat counter (SPEC §21.1).
//!
//! A [`Beat`] is the always-on supervisor's logical clock. It is not
//! wall time: the host's tick source decides *when* a beat happens,
//! the beat number decides *what* happens. Cadences, backoffs, and
//! liveness are all measured in beats, so nothing here claims elapsed
//! time across power loss (§7.3, the Waymaker durable-time rule).
//!
//! Arithmetic fails closed: [`Beat::after`] and [`Beat::next`] return
//! [`Error::BeatOverflow`] instead of wrapping, so a deadline can
//! never silently land in the past.

use core::fmt::{Display, Formatter, Result as FmtResult};

use crate::error::Error;

/// One tick of the supervisor's durable logical clock.
///
/// Beat `0` means "no beat has run yet"; the first beat is `1`. The
/// supervisor commits `BeatStarted { beat }` before any of the beat's
/// work, and a reset inside a beat finishes that same beat — the
/// counter never skips and never repeats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Beat(u64);

impl Beat {
    /// The "no beat yet" origin.
    pub const ZERO: Self = Self(0);

    /// Build a `Beat` from its raw value.
    #[must_use]
    pub const fn new(beat: u64) -> Self {
        Self(beat)
    }

    /// Return the raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// The beat after this one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BeatOverflow`] at `u64::MAX`.
    pub const fn next(self) -> Result<Self, Error> {
        match self.0.checked_add(1) {
            Some(n) => Ok(Self(n)),
            None => Err(Error::BeatOverflow),
        }
    }

    /// The beat `span` beats after this one.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BeatOverflow`] when the sum overflows `u64`.
    pub const fn after(self, span: BeatSpan) -> Result<Self, Error> {
        match self.0.checked_add(span.get() as u64) {
            Some(n) => Ok(Self(n)),
            None => Err(Error::BeatOverflow),
        }
    }

    /// Beats elapsed from `earlier` to `self`, saturating at zero when
    /// `earlier` is later.
    #[must_use]
    pub const fn since(self, earlier: Self) -> u64 {
        self.0.saturating_sub(earlier.0)
    }
}

impl Display for Beat {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "beat-{}", self.0)
    }
}

/// A nonzero distance in beats: a cadence, a backoff, a liveness
/// threshold.
///
/// Zero is unrepresentable, so a cadence can never reschedule a goal
/// into the beat that just ran it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BeatSpan(u32);

impl BeatSpan {
    /// One beat.
    pub const ONE: Self = Self(1);

    /// Build a span, rejecting zero.
    #[must_use]
    pub const fn new(beats: u32) -> Option<Self> {
        if beats == 0 { None } else { Some(Self(beats)) }
    }

    /// Return the raw beat count (always `>= 1`).
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }
}

impl Display for BeatSpan {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{} beats", self.0)
    }
}

/// The supervisor's scheduling policy (SPEC §21.2): failure handling
/// and the liveness threshold. Part of the agent's identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SchedulePolicy {
    /// Consecutive failed runs before a goal ends `Failed` (`>= 1`).
    pub max_consecutive_failures: u8,
    /// The first retry's backoff.
    pub backoff_base: BeatSpan,
    /// The longest backoff; doubling stops here.
    pub backoff_cap: BeatSpan,
    /// Beats a goal may wait on human input before it counts as
    /// overdue. A signal, never a timeout (SPEC §21.1).
    pub overdue_after: BeatSpan,
}

impl SchedulePolicy {
    /// Defaults: fail after 3 consecutive failures, back off 1, 2, 4 …
    /// beats up to 16, flag a goal overdue after 10 beats of waiting.
    /// Tuning defaults, not protocol constants.
    pub const DEFAULT: Self = Self {
        max_consecutive_failures: 3,
        backoff_base: BeatSpan(1),
        backoff_cap: BeatSpan(16),
        overdue_after: BeatSpan(10),
    };

    /// Validate the policy.
    ///
    /// # Errors
    ///
    /// Returns [`Error::GoalSpecInvalid`] when no failure is tolerated
    /// or the base backoff exceeds the cap.
    pub const fn validate(&self) -> Result<(), Error> {
        if self.max_consecutive_failures == 0 {
            return Err(Error::GoalSpecInvalid(
                "max_consecutive_failures must be at least 1",
            ));
        }
        if self.backoff_base.get() > self.backoff_cap.get() {
            return Err(Error::GoalSpecInvalid(
                "backoff_base must not exceed backoff_cap",
            ));
        }
        Ok(())
    }

    /// The backoff after the `streak`-th consecutive failure:
    /// `min(base · 2^(streak−1), cap)`, saturating. A streak of `0` or
    /// `1` yields the base.
    #[must_use]
    pub fn backoff(&self, streak: u8) -> BeatSpan {
        let shift = u32::from(streak.saturating_sub(1));
        let base = u64::from(self.backoff_base.get());
        let cap = u64::from(self.backoff_cap.get());
        // A shift of 32 or more already exceeds any u32 cap; clamp it so
        // the shift itself cannot overflow.
        let scaled = if shift >= 32 {
            cap
        } else {
            (base << shift).min(cap)
        };
        // `base <= scaled <= cap <= u32::MAX`, so this never falls back.
        u32::try_from(scaled)
            .ok()
            .and_then(BeatSpan::new)
            .unwrap_or(self.backoff_cap)
    }
}

#[cfg(test)]
mod tests {
    use super::{Beat, BeatSpan, SchedulePolicy};
    use crate::error::Error;

    #[test]
    fn span_rejects_zero() {
        assert_eq!(BeatSpan::new(0), None);
        assert_eq!(BeatSpan::new(1), Some(BeatSpan::ONE));
    }

    #[test]
    fn beat_arithmetic_fails_closed() {
        assert_eq!(Beat::new(u64::MAX).next(), Err(Error::BeatOverflow));
        assert_eq!(
            Beat::new(u64::MAX - 1).after(BeatSpan::new(2).expect("span")),
            Err(Error::BeatOverflow)
        );
        assert_eq!(Beat::ZERO.next(), Ok(Beat::new(1)));
        assert_eq!(Beat::new(3).since(Beat::new(5)), 0);
        assert_eq!(Beat::new(9).since(Beat::new(5)), 4);
    }

    #[test]
    fn backoff_doubles_then_caps() {
        let policy = SchedulePolicy::DEFAULT;
        let got: [u32; 7] = [0u8, 1, 2, 3, 4, 5, 6].map(|k| policy.backoff(k).get());
        assert_eq!(got, [1, 1, 2, 4, 8, 16, 16]);
        // Huge streaks saturate at the cap instead of overflowing.
        assert_eq!(policy.backoff(u8::MAX), policy.backoff_cap);
        let wide = SchedulePolicy {
            backoff_base: BeatSpan::new(u32::MAX).expect("span"),
            backoff_cap: BeatSpan::new(u32::MAX).expect("span"),
            ..policy
        };
        assert_eq!(wide.backoff(40).get(), u32::MAX);
    }

    #[test]
    fn backoff_is_monotonic_and_bounded() {
        for base in 1..=8u32 {
            for cap in base..=40u32 {
                let policy = SchedulePolicy {
                    backoff_base: BeatSpan::new(base).expect("span"),
                    backoff_cap: BeatSpan::new(cap).expect("span"),
                    ..SchedulePolicy::DEFAULT
                };
                let mut prev = 0;
                for streak in 0..=u8::MAX {
                    let span = policy.backoff(streak).get();
                    assert!(span >= base && span <= cap);
                    assert!(span >= prev, "backoff must never shrink");
                    prev = span;
                }
            }
        }
    }

    #[test]
    fn policy_validation() {
        SchedulePolicy::DEFAULT.validate().expect("default policy");
        let none = SchedulePolicy {
            max_consecutive_failures: 0,
            ..SchedulePolicy::DEFAULT
        };
        assert!(none.validate().is_err());
        let inverted = SchedulePolicy {
            backoff_base: BeatSpan::new(9).expect("span"),
            backoff_cap: BeatSpan::new(8).expect("span"),
            ..SchedulePolicy::DEFAULT
        };
        assert!(inverted.validate().is_err());
    }
}
