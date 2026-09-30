//! Holding looks at the screen back while the apps are busy with Ordo's own
//! writes (see [`LookGate`]). A thread of its own, so the engine is free the
//! whole time: it hands a look over and gets on with the next press, and the
//! look comes back as an ordinary rescan once the apps' queues run dry.

use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use ordo_core::{AxHintKind, RescanTrigger};

use crate::app_queue::AppQueues;
use crate::engine::{Msg, LOOK_BOUND};
use crate::ports::LookGate;

pub struct QueueGate {
    queues: AppQueues,
    held: Sender<(RescanTrigger, Instant)>,
}

impl LookGate for QueueGate {
    fn idle(&self) -> bool {
        self.queues.idle()
    }

    fn defer(&self, trigger: RescanTrigger, until: Instant) {
        let _ = self.held.send((trigger, until));
    }
}

pub fn spawn(queues: AppQueues, engine: Sender<Msg>) -> QueueGate {
    let (held, rx) = crossbeam_channel::unbounded();
    let waiting = queues.clone();
    std::thread::spawn(move || hold(waiting, rx, engine));
    QueueGate { queues, held }
}

fn hold(queues: AppQueues, rx: Receiver<(RescanTrigger, Instant)>, engine: Sender<Msg>) {
    while let Ok((first, until)) = rx.recv() {
        // The engine's deadline, not a fresh one: a look handed back again
        // keeps the time it was first held from, so the bound is one bound.
        queues.wait_idle(until.min(Instant::now() + LOOK_BOUND));
        let mut held = vec![first];
        held.extend(rx.try_iter().map(|(t, _)| t));
        for trigger in fold(held) {
            if engine.send(Msg::Rescan(trigger)).is_err() {
                return;
            }
        }
    }
}

/// Everything held becomes one look, except that each app's creation hint
/// survives on its own: only those authorize corralling a new window, and
/// the engine folds the rest the same way (`collapse_rescans`).
fn fold(held: Vec<RescanTrigger>) -> Vec<RescanTrigger> {
    let mut births: Vec<RescanTrigger> = Vec::new();
    let mut last = None;
    for t in held {
        match t {
            RescanTrigger::AxHint {
                kind: AxHintKind::WindowCreated,
                ..
            } => {
                if !births.contains(&t) {
                    births.push(t);
                }
            }
            other => last = Some(other),
        }
    }
    if births.is_empty() {
        last.into_iter().collect()
    } else {
        births
    }
}
