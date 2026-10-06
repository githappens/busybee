//! `Jobserver::acquire` against a hot jobserver build: a simulated make whose
//! workers return a token and block in `read` for the next one straight away
//! (`docs/design/bzbd.md` §Admission policy, the static drain).

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use bzb_core::jobserver::Jobserver;

const POOL: u32 = 6;
const WORKERS: u64 = 4;
const GRANT: u32 = 3;
/// The default `drain_deadline_ms`.
const DEADLINE: Duration = Duration::from_millis(2000);
const ROUNDS: usize = 20;

/// One make job slot: its own blocking read and write handles on the fifo, as
/// a fifo-jobserver client opens them. Takes a token, holds it for `hold`,
/// writes it back and blocks in `read` again at once.
fn worker(path: &Path, hold: Duration, stop: Arc<AtomicBool>) -> JoinHandle<()> {
    // Opened here, not in the thread: the daemon's read-write handle is
    // already open, so neither blocking open waits for a peer.
    let mut rd = File::open(path).expect("open the fifo for reading");
    let mut wr = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open the fifo for writing");
    thread::spawn(move || {
        let mut token = [0u8; 1];
        loop {
            rd.read_exact(&mut token).expect("read a token");
            if stop.load(Ordering::Acquire) {
                return;
            }
            thread::sleep(hold);
            wr.write_all(&token).expect("return the token");
        }
    })
}

#[test]
fn static_drain_collects_its_grant_against_a_hot_reader() {
    let dir = tempfile::tempdir().expect("temp dir");
    let js = Jobserver::create(dir.path(), POOL).expect("create the jobserver");
    let stop = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (0..WORKERS)
        .map(|i| {
            let hold = Duration::from_millis(50 + i * 50 / (WORKERS - 1));
            worker(js.path(), hold, Arc::clone(&stop))
        })
        .collect();
    // Let every worker take its first token and settle into the cycle.
    thread::sleep(Duration::from_millis(200));

    let mut short = Vec::new();
    for round in 0..ROUNDS {
        let start = Instant::now();
        let before = js.free().unwrap();
        let got = js.acquire(GRANT, DEADLINE).expect("acquire");
        eprintln!(
            "round {round}: free before {before}, got {got} in {:?}",
            start.elapsed()
        );
        if got < GRANT {
            short.push(format!(
                "round {round}: {got}/{GRANT} after {:?}",
                start.elapsed()
            ));
        }
        js.release(got).expect("release");
    }

    // Every worker is either blocked in `read` or about to be: one token each
    // wakes them to see the flag.
    stop.store(true, Ordering::Release);
    js.release(WORKERS as u32).expect("wake the workers");
    for w in workers {
        w.join().expect("worker thread");
    }
    drop(js);
    let _ = fs::remove_dir_all(dir.path());

    assert!(
        short.is_empty(),
        "{} of {ROUNDS} drains came up short within {DEADLINE:?}:\n{}",
        short.len(),
        short.join("\n")
    );
}
