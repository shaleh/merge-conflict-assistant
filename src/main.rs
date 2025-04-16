use std::error::Error;

use log::{debug, info};
use lsp_types::notification::PublishDiagnostics;
use lsp_types::{
    CodeActionKind, CodeActionOptions, CodeActionProviderCapability, Diagnostic,
    DidChangeTextDocumentParams, DidCloseTextDocumentParams, DidOpenTextDocumentParams,
    PublishDiagnosticsParams, TextDocumentSyncCapability, TextDocumentSyncKind,
};
use lsp_types::{InitializeParams, ServerCapabilities};

use lsp_server::{Connection, Message, Notification, Request};

mod parser;

type LSPResult = Result<Option<Notification>, Box<dyn Error + Sync + Send>>;

fn server_capabilities() -> ServerCapabilities {
    ServerCapabilities {
        text_document_sync: Some(TextDocumentSyncCapability::Kind(TextDocumentSyncKind::FULL)),
        code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions {
            code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]),
            ..Default::default()
        })),
        ..Default::default()
    }
}

fn main() -> Result<(), Box<dyn Error + Send + Sync>> {
    // Note that we must have our logging only write out to stderr.
    stderrlog::new()
        .module(module_path!())
        .verbosity(log::Level::Debug)
        .init()?;
    info!("starting merge-assistant LSP server");

    let (connection, io_threads) = Connection::stdio();

    let server_capabilities_json = serde_json::to_value(server_capabilities())?;
    let initialization_params = match connection.initialize(server_capabilities_json) {
        Ok(it) => it,
        Err(e) => {
            if e.channel_is_disconnected() {
                io_threads.join()?;
            }
            return Err(e.into());
        }
    };
    main_loop(connection, initialization_params)?;
    io_threads.join()?;

    info!("shutting down server");
    Ok(())
}

fn main_loop(connection: Connection, params: serde_json::Value) -> LSPResult {
    let _params: InitializeParams = serde_json::from_value(params).unwrap();
    for msg in &connection.receiver {
        debug!("got msg: {msg:?}");
        match msg {
            Message::Notification(notification) => {
                if let Some(reply) = handle_notification(notification)? {
                    _ = connection.sender.send(reply.into());
                }
            }
            Message::Request(req) => {
                if connection.handle_shutdown(&req)? {
                    return Ok(None);
                }
                handle_request(req)?;
            }
            Message::Response(resp) => {
                debug!("got response: {resp:?}");
            }
        }
    }
    Ok(None)
}

fn on_did_open(notification: Notification) -> LSPResult {
    debug!("did open intro");
    let params: DidOpenTextDocumentParams = serde_json::from_value(notification.params)?;
    debug!(
        "did open: {:?}: {:?}",
        params.text_document.uri, params.text_document.text
    );
    let mut parser = parser::Parser::default();
    let conflicts = parser.parse(&params.text_document);
    debug!("Conflicts: {conflicts:?}");
    let diagnostics: Vec<Diagnostic> = conflicts.iter().map(Diagnostic::from).collect();
    let publish_diagnostics_params = PublishDiagnosticsParams {
        uri: params.text_document.uri,
        diagnostics,
        version: Some(params.text_document.version),
    };
    let notification = lsp_server::Notification::new(
        <PublishDiagnostics as lsp_types::notification::Notification>::METHOD.to_owned(),
        publish_diagnostics_params,
    );

    Ok(Some(notification))
}

fn on_did_change(notification: Notification) -> LSPResult {
    let params: DidChangeTextDocumentParams = serde_json::from_value(notification.params)?;
    debug!(
        "did change: {:?}: {:?}",
        params.text_document.uri, params.content_changes
    );
    Ok(None)
}

fn on_did_close(notification: Notification) -> LSPResult {
    let params: DidCloseTextDocumentParams = serde_json::from_value(notification.params)?;
    debug!("did close: {:?}", params.text_document.uri);
    Ok(None)
}

fn handle_notification(notification: Notification) -> LSPResult {
    debug!("heard notification {notification:?}");
    match notification.method.as_ref() {
        "textDocument/didOpen" => on_did_open(notification),
        "textDocument/didClose" => on_did_close(notification),
        "textDocument/didChange" => on_did_change(notification),
        unknown => {
            debug!("notification: unknown: {unknown:?}");
            Ok(None)
        }
    }
}

fn handle_request(req: Request) -> LSPResult {
    debug!("got request: {req:?}");

    Ok(None)
}
