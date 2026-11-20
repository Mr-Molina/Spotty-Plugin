//! Unified `lms-spotty` daemon, scanner, and audio helper binary.

use std::env;
use std::process::ExitCode;

use lms_spotty::cli::{contains_legacy_action_flag, run_cli};
use lms_spotty::rpc_protocol::SPOTTY_VERSION;
use spotty_audio::{format_helper_check_output, HelperCapabilities};

#[tokio::main]
async fn main() -> ExitCode {
    let args: Vec<String> = env::args().collect();

    // Fast-path `--check` before tracing initialization to keep stdout pure.
    if contains_legacy_action_flag(&args) && args.iter().any(|arg| arg == "--check") {
        println!(
            "{}",
            format_helper_check_output(SPOTTY_VERSION, &HelperCapabilities::new())
        );
        return ExitCode::SUCCESS;
    }

    init_tracing();

    if let Err(err) = run_cli(args).await {
        eprintln!("lms-spotty error: {err}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}
