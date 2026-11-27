mod common;

use std::{collections::HashSet, fs, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use snap_box::{
    api,
    errors::SnapBoxError,
    lifecycle::SandboxState,
    sandbox::{CommandRequest, SandboxRecord},
};
use tokio::sync::watch;
use tower::ServiceExt;

#[tokio::test]
async fn two_clients_receive_independent_isolated_workspaces() {
    let (_tmp, pool) = common::create_test_pool(2, 4).await;

    let sb_a = pool.allocate(Some("agent-a".into()), None).await.unwrap();
    let sb_b = pool.allocate(Some("agent-b".into()), None).await.unwrap();

    assert_ne!(sb_a.id, sb_b.id);
    assert_ne!(sb_a.paths.workspace, sb_b.paths.workspace);

    // Mutate file in sandbox A and verify sandbox B and base remain untouched.
    fs::write(sb_a.paths.workspace.join("src/hello.txt"), "mutated by A\n").unwrap();
    fs::write(sb_a.paths.workspace.join("only_in_a.txt"), "secret-a\n").unwrap();

    let b_hello = fs::read_to_string(sb_b.paths.workspace.join("src/hello.txt")).unwrap();
    assert_eq!(b_hello, "immutable base content\n");
    assert!(!sb_b.paths.workspace.join("only_in_a.txt").exists());
    assert!(pool
        .backend()
        .verify_base_integrity(pool.base_image())
        .unwrap());

    pool.shutdown_all().await;
}

#[tokio::test]
async fn execute_release_reset_and_reallocate_restores_clean_state() {
    let (_tmp, pool) = common::create_test_pool(1, 2).await;

    let sb = pool.allocate(Some("ci-runner".into()), None).await.unwrap();
    let res = pool
        .execute(
            sb.id,
            CommandRequest {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "echo 'dirty' > dirty.txt && echo 'changed' > src/hello.txt".into(),
                ],
                cwd_rel: None,
                env: Default::default(),
                timeout_ms: Some(5_000),
            },
        )
        .await
        .unwrap();
    assert_eq!(res.exit_code, Some(0));
    assert!(sb.paths.workspace.join("dirty.txt").exists());

    let released = pool.release(sb.id).await.unwrap();
    assert_eq!(released.state, SandboxState::Warm);
    assert_eq!(released.reuse_count, 1);

    // Re-allocate the recycled sandbox and verify clean base state.
    let sb2 = pool
        .allocate(Some("next-agent".into()), None)
        .await
        .unwrap();
    assert_eq!(sb2.id, sb.id);
    assert!(!sb2.paths.workspace.join("dirty.txt").exists());
    assert_eq!(
        fs::read_to_string(sb2.paths.workspace.join("src/hello.txt")).unwrap(),
        "immutable base content\n"
    );

    pool.shutdown_all().await;
}

#[tokio::test]
async fn in_place_reset_discards_changes_while_retaining_lease() {
    let (_tmp, pool) = common::create_test_pool(1, 2).await;
    let sb = pool.allocate(Some("owner-1".into()), None).await.unwrap();

    fs::write(sb.paths.workspace.join("temp.log"), "noise").unwrap();
    let reset = pool.reset_in_place(sb.id).await.unwrap();
    assert_eq!(reset.state, SandboxState::Allocated);
    assert_eq!(reset.owner.as_deref(), Some("owner-1"));
    assert_eq!(reset.reuse_count, 1);
    assert!(!reset.paths.workspace.join("temp.log").exists());

    pool.shutdown_all().await;
}

#[tokio::test]
async fn concurrent_allocations_prevent_double_allocation_and_enforce_cap() {
    let (_tmp, pool) = common::create_test_pool(4, 12).await;

    let mut handles = Vec::new();
    for i in 0..12 {
        let p = Arc::clone(&pool);
        handles.push(tokio::spawn(async move {
            p.allocate(Some(format!("worker-{i}")), None).await
        }));
    }

    let mut seen_ids = HashSet::new();
    for h in handles {
        let sb = h.await.unwrap().unwrap();
        assert!(seen_ids.insert(sb.id), "duplicate sandbox ID allocated!");
    }
    assert_eq!(seen_ids.len(), 12);

    // 13th concurrent request must fail cleanly with PoolExhausted.
    let err = pool
        .allocate(Some("overflow".into()), None)
        .await
        .unwrap_err();
    assert!(matches!(err, SnapBoxError::PoolExhausted { .. }));

    pool.shutdown_all().await;
}

#[tokio::test]
async fn expired_lease_is_reaped_and_replenished_automatically() {
    let (_tmp, pool) = common::create_test_pool(2, 4).await;
    let (tx, rx) = watch::channel(false);
    pool.start_background_tasks(rx);

    let sb = pool
        .allocate(Some("short-lease".into()), Some(1))
        .await
        .unwrap();
    assert_eq!(sb.state, SandboxState::Allocated);

    // Wait for 1s lease to expire and background reaper (50ms interval) to reclaim it.
    tokio::time::sleep(Duration::from_millis(1400)).await;

    let status = pool.status();
    assert_eq!(status.allocated_count, 0);
    assert_eq!(status.warm_count, 2);
    match pool.get(sb.id) {
        Ok(rec) => assert_eq!(rec.state, SandboxState::Warm),
        Err(SnapBoxError::SandboxNotFound(_)) => assert!(!sb.paths.root.exists()),
        Err(other) => panic!("unexpected error inspecting reaped sandbox: {other}"),
    }

    let _ = tx.send(true);
    pool.shutdown_all().await;
}

#[tokio::test]
async fn rest_api_end_to_end_lifecycle_works() {
    let (_tmp, pool) = common::create_test_pool(2, 4).await;
    let app = api::build_router(Arc::clone(&pool), None);

    // 1. Health check
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 2. Allocate sandbox
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/v1/sandboxes/allocate")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"owner":"api-client","ttl_secs":120}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::CREATED);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let allocated: SandboxRecord = serde_json::from_slice(&body).unwrap();
    assert_eq!(allocated.state, SandboxState::Allocated);

    // 3. Exec command
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/sandboxes/{}/exec", allocated.id))
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"program":"/bin/sh","args":["-c","cat README.md"]}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let exec_bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let exec_json: serde_json::Value = serde_json::from_slice(&exec_bytes).unwrap();
    assert_eq!(exec_json["exit_code"], 0);
    assert!(exec_json["stdout"]
        .as_str()
        .unwrap()
        .contains("Seed Project"));

    // 4. Release sandbox
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/api/v1/sandboxes/{}/release", allocated.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 5. Invalid UUID returns 400
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/sandboxes/not-a-uuid")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // 6. Delete sandbox
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/v1/sandboxes/{}", allocated.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 7. Subsequent GET returns 404
    let resp = app
        .oneshot(
            Request::builder()
                .uri(format!("/api/v1/sandboxes/{}", allocated.id))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    pool.shutdown_all().await;
}
