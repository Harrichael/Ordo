//! Probe: can the window server's list stand in for the AX walk as the
//! source of a switch's frames? A switch saves the outgoing windows' frames
//! as their positions, so the two must agree exactly, and it needs the
//! frames and owners of hidden apps' windows too, which the on-screen list
//! drops. Read-only: every window AX reports is compared against the
//! all-windows list, which includes windows that are not on screen.
//!
//!   cargo run --release --example frames_probe            # 10 rounds
//!   cargo run --release --example frames_probe -- 50      # 50 rounds

use std::collections::HashMap;
use std::time::{Duration, Instant};

use ordo::platform::{ax, cf};
use ordo_core::Rect;
use ordo_skylight_sys as sys;

const OPTION_ALL: u32 = 0;

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGWindowListCopyWindowInfo(option: u32, relative_to: u32) -> sys::CFArrayRef;
    fn CGWindowListCreateDescriptionFromArray(windows: sys::CFArrayRef) -> sys::CFArrayRef;
}

/// Only the named windows, on screen or not. The array holds raw window ids,
/// not CF numbers, hence no callbacks.
fn described(ids: &[u32]) -> HashMap<u32, Listed> {
    let mut out = HashMap::new();
    let raw: Vec<*const std::ffi::c_void> = ids.iter().map(|w| *w as usize as *const _).collect();
    unsafe {
        let arr = sys::CFArrayCreate(std::ptr::null(), raw.as_ptr(), raw.len() as isize, std::ptr::null());
        if arr.is_null() {
            return out;
        }
        let desc = CGWindowListCreateDescriptionFromArray(arr);
        sys::CFRelease(arr);
        if desc.is_null() {
            return out;
        }
        out = parse(desc);
        sys::CFRelease(desc);
    }
    out
}

struct Listed {
    pid: i32,
    frame: Rect,
    on_screen: bool,
    layer: i64,
}

fn all_windows() -> HashMap<u32, Listed> {
    unsafe {
        let arr = CGWindowListCopyWindowInfo(OPTION_ALL, 0);
        if arr.is_null() {
            return HashMap::new();
        }
        let out = parse(arr);
        sys::CFRelease(arr);
        out
    }
}

unsafe fn parse(arr: sys::CFArrayRef) -> HashMap<u32, Listed> {
    let mut out = HashMap::new();
    {
        for i in 0..cf::array_len(arr) {
            let d = cf::array_get(arr, i) as sys::CFDictionaryRef;
            let b = cf::dict_get(d, "kCGWindowBounds");
            let entry = (|| {
                let id = cf::number_i64(cf::dict_get(d, "kCGWindowNumber"))? as u32;
                Some((
                    id,
                    Listed {
                        pid: cf::number_i64(cf::dict_get(d, "kCGWindowOwnerPID"))? as i32,
                        frame: Rect {
                            x: cf::number_f64(cf::dict_get(b, "X"))?,
                            y: cf::number_f64(cf::dict_get(b, "Y"))?,
                            w: cf::number_f64(cf::dict_get(b, "Width"))?,
                            h: cf::number_f64(cf::dict_get(b, "Height"))?,
                        },
                        on_screen: {
                            let b = cf::dict_get(d, "kCGWindowIsOnscreen");
                            !b.is_null() && b == sys::kCFBooleanTrue
                        },
                        layer: cf::number_i64(cf::dict_get(d, "kCGWindowLayer")).unwrap_or(-1),
                    },
                ))
            })();
            out.extend(entry);
        }
    }
    out
}

#[derive(Default)]
struct Tally {
    windows: usize,
    missing: usize,
    wrong_pid: usize,
    exact: usize,
    within_1pt: usize,
    off: usize,
}

fn main() {
    let rounds: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(10);
    let mut by_kind: HashMap<&'static str, Tally> = HashMap::new();
    let (mut ax_ms, mut cg_ms, mut some_ms) = (Vec::new(), Vec::new(), Vec::new());
    let mut some_missing = 0;
    let mut hidden_cache: HashMap<i32, bool> = HashMap::new();
    let mut shown = 0;
    for round in 0..rounds {
        let t = Instant::now();
        let windows = ax::windows();
        ax_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let t = Instant::now();
        let listed = all_windows();
        cg_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        let ids: Vec<u32> = windows.iter().map(|w| w.id.0).collect();
        let t = Instant::now();
        let some = described(&ids);
        some_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        for w in &windows {
            match (some.get(&w.id.0), listed.get(&w.id.0)) {
                (Some(a), Some(b)) if a.pid == b.pid && a.frame == b.frame => {}
                _ => some_missing += 1,
            }
        }
        if round == 0 {
            println!("AX: {} windows; list: {} entries", windows.len(), listed.len());
        }
        for w in &windows {
            let hidden = *hidden_cache
                .entry(w.app.0)
                .or_insert_with(|| ax::app_hidden(w.app) == Some(true));
            let l = listed.get(&w.id.0);
            let kind = match (hidden, l.map(|l| l.on_screen)) {
                (true, _) => "hidden app",
                (false, Some(true)) => "on screen",
                (false, _) => "off screen, app showing",
            };
            let t = by_kind.entry(kind).or_default();
            t.windows += 1;
            let Some(l) = l else {
                t.missing += 1;
                continue;
            };
            if l.pid != w.app.0 {
                t.wrong_pid += 1;
            }
            let a = w.frame;
            let b = l.frame;
            let d = [a.x - b.x, a.y - b.y, a.w - b.w, a.h - b.h]
                .iter()
                .fold(0.0f64, |m, v| m.max(v.abs()));
            if d == 0.0 {
                t.exact += 1;
            } else if d <= 1.0 {
                t.within_1pt += 1;
            } else {
                t.off += 1;
                if shown < 20 {
                    shown += 1;
                    println!(
                        "  off: {} pid {} {:?} layer {} [{kind}] ax {:?} list {:?}",
                        w.id.0, w.app.0, w.bundle_id, l.layer, a, b
                    );
                }
            }
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let med = |v: &mut Vec<f64>| {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    };
    println!(
        "reads, median ms: AX walk {:.2}, all-windows list {:.2}, named windows {:.2} ({} disagree with the full list)",
        med(&mut ax_ms),
        med(&mut cg_ms),
        med(&mut some_ms),
        some_missing
    );
    for (kind, t) in &by_kind {
        println!(
            "{kind}: {} window-reads; missing {}, wrong pid {}, exact {}, within 1pt {}, off {}",
            t.windows, t.missing, t.wrong_pid, t.exact, t.within_1pt, t.off
        );
    }
}
