//! Deliver-policy → positioning decision table (pure)
//!
//! Deriving a subscription's starting position is a pure decision: given the
//! [`DeliverPolicy`] and whether the consumer resumed from a stored offset,
//! decide (a) whether a partition-head probe is needed at subscribe time,
//! (b) what to do with the probed head, (c) an explicit start offset, and
//! (d) whether the first poll positions by timestamp. Keeping the table pure
//! makes the full matrix unit-testable without a broker.
//!
//! Server contract these decisions encode (Iggy 0.9):
//! - stored offsets are the **last consumed** offset; resume = stored + 1
//! - `New`/`Last` must probe at subscribe time — positioning from the first
//!   poll's messages would skip (or deliver) events published after the
//!   subscription but before the first poll ran.

use crate::types::DeliverPolicy;

/// What to do with the partition head probed at subscribe time
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadProbe {
    /// No probe: the start offset or timestamp already positions the cursor
    None,
    /// `DeliverPolicy::New`: discard the head, continue after it
    SkipToAfterHead,
    /// `DeliverPolicy::Last`: deliver the head, then continue after it
    DeliverHead,
}

/// Positioning decision for a fresh (non-resumed) or resumed consumer
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PositionPlan {
    /// Subscribe-time head probe behavior
    pub probe: HeadProbe,
    /// Explicit start offset (ignored when a probe or timestamp applies)
    pub start_offset: u64,
    /// First-poll timestamp positioning in Unix millis (`ByStartTime`)
    pub initial_timestamp_ms: Option<u64>,
}

impl PositionPlan {
    /// The plan for a consumer that resumed from a stored offset.
    ///
    /// The stored offset wins over every deliver policy: the caller seeds
    /// the cursor with `stored + 1` and nothing is probed.
    pub fn resumed() -> Self {
        Self {
            probe: HeadProbe::None,
            start_offset: 0, // caller overrides with stored + 1
            initial_timestamp_ms: None,
        }
    }
}

/// Decide positioning for one topic.
///
/// `resumed` means a stored offset exists for this topic — the policy then
/// only affects a consumer that never committed anything.
pub fn position_plan(policy: &DeliverPolicy, resumed: bool) -> PositionPlan {
    if resumed {
        return PositionPlan::resumed();
    }

    match policy {
        DeliverPolicy::All => PositionPlan {
            probe: HeadProbe::None,
            start_offset: 0,
            initial_timestamp_ms: None,
        },
        DeliverPolicy::ByStartSequence { sequence } => PositionPlan {
            probe: HeadProbe::None,
            start_offset: *sequence,
            initial_timestamp_ms: None,
        },
        DeliverPolicy::ByStartTime { timestamp } => PositionPlan {
            probe: HeadProbe::None,
            start_offset: 0,
            initial_timestamp_ms: Some(*timestamp),
        },
        DeliverPolicy::New => PositionPlan {
            probe: HeadProbe::SkipToAfterHead,
            start_offset: 0,
            initial_timestamp_ms: None,
        },
        DeliverPolicy::Last => PositionPlan {
            probe: HeadProbe::DeliverHead,
            start_offset: 0,
            initial_timestamp_ms: None,
        },
        // Rejected upstream (no cheap Iggy equivalent); mapped to All here so
        // the table stays total for callers that bypass the rejection.
        DeliverPolicy::LastPerSubject => PositionPlan {
            probe: HeadProbe::None,
            start_offset: 0,
            initial_timestamp_ms: None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_reads_from_zero() {
        let plan = position_plan(&DeliverPolicy::All, false);
        assert_eq!(plan.probe, HeadProbe::None);
        assert_eq!(plan.start_offset, 0);
        assert_eq!(plan.initial_timestamp_ms, None);
    }

    #[test]
    fn by_start_sequence_pins_the_offset() {
        let plan = position_plan(&DeliverPolicy::ByStartSequence { sequence: 42 }, false);
        assert_eq!(plan.probe, HeadProbe::None);
        assert_eq!(plan.start_offset, 42);
        assert_eq!(plan.initial_timestamp_ms, None);
    }

    #[test]
    fn by_start_time_positions_first_poll_only() {
        let plan = position_plan(&DeliverPolicy::ByStartTime { timestamp: 1234 }, false);
        assert_eq!(plan.probe, HeadProbe::None);
        assert_eq!(plan.initial_timestamp_ms, Some(1234));
    }

    #[test]
    fn new_probes_and_skips_head() {
        let plan = position_plan(&DeliverPolicy::New, false);
        assert_eq!(plan.probe, HeadProbe::SkipToAfterHead);
        assert_eq!(plan.initial_timestamp_ms, None);
    }

    #[test]
    fn last_probes_and_delivers_head() {
        let plan = position_plan(&DeliverPolicy::Last, false);
        assert_eq!(plan.probe, HeadProbe::DeliverHead);
    }

    #[test]
    fn resumed_overrides_every_policy() {
        for policy in [
            DeliverPolicy::All,
            DeliverPolicy::Last,
            DeliverPolicy::New,
            DeliverPolicy::ByStartSequence { sequence: 9 },
            DeliverPolicy::ByStartTime { timestamp: 9 },
            DeliverPolicy::LastPerSubject,
        ] {
            let plan = position_plan(&policy, true);
            assert_eq!(plan, PositionPlan::resumed(), "policy {policy:?}");
        }
    }

    #[test]
    fn unsupported_policy_falls_back_to_all_in_the_table() {
        // The provider rejects LastPerSubject before positioning; the table
        // itself stays total and degrades to All semantics.
        let plan = position_plan(&DeliverPolicy::LastPerSubject, false);
        assert_eq!(plan.probe, HeadProbe::None);
        assert_eq!(plan.start_offset, 0);
        assert_eq!(plan.initial_timestamp_ms, None);
    }
}
