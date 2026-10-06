use std::time::{Duration, Instant};

/// How a round of window cleanup checks that windows are alive.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CleanupCheck {
  /// Check against one window server listing, and ask only the windows
  /// it lacks.
  ///
  /// Cheap, but blind to a window the application has dropped while the
  /// window server still lists it.
  Listing,

  /// Ask every window for itself.
  ///
  /// Catches what [`CleanupCheck::Listing`] cannot, at the cost of a
  /// request to every application.
  Sweep,
}

/// Decides how each round of window cleanup checks its windows.
///
/// Rounds check against the window server's listing, except one in every
/// [`CleanupSchedule::SWEEP_INTERVAL`], which asks every window. A window
/// that only the sweep can find is therefore pruned within about one
/// interval, while every other round stays off the applications.
#[derive(Debug)]
pub struct CleanupSchedule {
  /// When the last sweep was taken, or when the schedule began.
  last_sweep: Instant,
}

impl CleanupSchedule {
  /// Minimum time between two sweeps of every window.
  ///
  /// Rounds are only as frequent as the cleanup tick, so a sweep can run
  /// up to one tick later than this.
  pub const SWEEP_INTERVAL: Duration = Duration::from_mins(1);

  /// Creates a schedule whose first sweep is due one interval after
  /// `now`.
  #[must_use]
  pub fn new(now: Instant) -> Self {
    Self { last_sweep: now }
  }

  /// Picks the check for a round of cleanup running at `now`.
  ///
  /// A sweep restarts the interval.
  pub fn next_check(&mut self, now: Instant) -> CleanupCheck {
    if now.saturating_duration_since(self.last_sweep)
      < Self::SWEEP_INTERVAL
    {
      return CleanupCheck::Listing;
    }

    self.last_sweep = now;
    CleanupCheck::Sweep
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  /// The cleanup tick of the main loop.
  const TICK: Duration = Duration::from_secs(5);

  /// Rounds check against the listing until a full interval has passed,
  /// then one sweeps, then the interval starts over.
  #[test]
  fn sweeps_once_per_interval_between_listing_rounds() {
    let start = Instant::now();
    let mut schedule = CleanupSchedule::new(start);

    let checks = (1..=24)
      .map(|round| schedule.next_check(start + TICK * round))
      .collect::<Vec<_>>();

    let sweeps = checks
      .iter()
      .enumerate()
      .filter(|(_, check)| **check == CleanupCheck::Sweep)
      .map(|(index, _)| (index + 1) * 5)
      .collect::<Vec<_>>();

    // Rounds at 60 s and 120 s sweep; the 22 others use the listing.
    assert_eq!(sweeps, [60, 120]);
    assert_eq!(
      checks
        .iter()
        .filter(|check| **check == CleanupCheck::Listing)
        .count(),
      22
    );
  }

  /// Nothing sweeps early, and a sweep delayed past the interval (a paused
  /// window manager skips rounds) still happens on the next round.
  #[test]
  fn sweep_is_not_early_and_is_not_skipped_when_late() {
    let start = Instant::now();
    let mut schedule = CleanupSchedule::new(start);

    let almost = start + Duration::from_secs(59);
    assert_eq!(schedule.next_check(almost), CleanupCheck::Listing);

    let late = start + CleanupSchedule::SWEEP_INTERVAL * 5;
    assert_eq!(schedule.next_check(late), CleanupCheck::Sweep);

    // The late sweep restarts the interval from itself.
    assert_eq!(schedule.next_check(late + TICK), CleanupCheck::Listing);
    assert_eq!(
      schedule.next_check(late + CleanupSchedule::SWEEP_INTERVAL),
      CleanupCheck::Sweep
    );
  }
}
