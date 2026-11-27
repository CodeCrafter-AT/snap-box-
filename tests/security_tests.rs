mod common;

use std::{collections::HashMap, os::unix::fs::symlink, time::Instant};

use snap_box::{errors::SnapBoxError, sandbox::CommandRequest};

#[tokio::test]
async fn base_filesystem_remains_unchanged_after_destructive_sandbox_writes() {
    let (_tmp, pool) = common::create_test_pool(1, 2).await;
    let initial_sha = pool.base_image().manifest_sha256.clone();

    let sb = pool.allocate(Some("mutator".into()), None).await.unwrap();
    let res = pool
        .execute(
            sb.id,
            CommandRequest {
                program: "/bin/sh".into(),
                args: vec![
                    "-c".into(),
                    "rm -rf src README.md && echo pwned > pwned.txt".into(),
                ],
                cwd_rel: None,
                env: HashMap::new(),
                timeout_ms: Some(5_000),
            },
        )
        .await
        .unwrap();
    assert_eq!(res.exit_code, Some(0));

    assert!(pool
        .backend()
        .verify_base_integrity(pool.base_image())
        .unwrap());
    assert_eq!(pool.base_image().manifest_sha256, initial_sha);

    pool.shutdown_all().await;
}

#[tokio::test]
async fn path_traversal_and_symlink_escape_in_cwd_are_blocked() {
    let (_tmp, pool) = common::create_test_pool(1, 2).await;
    let sb = pool
        .allocate(Some("escape-test".into()), None)
        .await
        .unwrap();

    // Attempt parent traversal
    let err = pool
        .execute(
            sb.id,
            CommandRequest {
                program: "/bin/pwd".into(),
                args: vec![],
                cwd_rel: Some("../../".into()),
                env: HashMap::new(),
                timeout_ms: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SnapBoxError::SecurityViolation(_)));

    // Attempt symlink escape out of workspace
    let evil_link = sb.paths.workspace.join("escape_link");
    symlink("/tmp", &evil_link).unwrap();

    let err2 = pool
        .execute(
            sb.id,
            CommandRequest {
                program: "/bin/pwd".into(),
                args: vec![],
                cwd_rel: Some("escape_link".into()),
                env: HashMap::new(),
                timeout_ms: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err2, SnapBoxError::SecurityViolation(_)));

    pool.shutdown_all().await;
}

#[tokio::test]
async fn host_secrets_are_not_exposed_and_loader_injection_is_rejected() {
    let (_tmp, pool) = common::create_test_pool(1, 2).await;
    let sb = pool.allocate(Some("env-test".into()), None).await.unwrap();

    std::env::set_var("OPENAI_API_KEY", "sk-secret-test-value");
    std::env::set_var("AWS_SECRET_ACCESS_KEY", "aws-secret-test-value");

    let res = pool
        .execute(
            sb.id,
            CommandRequest {
                program: "/usr/bin/env".into(),
                args: vec![],
                cwd_rel: None,
                env: HashMap::new(),
                timeout_ms: Some(3_000),
            },
        )
        .await
        .unwrap();
    assert!(!res.stdout.contains("sk-secret-test-value"));
    assert!(!res.stdout.contains("aws-secret-test-value"));

    let mut bad_env = HashMap::new();
    bad_env.insert("LD_PRELOAD".into(), "/tmp/malicious.so".into());
    let err = pool
        .execute(
            sb.id,
            CommandRequest {
                program: "/bin/echo".into(),
                args: vec!["hi".into()],
                cwd_rel: None,
                env: bad_env,
                timeout_ms: None,
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(err, SnapBoxError::SecurityViolation(_)));

    pool.shutdown_all().await;
}

#[tokio::test]
async fn runaway_process_is_terminated_on_timeout_and_workspace_cleaned_on_destroy() {
    let (_tmp, pool) = common::create_test_pool(1, 2).await;
    let sb = pool
        .allocate(Some("timeout-test".into()), None)
        .await
        .unwrap();
    let ws_root = sb.paths.root.clone();

    let started = Instant::now();
    let res = pool
        .execute(
            sb.id,
            CommandRequest {
                program: "/bin/sleep".into(),
                args: vec!["15".into()],
                cwd_rel: None,
                env: HashMap::new(),
                timeout_ms: Some(150),
            },
        )
        .await
        .unwrap();
    assert!(res.timed_out);
    assert!(started.elapsed().as_millis() < 2_000);

    pool.destroy(sb.id).await.unwrap();
    assert!(!ws_root.exists());

    pool.shutdown_all().await;
}
