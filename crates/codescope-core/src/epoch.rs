//! Repository-state generation counter.

use std::fmt;

/// Monotonic generation of the repository state (HEAD, index, worktree, LSP snapshot).
///
/// The dispatcher bumps the epoch every time it accepts a new change-set; every async job
/// captures the epoch at spawn and drops stale results at apply time (research 06). AI plans
/// echo the epoch they were generated against so the validator can gate stale plans
/// (research 05 §3).
#[derive(
    Debug,
    Default,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    serde::Serialize,
    serde::Deserialize,
)]
#[serde(transparent)]
pub struct Epoch(pub u64);

impl Epoch {
    /// The very first epoch, before any change-set has been accepted.
    pub const ZERO: Epoch = Epoch(0);

    /// The next epoch after `self`.
    #[must_use]
    pub fn next(self) -> Epoch {
        Epoch(self.0 + 1)
    }

    /// The raw counter value.
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for Epoch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "epoch-{}", self.0)
    }
}

impl From<u64> for Epoch {
    fn from(v: u64) -> Self {
        Epoch(v)
    }
}

impl From<Epoch> for u64 {
    fn from(e: Epoch) -> Self {
        e.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordering_and_next() {
        assert!(Epoch(1) > Epoch(0));
        assert_eq!(Epoch(0).next(), Epoch(1));
        assert_eq!(Epoch::ZERO.get(), 0);
        assert_eq!(Epoch(41).next().get(), 42);
    }

    #[test]
    fn display() {
        assert_eq!(Epoch(7).to_string(), "epoch-7");
    }
}
