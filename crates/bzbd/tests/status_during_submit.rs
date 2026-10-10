//! `bzbd` answers `status` while a lease's pueued submission is in progress.
//!
//! Regression for the defect where `bzbd` stopped answering `status` for up to
//! 3 s while it waited for an auto-spawned pueued to become reachable.

mod common;

use std::{os::unix::fs::PermissionsExt as _, time::Duration};

use bzb_core::protocol::LeaseEvent;
use common::{event, request, status, Fixture};

/// A directory containing a `pueued` stub that exits immediately without ever
/// binding a socket.  `bzbd` can spawn it but will never reach it, so it
/// retries for the full 3 s reachability window.
fn stub_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("create stub tempdir");
    let path = dir.path().join("pueued");
    std::fs::write(&path, "#!/bin/sh\nexit 0\n").expect("write stub pueued");
    let mut perms = std::fs::metadata(&path).expect("stat stub").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod stub pueued");
    dir
}

/// A minimal pueued YAML config whose socket will never be created.
/// The shared directory is created so `Settings::read` can parse the file;
/// the socket path named inside it is never bound.
fn unreachable_pueued_config() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("create pueued cfg tempdir");
    let shared = dir.path().join("shared");
    std::fs::create_dir_all(&shared).expect("create shared dir");
    let socket = dir.path().join("pueue.sock");
    let config_path = dir.path().join("pueue.yml");
    std::fs::write(
        &config_path,
        format!(
            "shared:\n  pueue_directory: {shared}\n  runtime_directory: {shared}\n  use_unix_socket: true\n  unix_socket_path: {socket}\n",
            shared = shared.display(),
            socket = socket.display(),
        ),
    )
    .expect("write pueued config");
    (dir, config_path)
}

/// Starts a `bzbd` whose pueued is permanently unreachable.  The stub on
/// `PATH` accepts the spawn but never binds, so every connection attempt fails
/// and `connect_or_spawn` burns its full 3-second window before returning an
/// error.
fn start_with_unreachable_pueued() -> (Fixture, tempfile::TempDir, tempfile::TempDir) {
    let stub = stub_dir();
    let (cfg_dir, config) = unreachable_pueued_config();
    let original_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", stub.path().display(), original_path);
    let daemon = Fixture::start_with(
        None,
        &[
            ("PATH", new_path),
            ("PUEUE_CONFIG_PATH", config.display().to_string()),
        ],
    );
    (daemon, stub, cfg_dir)
}

/// `bzbd` must answer `status` within 1 s even while it is waiting for
/// pueued to become reachable (the 3-second auto-spawn window).
///
/// On `main` this test times out because the lease actor blocks in
/// `connect_or_spawn` and does not process the `Status` command until the
/// 3-second window expires.
#[tokio::test]
async fn status_answers_while_submission_waits_for_pueued() {
    let (daemon, _stub, _cfg) = start_with_unreachable_pueued();

    // Submit a lease; admission is immediate and the actor enters the
    // pueued-reachability wait.
    let mut conn = common::submit(&daemon, request(&["cargo", "build"])).await;
    let lease_id = match event(&mut conn).await {
        LeaseEvent::Queued { id, .. } => id,
        other => panic!("expected Queued, got {other:?}"),
    };

    // Let the actor get a head start on the submission wait.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Status must answer while the submission is still in flight (< 1 s
    // budget; the submission window is 3 s, so there is plenty of time left).
    let reply = tokio::time::timeout(Duration::from_secs(1), status(&daemon))
        .await
        .expect("bzbd was unresponsive for 1 s while the submission waited for pueued");

    assert!(
        reply.leases.iter().any(|l| l.id == lease_id),
        "the lease was not reported in status: {:?}",
        reply.leases
    );
}

/// After the 3-second window expires the client receives an error event and
/// every token is back in the pool.
#[tokio::test]
async fn failed_submission_releases_its_lease() {
    let (daemon, _stub, _cfg) = start_with_unreachable_pueued();

    let mut conn = common::submit(&daemon, request(&["cargo", "build"])).await;
    match event(&mut conn).await {
        LeaseEvent::Queued { .. } => {}
        other => panic!("expected Queued, got {other:?}"),
    }

    // Wait for the submission to exhaust its 3-second window and end the lease.
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            match event(&mut conn).await {
                LeaseEvent::Finished { .. } => break,
                // The daemon sends a notice with the submission error before Finished.
                LeaseEvent::Notice { .. } => {}
                other => panic!("unexpected event {other:?}"),
            }
        }
    })
    .await
    .expect("the submission never ended within 10 s");

    let reply = status(&daemon).await;
    assert!(
        reply.leases.is_empty(),
        "lease was still tracked after failure: {:?}",
        reply.leases
    );
    assert_eq!(
        reply.free, reply.pool_size,
        "tokens were not returned: free={} pool={}",
        reply.free, reply.pool_size
    );
}
