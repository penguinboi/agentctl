use std::collections::BTreeSet;

use agentctl_core::{CanonicalEvent, EventId, EventVisibility, ProviderSessionId, SyncBatch};
use serde::{Deserialize, Serialize};

use crate::{ContextCheckpoint, Result, TranscriptError};

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct ReceiptKey {
    pub provider_session_id: ProviderSessionId,
    pub canonical_event_id: EventId,
    pub projection_version: u32,
}

#[derive(Clone, Debug, Default)]
pub struct IdempotencyLedger {
    applied: BTreeSet<ReceiptKey>,
}

impl IdempotencyLedger {
    pub fn new(keys: impl IntoIterator<Item = ReceiptKey>) -> Self {
        Self {
            applied: keys.into_iter().collect(),
        }
    }

    pub fn contains(&self, key: &ReceiptKey) -> bool {
        self.applied.contains(key)
    }

    pub fn record(&mut self, key: ReceiptKey) -> bool {
        self.applied.insert(key)
    }

    pub fn len(&self) -> usize {
        self.applied.len()
    }

    pub fn is_empty(&self) -> bool {
        self.applied.is_empty()
    }
}

#[derive(Clone, Debug)]
pub struct ProjectionRequest<'a> {
    pub provider_session_id: ProviderSessionId,
    pub last_synced_seq: u64,
    pub through_seq: u64,
    /// Latest sequence in the complete canonical log. `events` may be a sparse,
    /// checkpoint-bounded projection window.
    pub canonical_latest_seq: u64,
    pub projection_version: u32,
    pub events: &'a [CanonicalEvent],
    pub handoff: Option<String>,
}

/// Selects the bounded canonical window used to rebuild a lagging native
/// projection. Once a deterministic checkpoint covers the provider's lag, old
/// events are replaced by that checkpoint plus its explicitly retained recent
/// events and the post-checkpoint delta.
pub fn projection_window(
    events: &[CanonicalEvent],
    last_synced_seq: u64,
    through_seq: u64,
    checkpoint: Option<&ContextCheckpoint>,
) -> Result<Vec<CanonicalEvent>> {
    validate_order(events)?;
    let latest_seq = events.last().map_or(last_synced_seq, |event| event.seq);
    if through_seq > latest_seq {
        return Err(TranscriptError::ProjectionOutOfRange {
            through_seq,
            latest_seq,
        });
    }
    validate_sync_progress(last_synced_seq, through_seq)?;

    let applicable = checkpoint.filter(|checkpoint| {
        last_synced_seq < checkpoint.through_seq && checkpoint.through_seq <= through_seq
    });
    let retained = applicable.map(|checkpoint| {
        checkpoint
            .retained_event_ids
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
    });

    Ok(events
        .iter()
        .filter(|event| event.seq > last_synced_seq && event.seq <= through_seq)
        .filter(|event| {
            applicable.is_none_or(|checkpoint| {
                event.seq > checkpoint.through_seq
                    || retained
                        .as_ref()
                        .is_some_and(|ids| ids.contains(&event.event_id))
            })
        })
        .cloned()
        .collect())
}

pub fn build_sync_batch(
    request: ProjectionRequest<'_>,
    receipts: &IdempotencyLedger,
) -> Result<SyncBatch> {
    validate_order(request.events)?;
    if request.through_seq > request.canonical_latest_seq {
        return Err(TranscriptError::ProjectionOutOfRange {
            through_seq: request.through_seq,
            latest_seq: request.canonical_latest_seq,
        });
    }
    validate_sync_progress(request.last_synced_seq, request.through_seq)?;
    let events = request
        .events
        .iter()
        .filter(|event| {
            event.seq > request.last_synced_seq
                && event.seq <= request.through_seq
                && event.visibility != EventVisibility::Internal
                && !receipts.contains(&ReceiptKey {
                    provider_session_id: request.provider_session_id,
                    canonical_event_id: event.event_id,
                    projection_version: request.projection_version,
                })
        })
        .cloned()
        .collect();
    Ok(SyncBatch {
        from_seq_exclusive: request.last_synced_seq,
        through_seq_inclusive: request.through_seq,
        projection_version: request.projection_version,
        events,
        handoff: request.handoff,
    })
}

pub fn validate_sync_progress(current: u64, proposed: u64) -> Result<()> {
    if proposed < current {
        Err(TranscriptError::SyncRegression { current, proposed })
    } else {
        Ok(())
    }
}

pub fn sync_lag(latest_canonical_seq: u64, last_synced_seq: u64) -> u64 {
    latest_canonical_seq.saturating_sub(last_synced_seq)
}

fn validate_order(events: &[CanonicalEvent]) -> Result<()> {
    let mut previous = 0;
    for event in events {
        if event.seq <= previous {
            return Err(TranscriptError::NonMonotonic(event.seq));
        }
        previous = event.seq;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use agentctl_core::{EventVisibility, UnifiedSessionId};
    use chrono::Utc;

    use super::*;

    fn event(seq: u64) -> CanonicalEvent {
        CanonicalEvent {
            schema_version: 1,
            session_id: UnifiedSessionId::new(),
            seq,
            event_id: EventId::new(),
            turn_id: None,
            origin_provider: None,
            kind: "event".to_owned(),
            visibility: EventVisibility::User,
            payload: serde_json::json!({}),
            content_hash: format!("sha256:{seq}"),
            raw_event_id: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn excludes_current_prompt_and_already_applied_events() {
        let provider_session_id = ProviderSessionId::new();
        let events = vec![event(1), event(2), event(3)];
        let ledger = IdempotencyLedger::new([ReceiptKey {
            provider_session_id,
            canonical_event_id: events[1].event_id,
            projection_version: 1,
        }]);
        let batch = build_sync_batch(
            ProjectionRequest {
                provider_session_id,
                last_synced_seq: 0,
                through_seq: 2,
                canonical_latest_seq: 3,
                projection_version: 1,
                events: &events,
                handoff: None,
            },
            &ledger,
        )
        .unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.events[0].seq, 1);
        assert!(batch.events.iter().all(|event| event.seq != 3));
    }

    #[test]
    fn sync_lag_saturates_and_progress_never_regresses() {
        assert_eq!(sync_lag(10, 7), 3);
        assert_eq!(sync_lag(7, 10), 0);
        assert!(validate_sync_progress(10, 9).is_err());
    }

    #[test]
    fn checkpoint_bounds_projection_to_retained_events_and_new_delta() {
        let events = (1..=9).map(event).collect::<Vec<_>>();
        let checkpoint = ContextCheckpoint {
            through_seq: 6,
            retained_event_ids: vec![events[4].event_id, events[5].event_id],
            ..ContextCheckpoint::default()
        };

        let window = projection_window(&events, 0, 9, Some(&checkpoint)).unwrap();
        assert_eq!(
            window.iter().map(|event| event.seq).collect::<Vec<_>>(),
            vec![5, 6, 7, 8, 9]
        );

        let already_past_checkpoint = projection_window(&events, 7, 9, Some(&checkpoint)).unwrap();
        assert_eq!(
            already_past_checkpoint
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![8, 9]
        );
    }

    #[test]
    fn repeated_sync_is_idempotent_and_next_sync_is_delta_only() {
        let provider_session_id = ProviderSessionId::new();
        let events = vec![event(1), event(2), event(3)];
        let first = build_sync_batch(
            ProjectionRequest {
                provider_session_id,
                last_synced_seq: 0,
                through_seq: 2,
                canonical_latest_seq: 3,
                projection_version: 7,
                events: &events,
                handoff: None,
            },
            &IdempotencyLedger::default(),
        )
        .unwrap();
        assert_eq!(
            first
                .events
                .iter()
                .map(|event| event.seq)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );

        let receipts = IdempotencyLedger::new(first.events.iter().map(|event| ReceiptKey {
            provider_session_id,
            canonical_event_id: event.event_id,
            projection_version: 7,
        }));
        let repeated = build_sync_batch(
            ProjectionRequest {
                provider_session_id,
                last_synced_seq: 0,
                through_seq: 2,
                canonical_latest_seq: 3,
                projection_version: 7,
                events: &events,
                handoff: None,
            },
            &receipts,
        )
        .unwrap();
        assert!(repeated.events.is_empty());

        let delta = build_sync_batch(
            ProjectionRequest {
                provider_session_id,
                last_synced_seq: 2,
                through_seq: 3,
                canonical_latest_seq: 3,
                projection_version: 7,
                events: &events,
                handoff: None,
            },
            &receipts,
        )
        .unwrap();
        assert_eq!(delta.events.len(), 1);
        assert_eq!(delta.events[0].seq, 3);
    }

    #[test]
    fn sparse_checkpoint_window_can_advance_through_canonical_gap() {
        let provider_session_id = ProviderSessionId::new();
        let retained = vec![event(8)];
        let batch = build_sync_batch(
            ProjectionRequest {
                provider_session_id,
                last_synced_seq: 0,
                through_seq: 10,
                canonical_latest_seq: 10,
                projection_version: 2,
                events: &retained,
                handoff: Some("checkpoint".to_owned()),
            },
            &IdempotencyLedger::default(),
        )
        .unwrap();
        assert_eq!(batch.events.len(), 1);
        assert_eq!(batch.through_seq_inclusive, 10);
        assert_eq!(batch.handoff.as_deref(), Some("checkpoint"));
    }
}
