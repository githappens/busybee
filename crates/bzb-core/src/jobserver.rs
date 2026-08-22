//! The machine-wide CPU token pool: a GNU make 4.4-style fifo jobserver.
//!
//! The protocol, in full:
//!
//! 1. The pool is a named pipe holding `pool_size` single-byte tokens.
//! 2. A build joins when its environment carries
//!    `MAKEFLAGS=--jobserver-auth=fifo:<path>`. An explicit `-j` on the
//!    build's own command line makes it ignore the pool instead.
//! 3. Every participant may run one job without a token (the implicit job).
//! 4. Before starting any further job it reads one byte from the pipe,
//!    blocking until one is available.
//! 5. When that job exits it writes the byte back.
//! 6. Token content is irrelevant (`+`); only counts matter.
//!
//! So at most `pool_size + <participants>` jobs run at once, and tokens move
//! between builds at job granularity with no scheduler involved. Make, ninja
//! and cargo all speak this dialect.
//!
//! The daemon keeps an `O_RDWR | O_NONBLOCK` handle open: the pipe never
//! reports EOF when the last build exits, and creation never blocks waiting
//! for a peer. Reads and `FIONREAD` go through a separate read-only handle:
//! on macOS a fifo is a socket pair and `FIONREAD` on a read-write descriptor
//! reports the write side, which is always empty. The path carries the
//! creating pid so a restarted daemon gets a fresh pipe while orphaned builds
//! keep reading the old one.

use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Smallest pipe capacity on macOS and Linux; every token must fit at once.
const MAX_POOL: u32 = 4096;

pub struct Jobserver {
    path: PathBuf,
    /// Opened read-write so the fifo stays alive with zero clients; tokens
    /// are written back through it.
    fd_rw: File,
    /// Read-only handle for `read` and `FIONREAD`.
    fd_r: File,
    pool_size: u32,
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
        assert!(
            pool_size <= MAX_POOL,
            "pool_size {pool_size} exceeds the {MAX_POOL}-byte pipe capacity"
        );
        let path = dir.join(format!("jobserver-{}", std::process::id()));
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
            Ok((rw, r))
        });
        let (fd_rw, fd_r) = match handles {
            Ok(h) => h,
            Err(err) => {
                let _ = fs::remove_file(&path);
                return Err(err);
            }
        };
        let js = Self {
            path,
            fd_rw,
            fd_r,
            pool_size,
        };
        js.release(pool_size)?;
        Ok(js)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn pool_size(&self) -> u32 {
        self.pool_size
    }

    /// The `MAKEFLAGS` value a build needs to join this pool.
    pub fn makeflags_value(&self) -> String {
        format!("--jobserver-auth=fifo:{}", self.path.display())
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

    /// Take up to `n` tokens, sleeping in `poll(2)` until tokens arrive or
    /// `deadline` elapses. Returns how many were taken (`0..=n`); the caller
    /// owns them until it calls [`release`](Self::release).
    pub fn acquire(&self, n: u32, deadline: Duration) -> io::Result<u32> {
        let end = Instant::now() + deadline;
        let mut got = 0u32;
        let mut buf = vec![0u8; n as usize];
        while got < n {
            match (&self.fd_r).read(&mut buf[..(n - got) as usize]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "jobserver fifo reported EOF despite the held write end",
                    ))
                }
                Ok(k) => got += k as u32,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                    let now = Instant::now();
                    if now >= end {
                        break;
                    }
                    let timeout_ms = (end - now).as_micros().div_ceil(1000).min(i32::MAX as u128);
                    let mut pfd = libc::pollfd {
                        fd: self.fd_r.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: pfd is a valid array of one pollfd.
                    if unsafe { libc::poll(&mut pfd, 1, timeout_ms as i32) } < 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() != io::ErrorKind::Interrupted {
                            return Err(err);
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(got)
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
}

impl Drop for Jobserver {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}
