//! LSP server for detecting and resolving merge conflict markers in any file type.
//!
//! Communicates over stdio using the LSP protocol. Runtime messages are sent to
//! the editor via `window/logMessage`. Use `--log <path>` for detailed trace
//! output to a file (for debugging the server itself).

mod parser;
mod server;
mod state;
#[cfg(test)]
mod test_helpers;

use std::env;

use anyhow::Context;
use clap::Parser;
use lsp_server::Connection;
use server::{main_loop, server_capabilities};

#[derive(clap::Parser, Debug)]
#[command(version = env!("FULL_VERSION"), about, long_about = None)]
struct ArgumentParser {
    /// Include more debugging information.
    #[arg(short, long)]
    debug: bool,

    /// Write detailed trace output to a file (for debugging the server itself).
    #[arg(long)]
    log: Option<std::path::PathBuf>,
}

fn main() -> anyhow::Result<()> {
    let args = ArgumentParser::parse();

    let level = if args.debug {
        tracing::Level::DEBUG
    } else {
        tracing::Level::INFO
    };

    // Only set up a tracing subscriber when --log is passed. Without it, tracing
    // macros are no-ops and runtime messages reach the editor via window/logMessage.
    if let Some(raw_log_path) = &args.log {
        let log_path = expand_tilde(raw_log_path);
        let pid = std::process::id();
        let stem = log_path.file_stem().unwrap_or_default().to_string_lossy();
        let unique_name = match log_path.extension() {
            Some(ext) => format!("{stem}-{pid}.{}", ext.to_string_lossy()),
            None => format!("{stem}-{pid}"),
        };
        let unique_path = log_path.with_file_name(unique_name);
        let file = std::fs::File::create(&unique_path)
            .with_context(|| format!("failed to create log file '{}'", unique_path.display()))?;
        eprintln!("logging to {}", unique_path.display());
        tracing_subscriber::fmt::fmt()
            .with_max_level(level)
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
    }

    run_server()
}

/// Expand a leading `~` or `~/` to the user's home directory.
/// Paths without a leading tilde are returned unchanged.
fn expand_tilde(path: &std::path::Path) -> std::path::PathBuf {
    if let Ok(rest) = path.strip_prefix("~")
        && let Some(home) = env::var_os("HOME")
    {
        return std::path::PathBuf::from(home).join(rest);
    }
    path.to_path_buf()
}

fn run_server() -> anyhow::Result<()> {
    tracing::info!("server initializing");

    let (connection, io_threads) = Connection::stdio();
    let (initialize_id, initialize_params) = match connection.initialize_start() {
        Ok(it) => it,
        Err(e) => {
            if e.channel_is_disconnected() {
                io_threads.join()?;
            }
            tracing::error!("Failed to initialize!: {e:?}");
            return Err(e.into());
        }
    };
    let lsp_types::InitializeParams {
        initialization_options,
        ..
    } = serde_json::from_value(initialize_params)?;

    tracing::info!("initialization options: {:?}", initialization_options);
    let capabilities = server_capabilities();
    let server_info = Some(lsp_types::ServerInfo {
        name: env!("CARGO_PKG_NAME").to_string(),
        version: Some(env!("FULL_VERSION").to_string()),
    });
    let initialize_result = lsp_types::InitializeResult {
        capabilities,
        server_info,
    };
    let initialize_result = serde_json::to_value(initialize_result).unwrap();
    if let Err(e) = connection.initialize_finish(initialize_id, initialize_result) {
        if e.channel_is_disconnected() {
            io_threads.join()?;
        }
        return Err(e.into());
    }

    match (main_loop(connection), io_threads.join()) {
        (Err(loop_err), Err(join_err)) => anyhow::bail!("{loop_err}\n{join_err}"),
        (Ok(_), Err(join_err)) => anyhow::bail!("{join_err}"),
        (Err(loop_err), Ok(_)) => anyhow::bail!("{loop_err}"),
        (Ok(_), Ok(_)) => {}
    }

    tracing::info!("server shut down");
    Ok(())
}
