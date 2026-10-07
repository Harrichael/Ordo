//! Holding looks at the screen back while the apps are busy with Ordo's own
//! writes (see [`LookGate`]). A thread of its own, so the engine is free the
//! whole time: it hands a look over and gets on with the next press, and the
//! look comes back as an ordinary rescan once the apps' queues run dry.

use std::time::Instant;

use crossbeam_channel::{Receiver, Sender};
use ordo_core::RescanTrigger;

use crate::app_queue::AppQueues;
use crate::engine::{fold_triggers, Msg, LOOK_BOUND};
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
        let (kept, folded) = fold_triggers(held);
        if !folded.is_empty() && engine.send(Msg::Folded(folded)).is_err() {
            return;
        }
        for trigger in kept {
            if engine.send(Msg::Rescan(trigger)).is_err() {
                return;
            }
        }
    }
}
