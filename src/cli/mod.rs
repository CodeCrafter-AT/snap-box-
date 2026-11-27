use std::{
    fs,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use clap::{Args, Parser, Subcommand};

use crate::{
    config::SnapBoxConfig,
    daemon::{stop_daemon_by_pid, SnapBoxDaemon},
    errors::{Result, SnapBoxError},
    filesystem::{select_backend, CoWBackend, PortableCopyBackend},
    metrics::{summarize_durations, LatencyStats, MetricsRegistry},
    pool::{PoolStatus, WarmPoolManager},
    sandbox::{CommandRequest, CommandResult, SandboxRecord},
};

#[derive(Debug, Parser)]
#[command(
    name = "snap-box",
    version,
    about = "Sub-second Copy-on-Write sandbox pool engine for AI coding agents and CI workloads"
)]
pub struct Cli {
    /// Base URL of a running snap-box daemon.
    #[arg(
        long,
        env = "SNAP_BOX_ENDPOINT",
        default_value = "http://127.0.0.1:7432",
        global = true
    )]
    pub endpoint: String,

    /// Print raw JSON output instead of formatted terminal tables.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Start the snap-box daemon and initialize the warm CoW pool.
    Start(StartArgs),
    /// Stop a running snap-box daemon cleanly via HTTP or PID file.
    Stop(StopArgs),
    /// Show daemon health, pool summary, and latency metrics.
    Status,
    /// Manage and inspect the warm sandbox pool.
    Pool {
        #[command(subcommand)]
        command: PoolCommands,
    },
    /// Allocate, inspect, execute in, reset, release, or destroy sandboxes.
    Sandbox {
        #[command(subcommand)]
        command: SandboxCommands,
    },
    /// Run reproducible warm-pool CoW vs. cold-copy benchmarks locally.
    Benchmark(BenchmarkArgs),
}

#[derive(Debug, Args)]
pub struct StartArgs {
    #[arg(long, short)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub bind: Option<SocketAddr>,
    #[arg(long)]
    pub data_root: Option<PathBuf>,
    #[arg(long)]
    pub base_dir: Option<PathBuf>,
    #[arg(long)]
    pub pool_size: Option<usize>,
    #[arg(long)]
    pub max_sandboxes: Option<usize>,
    #[arg(long)]
    pub pid_file: Option<PathBuf>,
    #[arg(long)]
    pub allow_non_cow_fallback: bool,
}

#[derive(Debug, Args)]
pub struct StopArgs {
    #[arg(long)]
    pub pid_file: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
pub enum PoolCommands {
    /// Display pool capacity, backend capabilities, and host telemetry.
    List,
}

#[derive(Debug, Subcommand)]
pub enum SandboxCommands {
    /// Allocate an available sandbox from the warm pool.
    Create {
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        ttl_secs: Option<u64>,
    },
    /// List all tracked sandboxes in the pool.
    List,
    /// Inspect detailed metadata and disk usage for a sandbox.
    Inspect { id: String },
    /// Execute a command inside an allocated sandbox workspace.
    Exec {
        id: String,
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        timeout_ms: Option<u64>,
        #[arg(required = true, trailing_var_arg = true, allow_hyphen_values = true)]
        command: Vec<String>,
    },
    /// Release a sandbox, discard changes, and return it to the warm pool.
    Release { id: String },
    /// Reset an allocated sandbox's workspace in-place while keeping its lease.
    Reset { id: String },
    /// Immediately destroy a sandbox and delete its workspace.
    Destroy { id: String },
}

#[derive(Debug, Args)]
pub struct BenchmarkArgs {
    /// Number of sequential warm allocations + rollbacks to measure.
    #[arg(long, default_value_t = 30)]
    pub iterations: usize,
    /// Number of concurrent workers for the parallel burst benchmark.
    #[arg(long, default_value_t = 8)]
    pub concurrency: usize,
    /// Number of warm sandboxes in the benchmark pool.
    #[arg(long, default_value_t = 8)]
    pub pool_size: usize,
    /// Number of files to generate in the benchmark base tree.
    #[arg(long, default_value_t = 200)]
    pub file_count: usize,
    /// Size of each generated file in KiB.
    #[arg(long, default_value_t = 16)]
    pub file_size_kb: usize,
    /// Allow non-CoW fallback if host filesystem lacks native CoW.
    #[arg(long)]
    pub allow_non_cow_fallback: bool,
}

pub async fn run_cli(cli: Cli) -> Result<()> {
    match cli.command {
        Commands::Start(args) => cmd_start(args).await,
        Commands::Stop(args) => cmd_stop(&cli.endpoint, args).await,
        Commands::Status => cmd_status(&cli.endpoint, cli.json).await,
        Commands::Pool { command } => match command {
            PoolCommands::List => cmd_pool_list(&cli.endpoint, cli.json).await,
        },
        Commands::Sandbox { command } => cmd_sandbox(&cli.endpoint, command, cli.json).await,
        Commands::Benchmark(args) => cmd_benchmark(args, cli.json).await,
    }
}

async fn cmd_start(args: StartArgs) -> Result<()> {
    let mut cfg = match &args.config {
        Some(path) => SnapBoxConfig::from_toml_file(path)?,
        None => SnapBoxConfig::default(),
    };

    if let Some(bind) = args.bind {
        cfg.daemon.bind_addr = bind;
    }
    if let Some(root) = args.data_root {
        cfg.storage.data_root = root;
    }
    if let Some(base) = args.base_dir {
        cfg.storage.base_source_dir = Some(base);
    }
    if let Some(pool_size) = args.pool_size {
        cfg.pool.target_warm = pool_size;
        cfg.pool.min_warm = cfg.pool.min_warm.min(pool_size);
        cfg.pool.max_sandboxes = cfg.pool.max_sandboxes.max(pool_size);
    }
    if let Some(max) = args.max_sandboxes {
        cfg.pool.max_sandboxes = max;
    }
    if let Some(pid_file) = args.pid_file {
        cfg.daemon.pid_file = pid_file;
    }
    if args.allow_non_cow_fallback {
        cfg.storage.allow_non_cow_fallback = true;
    }

    let daemon = SnapBoxDaemon::initialize(cfg).await?;
    daemon.serve().await
}

async fn cmd_stop(endpoint: &str, args: StopArgs) -> Result<()> {
    let client = build_http_client(endpoint)?;
    let url = format!("{}/api/v1/daemon/shutdown", endpoint.trim_end_matches('/'));
    match client.post(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            println!("Daemon shutdown signal sent to {endpoint}");
            return Ok(());
        }
        _ => {}
    }

    let pid_file = args
        .pid_file
        .unwrap_or_else(|| SnapBoxConfig::default().daemon.pid_file);
    let pid = stop_daemon_by_pid(&pid_file)?;
    println!("Sent SIGTERM to snap-box daemon (PID {pid})");
    Ok(())
}

async fn cmd_status(endpoint: &str, as_json: bool) -> Result<()> {
    let client = build_http_client(endpoint)?;
    let base = endpoint.trim_end_matches('/');
    let pool: PoolStatus = send_get(&client, &format!("{base}/api/v1/pool")).await?;
    let metrics: serde_json::Value = send_get(&client, &format!("{base}/api/v1/metrics")).await?;

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "pool": pool,
                "metrics": metrics
            }))
            .unwrap_or_default()
        );
        return Ok(());
    }

    print_pool_status(&pool);
    Ok(())
}

async fn cmd_pool_list(endpoint: &str, as_json: bool) -> Result<()> {
    let client = build_http_client(endpoint)?;
    let base = endpoint.trim_end_matches('/');
    let pool: PoolStatus = send_get(&client, &format!("{base}/api/v1/pool")).await?;
    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&pool).unwrap_or_default()
        );
    } else {
        print_pool_status(&pool);
    }
    Ok(())
}

async fn cmd_sandbox(endpoint: &str, cmd: SandboxCommands, as_json: bool) -> Result<()> {
    let client = build_http_client(endpoint)?;
    let base = endpoint.trim_end_matches('/');

    match cmd {
        SandboxCommands::Create { owner, ttl_secs } => {
            let body = serde_json::json!({ "owner": owner, "ttl_secs": ttl_secs });
            let rec: SandboxRecord = send_post(
                &client,
                &format!("{base}/api/v1/sandboxes/allocate"),
                Some(&body),
            )
            .await?;
            print_or_json(&rec, as_json, |r| {
                println!(
                    "Allocated sandbox {}\n  State:     {}\n  Workspace: {}\n  Owner:     {}",
                    r.id,
                    r.state,
                    r.paths.workspace.display(),
                    r.owner.as_deref().unwrap_or("-")
                );
            });
        }
        SandboxCommands::List => {
            let list: Vec<SandboxRecord> =
                send_get(&client, &format!("{base}/api/v1/sandboxes")).await?;
            print_or_json(&list, as_json, |items| {
                println!(
                    "{:<36}  {:<12}  {:<14}  {:<6}  WORKSPACE",
                    "ID", "STATE", "OWNER", "REUSES"
                );
                for item in items {
                    println!(
                        "{:<36}  {:<12}  {:<14}  {:<6}  {}",
                        item.id,
                        item.state,
                        item.owner.as_deref().unwrap_or("-"),
                        item.reuse_count,
                        item.paths.workspace.display()
                    );
                }
            });
        }
        SandboxCommands::Inspect { id } => {
            let detail: serde_json::Value =
                send_get(&client, &format!("{base}/api/v1/sandboxes/{id}")).await?;
            println!(
                "{}",
                serde_json::to_string_pretty(&detail).unwrap_or_default()
            );
        }
        SandboxCommands::Exec {
            id,
            cwd,
            timeout_ms,
            command,
        } => {
            let (program, args) = command
                .split_first()
                .ok_or_else(|| SnapBoxError::Validation("command cannot be empty".into()))?;
            let req = CommandRequest {
                program: program.clone(),
                args: args.to_vec(),
                cwd_rel: cwd,
                env: Default::default(),
                timeout_ms,
            };
            let res: CommandResult = send_post(
                &client,
                &format!("{base}/api/v1/sandboxes/{id}/exec"),
                Some(&req),
            )
            .await?;
            print_or_json(&res, as_json, |r| {
                if !r.stdout.is_empty() {
                    print!("{}", r.stdout);
                }
                if !r.stderr.is_empty() {
                    eprint!("{}", r.stderr);
                }
                println!(
                    "[exit_code={:?}, timed_out={}, duration={}ms]",
                    r.exit_code, r.timed_out, r.duration_ms
                );
            });
        }
        SandboxCommands::Release { id } => {
            let rec: SandboxRecord = send_post::<serde_json::Value, SandboxRecord>(
                &client,
                &format!("{base}/api/v1/sandboxes/{id}/release"),
                None,
            )
            .await?;
            print_or_json(&rec, as_json, |r| {
                println!("Released sandbox {} -> {}", r.id, r.state);
            });
        }
        SandboxCommands::Reset { id } => {
            let rec: SandboxRecord = send_post::<serde_json::Value, SandboxRecord>(
                &client,
                &format!("{base}/api/v1/sandboxes/{id}/reset"),
                None,
            )
            .await?;
            print_or_json(&rec, as_json, |r| {
                println!(
                    "Reset sandbox {} -> {} (reuses={})",
                    r.id, r.state, r.reuse_count
                );
            });
        }
        SandboxCommands::Destroy { id } => {
            let resp = client
                .delete(format!("{base}/api/v1/sandboxes/{id}"))
                .send()
                .await
                .map_err(|e| SnapBoxError::Daemon(format!("HTTP request failed: {e}")))?;
            let rec: SandboxRecord = decode_response(resp).await?;
            print_or_json(&rec, as_json, |r| {
                println!("Destroyed sandbox {} -> {}", r.id, r.state);
            });
        }
    }
    Ok(())
}

async fn cmd_benchmark(args: BenchmarkArgs, as_json: bool) -> Result<()> {
    let temp = tempfile::tempdir().map_err(|e| SnapBoxError::Filesystem {
        operation: "tempdir".into(),
        path: "system_temp".into(),
        source: e,
    })?;

    let source_dir = temp.path().join("seed-project");
    fs::create_dir_all(&source_dir).map_err(|e| SnapBoxError::Filesystem {
        operation: "create_dir_all".into(),
        path: source_dir.display().to_string(),
        source: e,
    })?;
    let payload = vec![b'A'; args.file_size_kb * 1024];
    for idx in 0..args.file_count {
        let sub = source_dir.join(format!("pkg_{}", idx % 10));
        fs::create_dir_all(&sub).map_err(|e| SnapBoxError::Filesystem {
            operation: "create_dir_all".into(),
            path: sub.display().to_string(),
            source: e,
        })?;
        fs::write(sub.join(format!("file_{idx}.dat")), &payload).map_err(|e| {
            SnapBoxError::Filesystem {
                operation: "write".into(),
                path: sub.display().to_string(),
                source: e,
            }
        })?;
    }

    let data_root = temp.path().join("data");
    let mut cfg = SnapBoxConfig::default();
    cfg.storage.data_root = data_root.clone();
    cfg.storage.base_source_dir = Some(source_dir.clone());
    cfg.storage.allow_non_cow_fallback = args.allow_non_cow_fallback;
    cfg.pool.target_warm = args.pool_size;
    cfg.pool.min_warm = args.pool_size.min(2);
    cfg.pool.max_sandboxes = (args.pool_size + args.concurrency).max(args.pool_size * 2);

    let backend = select_backend(&cfg.storage.data_root, cfg.storage.allow_non_cow_fallback)?;
    let metrics = MetricsRegistry::new();
    let pool = WarmPoolManager::new(cfg.clone(), Arc::clone(&backend), metrics)?;
    pool.warm_initial_pool().await?;

    // 1. Measure sequential warm allocation + file mutation + release/rollback
    let mut alloc_durs = Vec::with_capacity(args.iterations);
    let mut release_durs = Vec::with_capacity(args.iterations);
    let seq_wall_start = Instant::now();
    for i in 0..args.iterations {
        let t_alloc = Instant::now();
        let sb = pool.allocate(Some(format!("bench-{i}")), Some(60)).await?;
        alloc_durs.push(t_alloc.elapsed());

        // Simulate realistic dirty write in workspace
        let dirty_file = sb.paths.workspace.join("dirty_mutation.txt");
        fs::write(&dirty_file, b"agent edit").map_err(|e| SnapBoxError::Filesystem {
            operation: "write".into(),
            path: dirty_file.display().to_string(),
            source: e,
        })?;

        let t_rel = Instant::now();
        pool.release(sb.id).await?;
        release_durs.push(t_rel.elapsed());
    }
    let seq_wall = seq_wall_start.elapsed();

    // 2. Measure standalone CoW clone latency
    let mut clone_durs = Vec::with_capacity(args.iterations);
    let clone_wall_start = Instant::now();
    let clone_dir = temp.path().join("clone-bench");
    fs::create_dir_all(&clone_dir).map_err(|e| SnapBoxError::Filesystem {
        operation: "create_dir_all".into(),
        path: clone_dir.display().to_string(),
        source: e,
    })?;
    for _ in 0..args.iterations {
        let id = crate::sandbox::SandboxId::new();
        let t = Instant::now();
        let mut ws = backend.create_workspace(id, pool.base_image(), &clone_dir)?;
        clone_durs.push(t.elapsed());
        backend.destroy_workspace(id, &mut ws)?;
    }
    let clone_wall = clone_wall_start.elapsed();

    // 3. Measure concurrent warm allocation burst
    let conc_wall_start = Instant::now();
    let mut conc_handles = Vec::with_capacity(args.concurrency);
    for w in 0..args.concurrency {
        let p = Arc::clone(&pool);
        conc_handles.push(tokio::spawn(async move {
            let t = Instant::now();
            let sb = p.allocate(Some(format!("conc-{w}")), Some(60)).await?;
            let d = t.elapsed();
            p.release(sb.id).await?;
            Ok::<Duration, SnapBoxError>(d)
        }));
    }
    let mut conc_durs = Vec::with_capacity(args.concurrency);
    let mut conc_failures = 0u64;
    for h in conc_handles {
        match h.await {
            Ok(Ok(d)) => conc_durs.push(d),
            _ => conc_failures += 1,
        }
    }
    let conc_wall = conc_wall_start.elapsed();

    // 4. Measure cold full-copy baseline (PortableCopyBackend)
    let baseline_iters = args.iterations.clamp(1, 10);
    let cold_backend = PortableCopyBackend::new();
    let cold_dir = temp.path().join("cold-baseline");
    fs::create_dir_all(&cold_dir).map_err(|e| SnapBoxError::Filesystem {
        operation: "create_dir_all".into(),
        path: cold_dir.display().to_string(),
        source: e,
    })?;
    let mut cold_durs = Vec::with_capacity(baseline_iters);
    let cold_wall_start = Instant::now();
    for _ in 0..baseline_iters {
        let id = crate::sandbox::SandboxId::new();
        let t = Instant::now();
        let mut ws = cold_backend.create_workspace(id, pool.base_image(), &cold_dir)?;
        cold_durs.push(t.elapsed());
        cold_backend.destroy_workspace(id, &mut ws)?;
    }
    let cold_wall = cold_wall_start.elapsed();

    pool.shutdown_all().await;

    let report = serde_json::json!({
        "backend": backend.capabilities(),
        "dataset": {
            "file_count": args.file_count,
            "file_size_kb": args.file_size_kb,
            "total_base_bytes": pool.base_image().total_bytes,
            "pool_size": args.pool_size,
            "iterations": args.iterations,
            "concurrency": args.concurrency,
        },
        "warm_allocation": summarize_durations(&alloc_durs, 0, seq_wall),
        "release_and_rollback": summarize_durations(&release_durs, 0, seq_wall),
        "cow_clone": summarize_durations(&clone_durs, 0, clone_wall),
        "concurrent_burst_allocation": summarize_durations(&conc_durs, conc_failures, conc_wall),
        "cold_full_copy_baseline": summarize_durations(&cold_durs, 0, cold_wall),
    });

    if as_json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).unwrap_or_default()
        );
    } else {
        let warm_s = summarize_durations(&alloc_durs, 0, seq_wall);
        let rel_s = summarize_durations(&release_durs, 0, seq_wall);
        let clone_s = summarize_durations(&clone_durs, 0, clone_wall);
        let conc_s = summarize_durations(&conc_durs, conc_failures, conc_wall);
        let cold_s = summarize_durations(&cold_durs, 0, cold_wall);

        println!("snap-box Benchmark Report");
        println!(
            "Backend: {} (true_cow={}) | Base: {} files ({:.2} MiB) | Pool: {}",
            backend.capabilities().backend_name,
            backend.capabilities().true_kernel_cow,
            pool.base_image().file_count,
            pool.base_image().total_bytes as f64 / (1024.0 * 1024.0),
            args.pool_size
        );
        println!(
            "{:<28} {:>8} {:>8} {:>8} {:>8} {:>8} {:>10}",
            "OPERATION", "MIN(ms)", "AVG(ms)", "P50(ms)", "P95(ms)", "P99(ms)", "OPS/SEC"
        );
        print_stat_row("warm_allocation", &warm_s);
        print_stat_row("release_and_rollback", &rel_s);
        print_stat_row("cow_clone", &clone_s);
        print_stat_row("concurrent_burst_alloc", &conc_s);
        print_stat_row("cold_full_copy_baseline", &cold_s);
    }

    Ok(())
}

fn print_stat_row(name: &str, s: &LatencyStats) {
    println!(
        "{:<28} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>8.3} {:>10.1}",
        name, s.min_ms, s.avg_ms, s.median_ms, s.p95_ms, s.p99_ms, s.throughput_ops_sec
    );
}

fn print_pool_status(pool: &PoolStatus) {
    println!("snap-box Pool Status");
    println!(
        "  Backend:        {} (true_cow={})",
        pool.backend.backend_name, pool.backend.true_kernel_cow
    );
    println!("  Isolation:      {:?}", pool.backend.isolation_level);
    println!(
        "  Base Image:     {} ({} files, {} bytes)",
        pool.base_id, pool.base_file_count, pool.base_bytes
    );
    println!(
        "  Sandboxes:      total={} warm={} allocated={} running={} resetting={} failed={}",
        pool.total_sandboxes,
        pool.warm_count,
        pool.allocated_count,
        pool.running_count,
        pool.resetting_count,
        pool.failed_count
    );
    println!(
        "  Pool Bounds:    min_warm={} target_warm={} max_sandboxes={}",
        pool.min_warm, pool.target_warm, pool.max_sandboxes
    );
    if let (Some(avail), Some(total)) = (
        pool.host_telemetry.available_memory_bytes,
        pool.host_telemetry.total_memory_bytes,
    ) {
        println!(
            "  Host Memory:    {:.2} GiB available / {:.2} GiB total",
            avail as f64 / (1024.0 * 1024.0 * 1024.0),
            total as f64 / (1024.0 * 1024.0 * 1024.0)
        );
    }
}

fn print_or_json<T: serde::Serialize>(val: &T, as_json: bool, f: impl FnOnce(&T)) {
    if as_json {
        println!("{}", serde_json::to_string_pretty(val).unwrap_or_default());
    } else {
        f(val);
    }
}

fn build_http_client(endpoint: &str) -> Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder();
    let lower = endpoint.to_ascii_lowercase();
    if lower.contains("://127.0.0.1")
        || lower.contains("://localhost")
        || lower.contains("://[::1]")
    {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|e| SnapBoxError::Daemon(format!("failed to build HTTP client: {e}")))
}

async fn send_get<R: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Result<R> {
    let resp = client
        .get(url)
        .send()
        .await
        .map_err(|e| SnapBoxError::Daemon(format!("GET {url} failed: {e}")))?;
    decode_response(resp).await
}

async fn send_post<B: serde::Serialize, R: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    body: Option<&B>,
) -> Result<R> {
    let req = match body {
        Some(b) => client.post(url).json(b),
        None => client.post(url),
    };
    let resp = req
        .send()
        .await
        .map_err(|e| SnapBoxError::Daemon(format!("POST {url} failed: {e}")))?;
    decode_response(resp).await
}

async fn decode_response<R: serde::de::DeserializeOwned>(resp: reqwest::Response) -> Result<R> {
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| SnapBoxError::Daemon(format!("failed reading response body: {e}")))?;
    if !status.is_success() {
        return Err(SnapBoxError::Daemon(format!(
            "server returned HTTP {status}: {text}"
        )));
    }
    serde_json::from_str(&text)
        .map_err(|e| SnapBoxError::Daemon(format!("invalid JSON response: {e}: {text}")))
}
