//! Deciding what one service event means before anything is applied.
//!
//! The GUI is one subscriber among several and its connection is not
//! guaranteed to be whole: frames can be replayed, reordered, or dropped by
//! the bounded queue. Everything that decides *whether* a frame is worth
//! applying lives here, as functions over values, so the revision rules and
//! the payload shapes are testable without a running service.
//!
//! Applying the update is [`super::GroveApp::apply_service_event`]'s job: it
//! is the part that needs the whole app.

use grove_core::ipc::Notification;
use grove_core::protocol::{Event, EventKind};
use grove_core::reconcile::Reconciliation;
use grove_core::state::State;

/// What a decoded event carries. One variant per [`EventKind`].
pub(super) enum ServiceUpdate {
    State(State),
    Reconciliation {
        reconciliation: Reconciliation,
        state: State,
    },
    Notification(Notification),
    ControlCompleted,
}

/// What to do with an event that has arrived.
pub(super) enum ServiceEventAction {
    Ignore,
    Recover(serde_json::Error),
    Apply {
        revision: u64,
        update: ServiceUpdate,
        gap: bool,
    },
}

/// Read one event's payload, refusing anything that is not exactly its
/// documented shape.
///
/// `deny_unknown_fields` on every payload: a frame carrying a field this
/// build does not know is a protocol disagreement, and the honest answer is to
/// re-poll git and tmux rather than apply half of it.
pub(super) fn decode_service_event(
    event: Event,
) -> Result<(u64, ServiceUpdate), serde_json::Error> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct StatePayload {
        state: State,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct ReconciliationPayload {
        reconciliation: Reconciliation,
        state: State,
    }

    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct NotificationPayload {
        notification: Notification,
    }

    let update = match event.kind {
        EventKind::StateChanged => {
            let payload = serde_json::from_value::<StatePayload>(event.payload)?;
            ServiceUpdate::State(payload.state)
        }
        EventKind::ReconciliationCompleted => {
            let payload = serde_json::from_value::<ReconciliationPayload>(event.payload)?;
            ServiceUpdate::Reconciliation {
                reconciliation: payload.reconciliation,
                state: payload.state,
            }
        }
        EventKind::NotificationReceived => {
            let payload = serde_json::from_value::<NotificationPayload>(event.payload)?;
            ServiceUpdate::Notification(payload.notification)
        }
        EventKind::ControlCompleted => ServiceUpdate::ControlCompleted,
    };
    Ok((event.revision, update))
}

/// Decide what to do with an event given the highest revision already applied.
pub(super) fn classify_service_event(last_revision: u64, event: Event) -> ServiceEventAction {
    if event.revision <= last_revision {
        return ServiceEventAction::Ignore;
    }
    let gap = event.revision > last_revision.saturating_add(1);
    match decode_service_event(event) {
        Ok((revision, update)) => ServiceEventAction::Apply {
            revision,
            update,
            gap,
        },
        Err(error) => ServiceEventAction::Recover(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use grove_core::status::SessionStatus;

    #[test]
    fn service_events_decode_only_their_documented_state_bearing_payloads() {
        let state = State::default();
        let reconciliation = Reconciliation::default();
        let notification = Notification::new("a1b2c3", SessionStatus::Attention);

        let (revision, update) = decode_service_event(Event::new(
            7,
            EventKind::StateChanged,
            serde_json::json!({"state": state}),
        ))
        .expect("state event");
        assert_eq!(revision, 7);
        assert!(matches!(update, ServiceUpdate::State(_)));

        let (_, update) = decode_service_event(Event::new(
            8,
            EventKind::ReconciliationCompleted,
            serde_json::json!({
                "reconciliation": reconciliation,
                "state": State::default(),
            }),
        ))
        .expect("reconciliation event");
        assert!(matches!(update, ServiceUpdate::Reconciliation { .. }));

        let (_, update) = decode_service_event(Event::new(
            9,
            EventKind::NotificationReceived,
            serde_json::json!({"notification": notification}),
        ))
        .expect("notification event");
        assert!(matches!(update, ServiceUpdate::Notification(_)));

        let (_, update) = decode_service_event(Event::new(
            10,
            EventKind::ControlCompleted,
            serde_json::json!({"operation": "anything"}),
        ))
        .expect("control completion");
        assert!(matches!(update, ServiceUpdate::ControlCompleted));

        for kind in [
            EventKind::StateChanged,
            EventKind::ReconciliationCompleted,
            EventKind::NotificationReceived,
        ] {
            assert!(
                decode_service_event(Event::new(11, kind, serde_json::Value::Null)).is_err(),
                "accepted a missing state-bearing payload for {kind:?}"
            );
            assert!(
                decode_service_event(Event::new(
                    11,
                    kind,
                    serde_json::json!({"unexpected": true}),
                ))
                .is_err(),
                "accepted an unknown payload shape for {kind:?}"
            );
        }
    }

    #[test]
    fn service_event_revisions_ignore_replays_and_recover_from_gaps_or_corruption() {
        let event = |revision, payload| Event::new(revision, EventKind::StateChanged, payload);

        assert!(matches!(
            classify_service_event(5, event(5, serde_json::json!({"state": State::default()}))),
            ServiceEventAction::Ignore
        ));
        assert!(matches!(
            classify_service_event(5, event(4, serde_json::json!({"state": State::default()}))),
            ServiceEventAction::Ignore
        ));
        assert!(matches!(
            classify_service_event(5, event(6, serde_json::json!({"state": State::default()}))),
            ServiceEventAction::Apply { gap: false, .. }
        ));
        assert!(matches!(
            classify_service_event(5, event(7, serde_json::json!({"state": State::default()}))),
            ServiceEventAction::Apply { gap: true, .. }
        ));
        assert!(matches!(
            classify_service_event(5, event(6, serde_json::Value::Null)),
            ServiceEventAction::Recover(_)
        ));
        assert!(matches!(
            classify_service_event(u64::MAX, event(u64::MAX, serde_json::Value::Null)),
            ServiceEventAction::Ignore
        ));
    }
}
