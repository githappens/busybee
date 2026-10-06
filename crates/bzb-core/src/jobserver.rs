//! The machine-wide CPU token pool: a GNU make 4.4-style fifo jobserver
//! (`docs/design/bzbd.md` §Jobserver).
//!
//! The daemon holds an `O_RDWR` handle so the fifo never reports EOF when the
//! last build exits. Reads and `FIONREAD` use a separate read-only handle: on
//! macOS a fifo is a socket pair and `FIONREAD` on a read-write descriptor
//! reports the always-empty write side.
//!
//! A drain waits for tokens in a blocking `read`, never in `poll`. A build's
//! job slots wait for their next token blocked in `read`; the kernel hands a
//! returned token to a reader it wakes in place, while a `poll` waiter has to
//! come back out and read, and by then the byte is gone. A blocking read has
//! no deadline, so it runs on a drainer thread and [`acquire`](Jobserver::acquire)
//! waits for that thread's tokens up to its own deadline. A token the drainer
//! reads after the caller stopped waiting goes straight back into the pipe.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

/// Smallest pipe capacity on macOS and Linux; every token must fit at once.
pub const MAX_POOL: u32 = 4096;

pub struct Jobserver {
    path: PathBuf,
    /// Opened read-write so the fifo stays alive with zero clients; tokens
    /// are written back through it.
    fd_rw: File,
    /// Read-only handle for non-blocking `read` and `FIONREAD`.
    fd_r: File,
    /// Shared with the drainer thread, which owns the blocking read handle.
    drain: Arc<Drain>,
    /// Set by [`leave`](Self::leave): `Drop` keeps the fifo.
    left: bool,
}

fn open_nonblocking(path: &Path, write: bool) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(write)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
}

impl Jobserver {
    /// Create `<dir>/jobserver-<pid>` (mode 0600), open it, and seed it with
    /// `pool_size` tokens.
    pub fn create(dir: &Path, pool_size: u32) -> io::Result<Self> {
        if pool_size > MAX_POOL {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("pool_size {pool_size} exceeds the {MAX_POOL}-byte pipe capacity"),
            ));
        }
        // Clients split MAKEFLAGS on whitespace (make, ninja, the `jobserver`
        // crate), so a path containing any is silently truncated; reject it.
        let Some(dir) = dir.to_str().filter(|s| !s.contains(char::is_whitespace)) else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "jobserver directory {} contains whitespace or non-UTF-8 bytes; MAKEFLAGS cannot carry it",
                    dir.display()
                ),
            ));
        };
        let path = Path::new(dir).join(format!("jobserver-{}", std::process::id()));
        let c_path = CString::new(path.as_os_str().as_bytes())?;
        // SAFETY: c_path is a valid NUL-terminated string for the call's duration.
        if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
            let err = io::Error::last_os_error();
            return Err(io::Error::new(
                err.kind(),
                format!("mkfifo {}: {err}", path.display()),
            ));
        }
        let handles = open_nonblocking(&path, true).and_then(|rw| {
            let r = open_nonblocking(&path, false)?;
            let drain = Drain::start(&path)?;
            Ok((rw, r, drain))
        });
        let (fd_rw, fd_r, drain) = match handles {
            Ok(h) => h,
            Err(err) => {
                return Err(match unlink(&path) {
                    Ok(()) => err,
                    Err(u) => {
                        io::Error::new(err.kind(), format!("{err}; cleanup also failed: {u}"))
                    }
                })
            }
        };
        let js = Self {
            path,
            fd_rw,
            fd_r,
            drain,
            left: false,
        };
        js.release(pool_size)?;
        Ok(js)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Tokens currently in the pipe (`FIONREAD`). A snapshot: any participant
    /// may read or write a token the instant after this returns.
    pub fn free(&self) -> io::Result<u32> {
        let mut n: libc::c_int = 0;
        // SAFETY: fd is open for the lifetime of self; FIONREAD writes one c_int.
        if unsafe { libc::ioctl(self.fd_r.as_raw_fd(), libc::FIONREAD, &mut n) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(n as u32)
    }

    /// Take up to `n` tokens: those in the pipe now, then whatever the
    /// drainer's blocking read collects before `deadline` elapses. A zero
    /// `deadline` takes only what is in the pipe now. Returns how many were
    /// taken (`0..=n`); the caller owns them until it calls
    /// [`release`](Self::release). On error nothing is owned: tokens read
    /// before the failure are written back first.
    pub fn acquire(&self, n: u32, deadline: Duration) -> io::Result<u32> {
        let mut got = 0u32;
        match self.read_tokens(n, deadline, &mut got) {
            Ok(()) => Ok(got),
            Err(err) => match self.release(got) {
                Ok(()) => Err(err),
                Err(rel) => Err(io::Error::new(
                    err.kind(),
                    format!("{err}; returning {got} partially acquired tokens also failed: {rel}"),
                )),
            },
        }
    }

    /// Body of [`acquire`](Self::acquire); `got` counts tokens read so far
    /// so the caller can return them when this fails.
    fn read_tokens(&self, n: u32, deadline: Duration, got: &mut u32) -> io::Result<()> {
        let end = Instant::now() + deadline;
        let mut buf = vec![0u8; n as usize];
        while *got < n {
            match (&self.fd_r).read(&mut buf[..(n - *got) as usize]) {
                Ok(0) => return Err(eof()),
                Ok(k) => *got += k as u32,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        if *got < n && !deadline.is_zero() {
            self.drain.collect(n - *got, end, got)?;
        }
        Ok(())
    }

    /// Write `n` tokens back into the pool.
    pub fn release(&self, n: u32) -> io::Result<()> {
        (&self.fd_rw).write_all(&vec![b'+'; n as usize])
    }

    /// Remove tokens above `expected_free` (a tool wrote bytes it never
    /// read). Returns how many were removed. Acts on a `free()` snapshot.
    pub fn drain_excess(&self, expected_free: u32) -> io::Result<u32> {
        let excess = self.free()?.saturating_sub(expected_free);
        if excess == 0 {
            return Ok(0);
        }
        self.acquire(excess, Duration::ZERO)
    }

    /// Keep the fifo when `self` is dropped: builds still holding it (a
    /// recursive make reopens the path per sub-make) keep working, and the
    /// next daemon's recovery unlinks it (spec §Failure and recovery).
    pub fn leave(&mut self) {
        self.left = true;
    }
}

fn eof() -> io::Error {
    io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "jobserver fifo reported EOF despite the held write end",
    )
}

/// The drainer thread's side of [`Jobserver::acquire`]: a request for tokens
/// and what the blocking read has delivered against it.
struct Drain {
    state: Mutex<DrainState>,
    changed: Condvar,
}

#[derive(Default)]
struct DrainState {
    /// Tokens the current `acquire` still waits for; 0 while none waits.
    want: u32,
    /// Tokens delivered to the current `acquire`.
    got: u32,
    /// A read or write-back failure, reported by the next `acquire`.
    err: Option<io::Error>,
    /// Set when the `Jobserver` is dropped.
    shutdown: bool,
}

impl Drain {
    /// Open a blocking read handle on the fifo at `path` and start the
    /// drainer thread on it.
    fn start(path: &Path) -> io::Result<Arc<Self>> {
        // Opened non-blocking so the open never waits for a writer, then
        // switched to blocking reads.
        let fd = open_nonblocking(path, false)?;
        // SAFETY: fd is open; F_GETFL/F_SETFL only read and set its status flags.
        let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFL) };
        if flags < 0
            || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFL, flags & !libc::O_NONBLOCK) } < 0
        {
            return Err(io::Error::last_os_error());
        }
        let drain = Arc::new(Self {
            state: Mutex::new(DrainState::default()),
            changed: Condvar::new(),
        });
        let shared = Arc::clone(&drain);
        let path = path.to_path_buf();
        thread::Builder::new()
            .name("jobserver-drain".into())
            .spawn(move || shared.run(fd, &path))?;
        Ok(drain)
    }

    fn lock(&self) -> MutexGuard<'_, DrainState> {
        self.state.lock().expect("jobserver drain state poisoned")
    }

    /// Wait until the drainer delivers `n` more tokens or `end` passes; adds
    /// what it delivered to `got`.
    fn collect(&self, n: u32, end: Instant, got: &mut u32) -> io::Result<()> {
        let mut st = self.lock();
        if let Some(err) = st.err.take() {
            return Err(err);
        }
        st.want = n;
        st.got = 0;
        self.changed.notify_all();
        loop {
            let now = Instant::now();
            if st.want == 0 || st.err.is_some() || now >= end {
                break;
            }
            st = self
                .changed
                .wait_timeout(st, end - now)
                .expect("jobserver drain state poisoned")
                .0;
        }
        // From here a token the drainer reads is surplus and goes back.
        st.want = 0;
        *got += std::mem::take(&mut st.got);
        match st.err.take() {
            Some(err) => Err(err),
            None => Ok(()),
        }
    }

    /// The drainer thread: while an `acquire` waits, block in `read` for its
    /// tokens. Exits on shutdown once its read returns; the read returns EOF
    /// when the last write end closes.
    fn run(&self, mut fd: File, path: &Path) {
        let mut buf = Vec::new();
        loop {
            let ask = {
                let mut st = self.lock();
                while st.want == 0 && !st.shutdown {
                    st = self
                        .changed
                        .wait(st)
                        .expect("jobserver drain state poisoned");
                }
                if st.shutdown {
                    return;
                }
                st.want
            };
            buf.resize(ask as usize, 0);
            let read = fd.read(&mut buf);
            let mut st = self.lock();
            if st.shutdown {
                // A `left` fifo still serves builds, so its token goes back; an
                // unlinked one has no pool to return to.
                if let Ok(k @ 1..) = read {
                    match write_back(path, k as u32) {
                        Err(e) if e.kind() != io::ErrorKind::NotFound => {
                            eprintln!("warning: jobserver drainer could not return {k} tokens: {e}")
                        }
                        _ => {}
                    }
                }
                return;
            }
            let surplus = match read {
                Ok(0) => {
                    st.err = Some(eof());
                    0
                }
                Ok(k) => {
                    let k = k as u32;
                    let take = k.min(st.want);
                    st.got += take;
                    st.want -= take;
                    k - take
                }
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => {
                    st.err = Some(e);
                    0
                }
            };
            if surplus > 0 {
                if let Err(e) = write_back(path, surplus) {
                    st.err = Some(io::Error::new(
                        e.kind(),
                        format!("the drainer could not return {surplus} late tokens: {e}"),
                    ));
                }
            }
            if st.err.is_some() {
                // An error stops the drainer until the next request.
                st.want = 0;
            }
            drop(st);
            self.changed.notify_all();
        }
    }
}

/// Return `n` tokens through a write handle of their own: the drainer keeps no
/// write end, so its read sees EOF once the `Jobserver` and every build close
/// theirs.
fn write_back(path: &Path, n: u32) -> io::Result<()> {
    OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?
        .write_all(&vec![b'+'; n as usize])
}

fn unlink(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
        .map_err(|e| io::Error::new(e.kind(), format!("unlink {}: {e}", path.display())))
}

impl Drop for Jobserver {
    fn drop(&mut self) {
        self.drain.lock().shutdown = true;
        self.drain.changed.notify_all();
        if self.left {
            return;
        }
        if let Err(e) = unlink(&self.path) {
            eprintln!("warning: jobserver fifo left behind: {e}");
        }
    }
}
