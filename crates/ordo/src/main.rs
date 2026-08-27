use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ordo", version, about = "macOS workspace navigator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: intercept hotkeys and act on them (focus + mouse warp
    /// now; workspace switching lands in a later milestone). Pass --observe to
    /// decide-and-log without touching anything.
    Run {
        /// Log database path (defaults to ~/Library/Application Support/Ordo/log.db).
        #[arg(long)]
        db: Option<PathBuf>,
        /// Seconds between full rescans (the reconcile safety net).
        #[arg(long, default_value_t = 2.0)]
        interval: f64,
        /// Observe and log only; execute nothing and intercept no keys.
        #[arg(long)]
        observe: bool,
    },
    /// Replay a logged run through the core and report any effect-stream
    /// divergence — the tool for "what happened, and does the current core
    /// still agree".
    Replay {
        #[arg(long)]
        db: Option<PathBuf>,
        /// Run id to replay (defaults to the most recent).
        #[arg(long)]
        run: Option<i64>,
        /// Resume from the nearest checkpoint at or before this event seq.
        #[arg(long)]
        from: Option<u64>,
    },
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Command::Run {
            db,
            interval,
            observe,
        } => run(db, interval, observe),
        Command::Replay { db, run, from } => replay(db, run, from),
    }
}

fn db_path(db: Option<PathBuf>) -> PathBuf {
    db.unwrap_or_else(ordo::logger::default_db_path)
}

#[cfg(target_os = "macos")]
fn run(db: Option<PathBuf>, interval: f64, observe: bool) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use ordo::clock::{Clock, SystemClock};
    use ordo::engine::{Engine, Msg};
    use ordo::logger::Logger;
    use ordo::platform::{native_backend, tap, MacEffector, MacWorldSource};
    use ordo::ports::{Effector, NullEffector};
    use ordo_core::RescanTrigger;

    let path = db_path(db);
    if observe {
        println!("ordo: observe-only. Logging to {}", path.display());
    } else {
        println!("ordo: active. Logging to {}", path.display());
        println!("ordo: rescue = Ctrl+Alt+Cmd+Escape pressed twice within 2s.");
    }
    println!("ordo: requires Accessibility permission (System Settings > Privacy & Security > Accessibility).");
    println!("ordo: press Ctrl-C to stop.");

    let stop = Arc::new(AtomicBool::new(false));
    install_sigint(stop.clone());

    // Intercepting starts on for an active run; the tap and effector share it.
    let intercepting = Arc::new(AtomicBool::new(!observe));
    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();

    if !observe {
        tap::spawn(tx.clone(), intercepting.clone());
    }

    // The engine and all macOS handles live entirely on this one thread.
    let engine_intercepting = intercepting.clone();
    let engine_thread = std::thread::spawn(move || {
        let clock = SystemClock::new();
        let backend_label = if observe { "native(observe)" } else { "native" };
        let logger = match Logger::open(&path, ordo::VERSION, backend_label, clock.now().wall_ms) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ordo: cannot open log db: {e}");
                return;
            }
        };
        let backend = native_backend();
        let world = MacWorldSource::new(backend.clone());
        let effector: Box<dyn Effector> = if observe {
            Box::new(NullEffector)
        } else {
            Box::new(MacEffector::new(backend, engine_intercepting))
        };
        let engine = Engine::new(logger, Box::new(world), effector, Box::new(clock));
        engine.run(rx);
    });

    // Periodic rescans drive observation. Dropping this tx on stop closes the
    // engine's channel, which ends its loop and writes the run's end time.
    let period = Duration::from_secs_f64(interval.max(0.1));
    while !stop.load(Ordering::Relaxed) {
        if tx.send(Msg::Rescan(RescanTrigger::Periodic)).is_err() {
            break;
        }
        std::thread::sleep(period);
    }
    // An explicit shutdown, because the tap thread holds a sender clone that
    // would otherwise keep the engine's channel open forever.
    let _ = tx.send(Msg::Shutdown);
    let _ = engine_thread.join();
    println!("ordo: stopped.");
}

#[cfg(not(target_os = "macos"))]
fn run(_db: Option<PathBuf>, _interval: f64) {
    eprintln!("ordo run is only supported on macOS.");
}

#[cfg(target_os = "macos")]
fn install_sigint(stop: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    use std::sync::OnceLock;
    static FLAG: OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> = OnceLock::new();
    let _ = FLAG.set(stop);
    extern "C" fn handler(_sig: libc::c_int) {
        if let Some(f) = FLAG.get() {
            f.store(true, Ordering::Relaxed);
        }
    }
    let ptr = handler as extern "C" fn(libc::c_int);
    unsafe {
        libc::signal(libc::SIGINT, ptr as libc::sighandler_t);
    }
}

fn replay(db: Option<PathBuf>, run: Option<i64>, from: Option<u64>) {
    let path = db_path(db);
    let conn = match rusqlite::Connection::open(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("ordo: cannot open log db {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let run_id = match run.or_else(|| ordo::rescue::latest_run(&conn).ok().flatten()) {
        Some(r) => r,
        None => {
            eprintln!("ordo: no runs in {}", path.display());
            std::process::exit(1);
        }
    };
    match ordo::replay::replay(&conn, run_id, from) {
        Ok(report) => {
            println!(
                "ordo: replayed run {} ({} events from seq {})",
                report.run_id, report.events_replayed, report.from_seq
            );
            if report.is_clean() {
                println!("ordo: clean — the core reproduced every logged effect.");
            } else {
                println!("ordo: {} divergence(s):", report.mismatches.len());
                for m in &report.mismatches {
                    println!(
                        "  seq {}: logged {} effect(s), recomputed {}",
                        m.seq,
                        m.logged.len(),
                        m.recomputed.len()
                    );
                }
                std::process::exit(2);
            }
        }
        Err(e) => {
            eprintln!("ordo: replay failed: {e}");
            std::process::exit(1);
        }
    }
}
