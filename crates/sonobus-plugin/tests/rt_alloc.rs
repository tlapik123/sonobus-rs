//! The plugin's `process` never allocates or frees on the host's audio thread: across a peer
//! joining through a local server and streaming (aux peer outputs on), host transport changes
//! every block, parameter changes (forwarded by the parameter thread), bypass toggling, and
//! host callbacks that are not whole engine blocks.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use common::*;
use nice_plug::params::InternalParamMut;

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    let _ = ARMED.try_with(|a| {
        if a.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
}

struct Counting;

// SAFETY: forwards every call unchanged to the system allocator; the bookkeeping only touches
// const-initialised thread-locals, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn armed(f: &mut dyn FnMut()) {
    ARMED.with(|a| a.set(true));
    f();
    ARMED.with(|a| a.set(false));
}

/// One armed callback of `n` frames on `a` (the sine and the bookkeeping stay unarmed).
fn step(a: &mut Host, n: usize) {
    a.fill(n);
    a.process(n, &mut armed);
    a.advance(n);
}

#[test]
fn process_never_allocates() {
    let _s = serial();
    let srv = aoo::server::run("127.0.0.1:0".parse().unwrap()).expect("server");
    let (mut a, mut b) = (Host::new(440.0), Host::new(660.0));
    assert!(a.activate(SR, BLOCK as u32, 0));
    assert!(b.activate(SR, BLOCK as u32, 0));
    // warm up: the first process call of each kind (thread-locals, lazy statics in the clock)
    step(&mut a, BLOCK);
    COUNT.with(|c| c.set(0));

    a.join(srv.local_addr(), "alice", "rt");
    b.join(srv.local_addr(), "bob", "rt");
    let pace = std::time::Duration::from_secs_f64(BLOCK as f64 / f64::from(SR));
    let mut connected = false;
    let p = a.plugin.sb_params().clone();
    for i in 0..4000 {
        // the host transport moves and jumps
        let t = &mut a.ctx.transport;
        t.playing = i % 300 < 200;
        t.tempo = Some(if i % 500 < 250 { 120.0 } else { 93.5 });
        if i % 700 == 0 {
            t.pos_beats = Some(f64::from(i) * 0.1);
        }
        // parameter changes reach the engine through the parameter thread
        if i % 400 == 0 {
            let mut c = p.to_core();
            c.met_enabled = !c.met_enabled;
            c.dry = if c.dry > 0.5 { 0.3 } else { 1.0 };
            c.main_reverb_enabled = i % 800 == 0;
            p.set_from_core(&c);
        }
        if i % 900 == 450 {
            // SAFETY: atomic store, as the wrappers do for host automation
            unsafe { p.bypass._internal_set_plain_value(!p.bypass.value()) };
        }
        // odd host callback sizes in the last quarter (the engine's FIFO path)
        let n = if i > 3000 {
            [BLOCK, 100, 37, 1, 300][i as usize % 5]
        } else {
            BLOCK
        };
        step(&mut a, n);
        b.block(BLOCK);
        connected |= a.receiving() == 1 && b.receiving() == 1;
        std::thread::sleep(pace);
    }
    assert!(
        connected,
        "the peers never connected: the test did not cover streaming"
    );
    let count = COUNT.with(Cell::get);
    assert_eq!(count, 0, "allocations on the audio thread");
}
