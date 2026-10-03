//! Tests for `Jobserver::acquire` under hot-reader contention: a simulated
//! jobserver build competes for the fifo with the blocking drain in `acquire`.

use std::fs::{self, File};
use std::io::Read;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::Duration;

use bzb_core::jobserver::Jobserver;

/// A six-token pool under four simulated make slots: `acquire(3, 2000 ms)` must
/// collect all three on every call.
///
/// Each slot is modelled with two threads, matching make's own design:
/// - A **reader** thread: always blocked in `read(2)`, just as make's scheduler
///   is perpetually waiting for the next free job token.
/// - A **writer** thread: sleeps for 50–100 ms (the job) and then writes the
///   token back, just as a make child process does when it exits.
///
/// With this design the reader is in the kernel's `rd_wait` queue before any
/// token arrives, so the daemon — after the fix — competes on equal terms: both
/// are exclusive waiters, and the kernel picks one.
///
/// The pre-fix code runs `poll(2)` then reads with `O_NONBLOCK`.  On systems
/// where exclusive `rd_wait` waiters are woken before non-exclusive poll
/// waiters, the exclusive reader always takes the byte and the daemon is left
/// with `EAGAIN`, forcing a full restart of the poll cycle.  That restart can
/// repeat for the entire 2000 ms deadline, short-draining the grant.  The
/// empirical failure rate was 6/25 acquire attempts at 2 s in the e2e test
/// (`drain_deadline_ms`); see `e2e_pool.rs` for details.
#[test]
fn static_drain_collects_its_grant_against_a_hot_reader() {
    let dir = std::env::temp_dir().join(format!("bzb-contention-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();

    let js = Arc::new(Jobserver::create(&dir, 6).unwrap());

    let stop = Arc::new(AtomicBool::new(false));
    let mut handles = vec![];

    for slot in 0..4u64 {
        // Channel: reader → writer, "job started, you hold the token"
        let (tx, rx) = std::sync::mpsc::channel::<()>();

        // Reader thread: perpetually blocking in read(2), exactly as make's
        // job-scheduling loop does.  As soon as it acquires a token it hands
        // off responsibility for returning it to the writer and immediately
        // loops back into read(2).
        let path: PathBuf = js.path().to_path_buf();
        let js_r = Arc::clone(&js);
        let stop_r = Arc::clone(&stop);
        handles.push(std::thread::spawn(move || {
            let mut rd = File::open(&path).expect("open fifo for blocking read");
            let mut buf = [0u8; 1];
            loop {
                if rd.read_exact(&mut buf).is_err() {
                    break; // fifo closed (test cleanup)
                }
                if stop_r.load(Ordering::Acquire) {
                    // Return the token we just took before stopping.
                    let _ = js_r.release(1);
                    break;
                }
                // Delegate holding + returning to the writer thread.
                if tx.send(()).is_err() {
                    // Writer stopped.  Return this token ourselves and exit.
                    let _ = js_r.release(1);
                    break;
                }
                // Back to read(2) immediately — the reader is always blocking.
            }
        }));

        // Writer thread: receives the signal, simulates a 50–100 ms job, then
        // writes the token back (as a make child process does when it exits).
        let js_w = Arc::clone(&js);
        let stop_w = Arc::clone(&stop);
        let hold = Duration::from_millis(50 + slot * 10);
        handles.push(std::thread::spawn(move || {
            while rx.recv().is_ok() {
                std::thread::sleep(hold);
                if stop_w.load(Ordering::Acquire) {
                    // Return the token we're holding before the pool is torn down.
                    let _ = js_w.release(1);
                    break;
                }
                let _ = js_w.release(1);
            }
        }));
    }

    // Let the slots settle: readers block in read(2), writers start their first
    // hold cycle.
    std::thread::sleep(Duration::from_millis(300));

    // 20 rounds of acquire(3) at the default drain_deadline_ms.  On the
    // pre-fix code at least one round comes up short on systems where the
    // exclusive rd_wait waiters reliably beat non-exclusive poll; with the fix
    // the daemon's blocking read places it in the same queue, making the
    // competition fair.
    let mut short = 0u32;
    for _ in 0..20 {
        let got = js.acquire(3, Duration::from_millis(2000)).expect("acquire");
        if got < 3 {
            short += 1;
        }
        js.release(got).expect("release");
        // Brief gap so readers can cycle and re-enter read(2).
        std::thread::sleep(Duration::from_millis(50));
    }

    // Signal threads to stop and unblock any reader waiting in read(2).
    stop.store(true, Ordering::Release);
    let _ = js.release(8); // enough to unblock all four readers
    for h in handles {
        let _ = h.join();
    }
    drop(js);
    let _ = fs::remove_dir_all(&dir);

    assert_eq!(short, 0, "acquire(3) came up short {short}/20 times");
}
