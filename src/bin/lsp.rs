mod parser;
mod server;

use std::env;

use lsp_server::Connection;
use server::MergeConflictAssistant;

fn main() -> anyhow::Result<()> {
    let mut debug = false;

    let args: Vec<String> = env::args().collect();
    let s_args: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    match &s_args[1..] {
        [] => { /* do nothing */ }
        ["--debug"] => {
            debug = true;
        }
        ["--version"] => {
            println!("{}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        _ => {
            println!("{}", env!("CARGO_PKG_NAME"));
            println!(" --debug   Enable debugging");
            println!(" --version Print version and exit");
            std::process::exit(0);
        }
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
    log::info!("server initializing");

    let (connection, io_threads) = Connection::stdio();
    let (initialize_id, initialize_params) = match connection.initialize_start() {
        Ok(it) => it,
        Err(e) => {
            if e.channel_is_disconnected() {
                io_threads.join()?;
            }
            return Err(e.into());
        }
    };
    let lsp_types::InitializeParams {
        initialization_options,
        ..
    } = serde_json::from_value(initialize_params)?;

    log::info!("initialization options: {:?}", initialization_options);
    let capabilities = MergeConflictAssistant::server_capabilities();
    let server_info = Some(lsp_types::ServerInfo {
        name: env!("CARGO_PKG_NAME").to_string(),
        version: Some(env!("CARGO_PKG_VERSION").to_string()),
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

    match (
        MergeConflictAssistant::main_loop(connection),
        io_threads.join(),
    ) {
        (Err(loop_err), Err(join_err)) => anyhow::bail!("{loop_err}\n{join_err}"),
        (Ok(_), Err(join_err)) => anyhow::bail!("{join_err}"),
        (Err(loop_err), Ok(_)) => anyhow::bail!("{loop_err}"),
        (Ok(_), Ok(_)) => {}
    }

    log::info!("server shut down");
    Ok(())
}
