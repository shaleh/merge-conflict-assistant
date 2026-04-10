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
}

fn main() -> anyhow::Result<()> {
    let mut debug = false;

    let args = ArgumentParser::parse();
    if args.debug {
        debug = true;
    }

    tracing_subscriber::fmt::fmt()
        .with_max_level(if debug {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        // Note that we must have our logging only write out to stderr. stdout is assumed to be protocol data.
        .with_writer(std::io::stderr)
        .without_time()
        .with_ansi(false)
        .init();

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
