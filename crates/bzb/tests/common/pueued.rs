use std::{
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread::sleep,
    time::{Duration, Instant},
};
use tempfile::TempDir;

/// Spawns an isolated `pueued` in a tempdir with its own socket. Kills it on
/// `Drop`. Tests skip themselves (ignored) when `pueued` is not on PATH — this
/// keeps `cargo test` green on machines without pueue installed.
pub struct PueuedFixture {
    child: Child,
    _tmp: TempDir,
    pub socket_path: PathBuf,
    pub config_path: PathBuf,
}

impl PueuedFixture {
    pub fn try_start() -> Option<Self> {
        if which::which("pueued").is_err() {
            eprintln!("pueued not on PATH; skipping integration test");
            return None;
        }
        let tmp = TempDir::new().expect("create tempdir");
        let config_path = tmp.path().join("pueue.yml");
        let socket_path = tmp.path().join("pueue.sock");
        let shared_dir = tmp.path().join("shared");
        std::fs::create_dir_all(&shared_dir).unwrap();

        // Minimal pueue 4.x config. Only `shared` keys needed; groups are a
        // runtime concept, not config.
        let config = format!(
            r#"shared:
  pueue_directory: {shared}
  runtime_directory: {shared}
  use_unix_socket: true
  unix_socket_path: {socket}
"#,
            socket = socket_path.display(),
            shared = shared_dir.display(),
        );
        std::fs::write(&config_path, config).unwrap();

        let mut child = Command::new("pueued")
            .arg("--config")
            .arg(&config_path)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn pueued");

        // Wait up to 3s for the socket to appear. A `pueued` that spawns but
        // never binds is a broken fixture, not a reason to skip the test.
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if socket_path.exists() {
                return Some(Self {
                    child,
                    _tmp: tmp,
                    socket_path,
                    config_path,
                });
            }
            sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
        panic!("pueued did not create {} within 3s", socket_path.display());
    }
}

impl Drop for PueuedFixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
