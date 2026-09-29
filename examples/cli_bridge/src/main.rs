// SPDX-FileCopyrightText: Copyright (c) 2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Binary entrypoint for `switchyard-cli-bridge`.

use std::io;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use switchyard_cli_bridge::cli::Programs;
use switchyard_cli_bridge::{Config, router};
use tracing_subscriber::EnvFilter;

/// Serve the Claude Code, Codex, and Grok CLIs as a local OpenAI Chat Completions endpoint.
///
/// Use the model id `claude`, `codex`, or `grok`, optionally followed by `/<model>`,
/// for example `claude/opus` or `codex/gpt-5.5`.
#[derive(Debug, Parser)]
#[command(name = "switchyard-cli-bridge", version)]
struct Args {
    /// Address to listen on. Keep it on localhost: requests run with your CLI logins.
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// Port to listen on.
    #[arg(long, default_value_t = 4100)]
    port: u16,
    /// Directory the CLIs run in. Defaults to a new empty temporary directory.
    #[arg(long)]
    workdir: Option<PathBuf>,
    /// Seconds one CLI call may run before it is stopped.
    #[arg(long, default_value_t = 600)]
    timeout_secs: u64,
    /// Claude Code executable.
    #[arg(long, default_value = "claude")]
    claude_bin: PathBuf,
    /// Codex CLI executable.
    #[arg(long, default_value = "codex")]
    codex_bin: PathBuf,
    /// Grok CLI executable.
    #[arg(long, default_value = "grok")]
    grok_bin: PathBuf,
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_writer(io::stderr)
        .init();

    match run(Args::parse()).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("switchyard-cli-bridge: {error}");
            ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> io::Result<()> {
    // Kept alive until the server stops, then deleted.
    let temp_dir;
    let workdir = match args.workdir {
        Some(workdir) => workdir,
        None => {
            temp_dir = tempfile::Builder::new()
                .prefix("switchyard-cli-bridge-")
                .tempdir()?;
            temp_dir.path().to_path_buf()
        }
    };

    let config = Config {
        programs: Programs {
            claude: args.claude_bin,
            codex: args.codex_bin,
            grok: args.grok_bin,
        },
        workdir,
        timeout: Duration::from_secs(args.timeout_secs),
    };

    let listener = tokio::net::TcpListener::bind((args.host.as_str(), args.port)).await?;
    tracing::info!(
        address = %listener.local_addr()?,
        workdir = %config.workdir.display(),
        "switchyard-cli-bridge listening"
    );
    axum::serve(listener, router(config))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}
