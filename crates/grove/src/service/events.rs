//! Event publication and the subscription stream.
//!
//! One place for the fan-out half of the API, kept away from the request
//! handlers because it is the only part that outlives a request: a subscriber
//! holds its connection open and is written to by whichever thread published,
//! so its queue bound and its drop rules are what stop a slow reader becoming
//! everybody's problem.

use std::collections::{HashMap, HashSet};
use std::os::unix::net::UnixStream;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};

use grove_core::protocol::{self, Event, EventKind, Request, Response};
use serde_json::json;

use super::{ApiContext, SUBSCRIBER_QUEUE, params::SubscribeParams, params::UnsubscribeParams};

#[derive(Default)]
pub(super) struct EventHub {
    revision: AtomicU64,
    next_subscriber: AtomicU64,
    subscribers: Mutex<HashMap<String, Subscriber>>,
}

pub(super) struct Subscriber {
    topics: HashSet<EventKind>,
    sender: SyncSender<Event>,
    replaces_legacy_gui_delivery: bool,
}

#[derive(Default)]
pub(super) struct PublishOutcome {
    pub(super) delivered: bool,
    pub(super) delivered_to_gui: bool,
}

impl EventHub {
    pub(super) fn subscribe(
        &self,
        topics: HashSet<EventKind>,
        replaces_legacy_gui_delivery: bool,
    ) -> (String, Receiver<Event>, u64) {
        let id = format!(
            "sub-{}",
            self.next_subscriber.fetch_add(1, Ordering::Relaxed) + 1
        );
        let (sender, receiver) = sync_channel(SUBSCRIBER_QUEUE);
        let mut subscribers = lock_subscribers(self);
        let revision = self.revision.load(Ordering::Relaxed);
        subscribers.insert(
            id.clone(),
            Subscriber {
                topics,
                sender,
                replaces_legacy_gui_delivery,
            },
        );
        (id, receiver, revision)
    }

    pub(super) fn unsubscribe(&self, id: &str) -> bool {
        lock_subscribers(self).remove(id).is_some()
    }

    pub(super) fn publish(&self, kind: EventKind, payload: serde_json::Value) -> PublishOutcome {
        let mut subscribers = lock_subscribers(self);
        // Registration and revision assignment share this lock, giving the
        // acknowledgement baseline and subsequent events one total order.
        let revision = self.revision.fetch_add(1, Ordering::Relaxed) + 1;
        let event = Event::new(revision, kind, payload);
        let mut outcome = PublishOutcome::default();
        subscribers.retain(|_, subscriber| {
            if !subscriber.topics.contains(&kind) {
                return true;
            }
            match subscriber.sender.try_send(event.clone()) {
                Ok(()) => {
                    outcome.delivered = true;
                    outcome.delivered_to_gui |= subscriber.replaces_legacy_gui_delivery;
                    true
                }
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
            }
        });
        outcome
    }
}

pub(super) fn lock_subscribers(
    events: &EventHub,
) -> std::sync::MutexGuard<'_, HashMap<String, Subscriber>> {
    events
        .subscribers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
pub(super) fn serve_subscription(mut stream: UnixStream, request: &Request, api: &ApiContext) {
    if request.protocol != protocol::VERSION {
        let response = Response::error(
            &request.id,
            "unsupported_protocol",
            format!(
                "protocol {} is unsupported; this service speaks {}",
                request.protocol,
                protocol::VERSION
            ),
        );
        let _ = protocol::write_response(&mut stream, &response);
        return;
    }
    let SubscribeParams { topics, client } = match serde_json::from_value(request.params.clone()) {
        Ok(params) => params,
        Err(error) => {
            let response = Response::error(&request.id, "invalid_params", error.to_string());
            let _ = protocol::write_response(&mut stream, &response);
            return;
        }
    };
    if topics.is_empty() {
        let response = Response::error(
            &request.id,
            "invalid_params",
            "at least one event topic is required",
        );
        let _ = protocol::write_response(&mut stream, &response);
        return;
    }
    if client.as_deref().is_some_and(|client| client != "gui") {
        let response = Response::error(
            &request.id,
            "invalid_params",
            "subscription client must be `gui` when present",
        );
        let _ = protocol::write_response(&mut stream, &response);
        return;
    }
    let (subscription_id, events, revision) = api
        .events
        .subscribe(topics, client.as_deref() == Some("gui"));
    let response = Response::success(
        &request.id,
        json!({
            "subscription_id": subscription_id,
            "revision": revision,
        }),
    );
    if protocol::write_response(&mut stream, &response).is_err() {
        api.events.unsubscribe(&subscription_id);
        return;
    }
    while let Ok(event) = events.recv() {
        if protocol::write_json(&mut stream, &event).is_err() {
            break;
        }
    }
    api.events.unsubscribe(&subscription_id);
}

pub(super) fn unsubscribe(request: &Request, api: &ApiContext) -> Response {
    let UnsubscribeParams { subscription_id } = match serde_json::from_value(request.params.clone())
    {
        Ok(params) => params,
        Err(error) => {
            return Response::error(&request.id, "invalid_params", error.to_string());
        }
    };
    Response::success(
        &request.id,
        json!({"unsubscribed": api.events.unsubscribe(&subscription_id)}),
    )
}
