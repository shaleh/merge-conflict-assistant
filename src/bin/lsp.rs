use std::env;

use clap::Parser;
use common::server::{main_loop, server_capabilities};
use lsp_server::Connection;

#[derive(clap::Parser, Debug)]
#[command(version = env!("FULL_VERSION"), about, long_about = None)]
struct ArgumentParser {
    /// Include more debugging infomration.
    #[arg(short, long)]
    debug: bool,

    /// Write log output to a file instead of stderr.
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

    if let Some(log_path) = &args.log {
        let file = std::fs::File::create(log_path)?;
        tracing_subscriber::fmt::fmt()
            .with_max_level(level)
            .with_writer(std::sync::Mutex::new(file))
            .with_ansi(false)
            .init();
    } else {
        // Note that we must have our logging only write out to stderr. stdout is assumed to be protocol data.
        tracing_subscriber::fmt::fmt()
            .with_max_level(level)
            .with_writer(std::io::stderr)
            .without_time()
            .with_ansi(false)
            .init();
    }

    run_server()
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
