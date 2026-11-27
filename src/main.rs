use clap::Parser;
use tracing_subscriber::{fmt, EnvFilter};

use snap_box::cli::{run_cli, Cli, Commands};

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let is_json_logs = matches!(cli.command, Commands::Start(_)) && cli.json;
    init_tracing(is_json_logs);

    if let Err(err) = run_cli(cli).await {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn init_tracing(json: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    if json {
        let _ = fmt().with_env_filter(filter).json().try_init();
    } else {
        let _ = fmt().with_env_filter(filter).try_init();
    }
}
