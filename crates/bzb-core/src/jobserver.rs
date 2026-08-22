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
//! keep reading the old one. `MAKEFLAGS` has no quoting, so [`Jobserver::create`]
//! refuses directories containing whitespace rather than hand out a path
//! clients would truncate.

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
    /// Set by [`close`](Self::close), whose caller owns the unlink result;
    /// `Drop` then neither retries nor reports it a second time.
    closed: bool,
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
        // A configuration error rather than a bug: the pool size comes from
        // the user, so it is reported, not asserted.
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
            Ok((rw, r))
        });
        let (fd_rw, fd_r) = match handles {
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
            pool_size,
            closed: false,
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
    /// owns them until it calls [`release`](Self::release). On error nothing
    /// is owned: tokens read before the failure are written back first.
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
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "jobserver fifo reported EOF despite the held write end",
                    ))
                }
                Ok(k) => *got += k as u32,
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

    /// Unlink the fifo and report failure to do so. Prefer this over `Drop`
    /// at daemon shutdown: `Drop` can only warn on stderr, and a stale
    /// `jobserver-<pid>` makes a later process with that pid fail `create`
    /// with `EEXIST`. The descriptors close when `self` is dropped either way.
    pub fn close(mut self) -> io::Result<()> {
        self.closed = true;
        unlink(&self.path)
    }

    /// Keep the fifo when `self` is dropped. A daemon shutting down leaves it
    /// to the builds that hold it open — a recursive make opens the path
    /// again for every sub-make — and the next daemon's recovery unlinks it
    /// once they are gone (`docs/design/bzbd.md` §Failure and recovery). The
    /// descriptors still close with `self`.
    pub fn leave(&mut self) {
        self.closed = true;
    }
}

fn unlink(path: &Path) -> io::Result<()> {
    fs::remove_file(path)
        .map_err(|e| io::Error::new(e.kind(), format!("unlink {}: {e}", path.display())))
}

impl Drop for Jobserver {
    fn drop(&mut self) {
        if self.closed {
            return;
        }
        if let Err(e) = unlink(&self.path) {
            eprintln!("warning: jobserver fifo left behind: {e}");
        }
    }
}
