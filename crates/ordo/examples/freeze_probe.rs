//! Probe: take one freeze snapshot ([`ordo::platform::freeze::gather`]) in
//! this process and print it — the stderr summary line the daemon would
//! write, then the JSON the log would hold. For checking what the machine
//! reports on a good day, so a bad day's record has something to differ from.
//!
//!   cargo run --example freeze_probe
//!   cargo run --example freeze_probe -- taps     # the tap list alone

fn main() {
    let taps_only = std::env::args().any(|a| a == "taps");
    ordo::platform::drained(|| {
        if taps_only {
            let (taps, error) = ordo::platform::freeze::tap_list();
            if let Some(e) = error {
                eprintln!("{e}");
            }
            println!("{}", serde_json::to_string_pretty(&taps).unwrap());
            return;
        }
        let snap = ordo::platform::freeze::gather();
        eprintln!("{}", snap.summary());
        println!("{}", serde_json::to_string_pretty(&snap).unwrap());
    });
}
