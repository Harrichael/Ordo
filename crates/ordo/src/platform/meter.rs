//! What the slow OS reads cost, counted by the reads themselves.
//!
//! Most of them are made deep inside the workspace backend, through its
//! `Desktop` port, where no snapshot phase can see them apart. The tally is
//! per thread: the snapshot runs on the engine thread and reads only that
//! thread's; reads on other threads add to tallies nobody takes.

use std::cell::Cell;
use std::time::Instant;

use crate::ports::{OsReads, Tally};

thread_local! {
    static READS: Cell<OsReads> = Cell::new(OsReads::default());
}

pub(crate) fn metered<T>(tally: fn(&mut OsReads) -> &mut Tally, read: impl FnOnce() -> T) -> T {
    let started = Instant::now();
    let out = read();
    let took = started.elapsed();
    READS.with(|r| {
        let mut reads = r.get();
        let t = tally(&mut reads);
        t.calls += 1;
        t.time += took;
        r.set(reads);
    });
    out
}

/// This thread's reads since the last take.
pub fn take() -> OsReads {
    READS.with(Cell::take)
}
