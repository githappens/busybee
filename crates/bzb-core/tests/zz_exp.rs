use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use bzb_core::jobserver::Jobserver;
use bzb_test_support::counter;

#[test]
#[ignore]
fn zz_exp() {
    unsafe {
        let mut prio = libc::getpriority(libc::PRIO_PROCESS, 0);
        eprintln!("RESULT main qos={} nice={}", qos(), prio);
        prio = libc::getpriority(libc::PRIO_DARWIN_PROCESS, 0);
        eprintln!("RESULT darwin_bg={}", prio);
        std::thread::spawn(|| {
            let rc = libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0);
            eprintln!("RESULT boosted rc={rc} qos={}", qos());
        }).join().unwrap();
    }
    let hogs: usize = std::env::var("HOGS").ok().and_then(|s| s.parse().ok()).unwrap_or(0);
    let dir = tempfile::tempdir().unwrap();
    let build = dir.path().join("b");
    counter::make_build(&build, 600, "0.5");
    let js = Jobserver::create(dir.path(), 6).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let hs: Vec<_> = (0..hogs).map(|_| { let s = stop.clone(); std::thread::spawn(move || { let mut x = 0u64; while !s.load(Ordering::Relaxed) { x = x.wrapping_add(1); } x }) }).collect();
    let mut makes: Vec<_> = ["a","b"].iter().map(|n| Command::new("make").current_dir(&build)
        .env("MAKEFLAGS", format!("--jobserver-auth=fifo:{}", js.path().display()))
        .env("COUNTER_NAME", n).arg("run").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap()).collect();
    std::thread::sleep(Duration::from_secs(1));
    let mut short = 0; let mut times = vec![];
    for r in 0..30 {
        let t = Instant::now();
        let got = if std::env::var("QOS").is_ok() { qos_block(js.path(), 2) } else if std::env::var("SPIN").is_ok() { spin(js.path(), 2, Duration::from_millis(2000)) } else { js.acquire(2, Duration::from_millis(2000)).unwrap() };
        times.push(t.elapsed().as_millis());
        if got < 2 { short += 1; eprintln!("round {r}: got {got}"); }
        js.release(got).unwrap();
        std::thread::sleep(Duration::from_millis(300));
    }
    times.sort();
    eprintln!("RESULT spin={} hogs={hogs}", std::env::var("SPIN").is_ok());
    eprintln!("RESULT hogs={hogs} short={short}/30 median={}ms p90={}ms max={}ms", times[15], times[27], times[29]);
    stop.store(true, Ordering::Relaxed);
    for h in hs { h.join().unwrap(); }
    for m in &mut makes { let _ = m.kill(); let _ = m.wait(); }
}

fn spin(path: &std::path::Path, n: u32, deadline: Duration) -> u32 {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(path).unwrap();
    let end = Instant::now() + deadline;
    let mut got = 0u32; let mut buf = [0u8; 8];
    while got < n && Instant::now() < end {
        match f.read(&mut buf[..(n - got) as usize]) {
            Ok(k) => got += k as u32,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => { if std::env::var("SPIN").as_deref() == Ok("yield") { std::thread::yield_now() } else { std::hint::spin_loop() } }
            Err(e) => panic!("{e}"),
        }
    }
    got
}

fn qos_block(path: &std::path::Path, n: u32) -> u32 {
    use std::io::Read;
    let p = path.to_path_buf();
    std::thread::spawn(move || {
        #[cfg(target_os = "macos")]
        unsafe { libc::pthread_set_qos_class_self_np(libc::qos_class_t::QOS_CLASS_USER_INTERACTIVE, 0); }
        let mut f = std::fs::File::open(&p).unwrap();
        let mut got = 0u32; let mut buf = [0u8; 8];
        while got < n { got += f.read(&mut buf[..(n - got) as usize]).unwrap() as u32; }
        got
    }).join().unwrap()
}

fn qos() -> String {
    let mut q = libc::qos_class_t::QOS_CLASS_UNSPECIFIED;
    let mut rel = 0;
    let rc = unsafe { libc::pthread_get_qos_class_np(libc::pthread_self(), &mut q, &mut rel) };
    format!("{:?}/rc{rc}", q as u32)
}
