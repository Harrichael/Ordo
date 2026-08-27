use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum Backend {
    Native,
    Emulated,
}

#[derive(Parser)]
#[command(name = "ordo", version, about = "macOS workspace navigator")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the daemon: intercept hotkeys and act on them (workspace switching,
    /// MRU focus, mouse-follows-focus, new-window placement). Pass --observe to
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
        /// Start disengaged: hotkeys pass through untouched until you press
        /// Ctrl+Alt+Cmd+O to engage. (Meaningless with --observe, which never
        /// engages at all.)
        #[arg(long, conflicts_with = "observe")]
        paused: bool,
        /// Workspace backend: Ordo-emulated (park-off-screen) workspaces —
        /// instant, animation-free — or native macOS Spaces (animated, and
        /// needs the Mission Control shortcut lever + pre-created Spaces).
        #[arg(long, value_enum, default_value_t = Backend::Emulated)]
        backend: Backend,
        /// Number of emulated workspaces (ignored for the native backend, which
        /// uses the Spaces you created in Mission Control).
        #[arg(long, default_value_t = 9)]
        workspaces: u8,
    },
    /// Disengage Ordo and gather displaced windows back onto the visible area.
    /// Signals a running daemon to go inert, then gathers in this process too,
    /// so it works even if the daemon is wedged or gone.
    Rescue {
        #[arg(long)]
        db: Option<PathBuf>,
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
            paused,
            backend,
            workspaces,
        } => run(db, interval, observe, paused, backend, workspaces),
        Command::Rescue { db } => rescue(db),
        Command::Replay { db, run, from } => replay(db, run, from),
    }
}

fn db_path(db: Option<PathBuf>) -> PathBuf {
    db.unwrap_or_else(ordo::logger::default_db_path)
}

/// The pidfile lives next to the log, so the rescue CLI can find a running
/// daemon given the same (default or explicit) db path.
fn pid_path(db: &std::path::Path) -> PathBuf {
    db.parent()
        .unwrap_or(std::path::Path::new("."))
        .join("ordo.pid")
}

#[cfg(target_os = "macos")]
fn run(
    db: Option<PathBuf>,
    interval: f64,
    observe: bool,
    paused: bool,
    backend: Backend,
    workspaces: u8,
) {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use ordo::clock::{Clock, SystemClock};
    use ordo::engine::{Engine, Msg};
    use ordo::logger::Logger;
    use ordo::platform::{
        emulated_backend, native_backend, observer, tap, MacEffector, MacWorldSource,
    };
    use ordo::ports::{Effector, NullEffector};
    use ordo_core::RescanTrigger;

    let path = db_path(db);
    if observe {
        println!("ordo: observe-only. Logging to {}", path.display());
    } else {
        if paused {
            println!("ordo: paused (disengaged). Logging to {}", path.display());
        } else {
            println!("ordo: active. Logging to {}", path.display());
        }
        println!("ordo: rescue (off) = Ctrl+Alt+Cmd+Escape pressed twice within 2s.");
        println!("ordo: engage (on)  = Ctrl+Alt+Cmd+O.");
    }
    println!("ordo: requires Accessibility permission (System Settings > Privacy & Security > Accessibility).");
    println!("ordo: press Ctrl-C to stop.");

    let stop = Arc::new(AtomicBool::new(false));
    install_sigint(stop.clone());
    let rescue_requested = Arc::new(AtomicBool::new(false));
    install_sigusr1(rescue_requested.clone());

    // Record our pid so `ordo rescue` can signal us; clean it up on exit.
    let pidfile = pid_path(&path);
    let _ = std::fs::write(&pidfile, std::process::id().to_string());

    // Intercepting starts on for an active run; the tap and effector share it.
    let intercepting = Arc::new(AtomicBool::new(!observe && !paused));
    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();

    if !observe {
        // The tap runs even when paused — it's what hears the engage chord.
        tap::spawn(tx.clone(), intercepting.clone());
        // New-window corralling depends on the WindowCreated hint this emits.
        observer::spawn(tx.clone());
    }
    if paused {
        // A paused start is a run born rescued: same inert mode, entered via a
        // logged event so a replay reproduces the pause exactly.
        let _ = tx.send(Msg::Rescue);
    }

    // The engine and all macOS handles live entirely on this one thread.
    let engine_intercepting = intercepting.clone();
    let engine_thread = std::thread::spawn(move || {
        let clock = SystemClock::new();
        let backend_label = match (backend, observe) {
            (Backend::Native, false) => "native",
            (Backend::Native, true) => "native(observe)",
            (Backend::Emulated, false) => "emulated",
            (Backend::Emulated, true) => "emulated(observe)",
        };
        let logger = match Logger::open(&path, ordo::VERSION, backend_label, clock.now().wall_ms) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("ordo: cannot open log db: {e}");
                return;
            }
        };
        let backend = match backend {
            Backend::Native => native_backend(),
            Backend::Emulated => emulated_backend(workspaces),
        };
        let world = MacWorldSource::new(backend.clone());
        let effector: Box<dyn Effector> = if observe {
            Box::new(NullEffector)
        } else {
            Box::new(MacEffector::new(backend, engine_intercepting))
        };
        let engine = Engine::new(logger, Box::new(world), effector, Box::new(clock));
        engine.run(rx);
    });

    if !observe {
        // Near-instant reaction to external Space switches (Ordo's own
        // switches already rescan via PostEffect).
        observer::spawn_space_watcher(tx.clone());
    }

    // Periodic rescans drive observation. Dropping this tx on stop closes the
    // engine's channel, which ends its loop and writes the run's end time.
    let period = Duration::from_secs_f64(interval.max(0.1));
    while !stop.load(Ordering::Relaxed) {
        // A SIGUSR1 (from `ordo rescue`) disengages interception immediately —
        // the same effect as the hotkey fast path — and tells the core to go
        // inert. The gather itself runs in the rescue CLI's own process.
        if rescue_requested.swap(false, Ordering::Relaxed) {
            intercepting.store(false, Ordering::Relaxed);
            let _ = tx.send(Msg::Rescue);
        }
        if tx.send(Msg::Rescan(RescanTrigger::Periodic)).is_err() {
            break;
        }
        std::thread::sleep(period);
    }
    // An explicit shutdown, because the tap thread holds a sender clone that
    // would otherwise keep the engine's channel open forever.
    let _ = tx.send(Msg::Shutdown);
    let _ = engine_thread.join();
    let _ = std::fs::remove_file(&pidfile);
    println!("ordo: stopped.");
}

#[cfg(not(target_os = "macos"))]
fn run(
    _db: Option<PathBuf>,
    _interval: f64,
    _observe: bool,
    _paused: bool,
    _backend: Backend,
    _workspaces: u8,
) {
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

#[cfg(target_os = "macos")]
fn install_sigusr1(flag: std::sync::Arc<std::sync::atomic::AtomicBool>) {
    use std::sync::atomic::Ordering;
    use std::sync::OnceLock;
    static FLAG: OnceLock<std::sync::Arc<std::sync::atomic::AtomicBool>> = OnceLock::new();
    let _ = FLAG.set(flag);
    extern "C" fn handler(_sig: libc::c_int) {
        if let Some(f) = FLAG.get() {
            f.store(true, Ordering::Relaxed);
        }
    }
    let ptr = handler as extern "C" fn(libc::c_int);
    unsafe {
        libc::signal(libc::SIGUSR1, ptr as libc::sighandler_t);
    }
}

#[cfg(target_os = "macos")]
fn rescue(db: Option<PathBuf>) {
    let path = db_path(db);
    // Signal a running daemon to go inert (best-effort; it may not be running).
    let pidfile = pid_path(&path);
    if let Ok(text) = std::fs::read_to_string(&pidfile) {
        if let Ok(pid) = text.trim().parse::<i32>() {
            let sent = unsafe { libc::kill(pid, libc::SIGUSR1) } == 0;
            if sent {
                println!("ordo rescue: signaled daemon (pid {pid}) to disengage.");
                // Give it a moment to flip interception off before we move windows.
                std::thread::sleep(std::time::Duration::from_millis(300));
            }
        }
    }
    // Gather regardless — this must succeed against a hung or dead daemon.
    let now = ordo::clock::SystemClock::new();
    let wall = {
        use ordo::clock::Clock;
        now.now().wall_ms
    };
    let count = ordo::platform::rescue_gather::gather(&path, wall);
    println!("ordo rescue: gathered {count} window(s) onto the visible area.");
}

#[cfg(not(target_os = "macos"))]
fn rescue(_db: Option<PathBuf>) {
    eprintln!("ordo rescue is only supported on macOS.");
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
