mod parser;
mod server;

use lsp_server::Connection;
use server::MergeAssistant;

fn main() -> anyhow::Result<()> {
    // Note that we must have our logging only write out to stderr.
    stderrlog::new()
        .module(module_path!())
        .verbosity(log::Level::Debug)
        .init()?;

    run_server()
}

fn run_server() -> anyhow::Result<()> {
    log::info!("server will start");

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
    // TODO: use more of these params.
    let lsp_types::InitializeParams {
        initialization_options,
        ..
    } = serde_json::from_value(initialize_params)?;

    log::info!("initialization options: {:?}", initialization_options);
    let capabilities = MergeAssistant::server_capabilities();
    let server_info = Some(lsp_types::ServerInfo {
        name: String::from("merge-assistant"),
        version: Some("0.1.0".to_string()),
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

    match (MergeAssistant::main_loop(connection), io_threads.join()) {
        (Err(loop_err), Err(join_err)) => anyhow::bail!("{loop_err}\n{join_err}"),
        (Ok(_), Err(join_err)) => anyhow::bail!("{join_err}"),
        (Err(loop_err), Ok(_)) => anyhow::bail!("{loop_err}"),
        (Ok(_), Ok(_)) => {}
    }

    log::info!("server shut down");
    Ok(())
}
