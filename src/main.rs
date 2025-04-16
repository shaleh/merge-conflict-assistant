use std::error::Error;

use log::{debug, info, warn};
use lsp_types::notification::PublishDiagnostics;
use lsp_types::{
    CodeActionKind, CodeActionOptions, CodeActionProviderCapability, Diagnostic,
    DiagnosticSeverity, DidChangeTextDocumentParams, DidCloseTextDocumentParams,
    DidOpenTextDocumentParams, Position, PublishDiagnosticsParams, Range, TextDocumentItem,
    TextDocumentSyncCapability, TextDocumentSyncKind,
};
use lsp_types::{InitializeParams, ServerCapabilities};

use lsp_server::{Connection, Message, Notification, Request};

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

// Region in a conflict.
//
// Defined by a start and end.
// Name is the branch or other identifier associated with the conflict marker.
//
// The values are optional to allow partial building by the parser. In reality,
// only the name is truly optional.
//
#[derive(Debug, Clone)]
struct ConflictRegion {
    start: Option<u32>,
    end: Option<u32>,
    name: Option<String>,
}

// Merge conflict information.
//
// A conflict has an ours and a theirs and in the case of diff3 also an ancestor.
//
#[derive(Debug, Clone)]
struct Conflict {
    ours: ConflictRegion,
    theirs: ConflictRegion,
    ancestor: Option<ConflictRegion>,
}

impl Conflict {
    fn start(&self) -> u32 {
        self.ours.start.unwrap()
    }

    fn end(&self) -> u32 {
        self.theirs.end.unwrap() + 1
    }

    fn is_in_range(&self, range: Range) -> bool {
        self.start() <= range.start.line && self.end() >= range.end.line
    }
}

impl From<&Conflict> for Range {
    fn from(conflict: &Conflict) -> Self {
        Self {
            start: Position {
                line: conflict.start(),
                character: 0,
            },
            end: Position {
                line: conflict.end(),
                character: 0,
            },
        }
    }
}

impl From<&Conflict> for Diagnostic {
    fn from(conflict: &Conflict) -> Self {
        let range = Range::from(conflict);
        let message = "merge conflict";
        let source = "merge";
        Self {
            range,
            message: message.to_owned(),
            source: Some(source.to_owned()),
            severity: Some(DiagnosticSeverity::ERROR),
            ..Default::default()
        }
    }
}

#[derive(Debug, Default)]
struct Parser {
    conflicts: Vec<Conflict>,
    ours: Option<ConflictRegion>,
    theirs: Option<ConflictRegion>,
    ancestor: Option<ConflictRegion>,
}

impl Parser {
    fn on_new_conflict(&mut self, number: u32, name: &str) -> Result<(), String> {
        if self.ours.is_some() {
            self.ours = None;
            return Err("found an unterminated conflict marker".to_owned());
        }
        self.ours.replace(ConflictRegion {
            start: Some(number),
            end: None,
            name: Some(name.to_owned()),
        });
        debug!("start ours {}: {:?}", number, self.ours);
        Ok(())
    }

    fn on_leave_ours(&mut self, number: u32) -> Result<(), String> {
        if let Some(ours_) = self.ours.as_mut() {
            if ours_.end.is_none() {
                ours_.end.replace(number);
            }
        } else {
            return Err("unexpected end of OURS region".to_owned());
        }
        Ok(())
    }

    fn on_enter_ancestor(&mut self, number: u32, name: &str) -> Result<(), String> {
        if let Some(ours_) = self.ours.as_mut() {
            ours_.end.replace(number);
        } else {
            return Err("Found ancestor marker, but no active conflict".to_owned());
        }
        self.ancestor.replace(ConflictRegion {
            start: Some(number),
            end: None,
            name: Some(name.to_owned()),
        });
        debug!("start ancestor {}: {:?}", number, self.ancestor);
        Ok(())
    }

    fn on_leave_ancestor(&mut self, number: u32) -> Result<(), String> {
        if let Some(ancestor_) = self.ancestor.as_mut() {
            if ancestor_.end.is_none() {
                ancestor_.end.replace(number);
            }
        }

        Ok(())
    }

    fn on_enter_theirs(&mut self, number: u32) -> Result<(), String> {
        self.on_leave_ours(number)?;
        self.on_leave_ancestor(number)?;
        if self.theirs.is_some() {
            return Err("found THEIRS marker, expected conflict end marker".to_owned());
        }
        self.theirs.replace(ConflictRegion {
            start: Some(number),
            end: None,
            name: None,
        });
        debug!("start theirs {}", number);
        Ok(())
    }

    fn on_leave_theirs(&mut self, number: u32, name: &str) -> Result<(), String> {
        if let Some(theirs_) = self.theirs.as_mut() {
            theirs_.end.replace(number);
            theirs_.name.replace(name.to_owned());
        } else {
            return Err("unexpected end of conflict marker".to_owned());
        }
        debug!("end theirs {}: {:?}", number, self.theirs);
        if let (Some(ours_), Some(theirs_)) = (self.ours.as_ref(), self.theirs.as_ref()) {
            self.conflicts.push(Conflict {
                ours: ours_.clone(),
                theirs: theirs_.clone(),
                ancestor: self.ancestor.clone(),
            });
        }
        self.ours = None;
        self.theirs = None;
        if self.ancestor.is_some() {
            self.ancestor = None;
        }
        Ok(())
    }

    fn parse(&mut self, document: &TextDocumentItem) -> Vec<Conflict> {
        debug!("parsing: {:?}", document.uri);

        for (number, line) in document.text.lines().enumerate() {
            let result = if let Some(rest) = line.strip_prefix("<<<<<<<") {
                self.on_new_conflict(number.try_into().unwrap(), rest.trim())
            } else if let Some(rest) = line.strip_prefix("|||||||") {
                self.on_enter_ancestor(number.try_into().unwrap(), rest.trim())
            } else if line.starts_with("=======") {
                self.on_enter_theirs(number.try_into().unwrap())
            } else if let Some(rest) = line.strip_prefix(">>>>>>>") {
                self.on_leave_theirs(number.try_into().unwrap(), rest.trim())
            } else {
                Ok(())
            };
            if let Err(message) = result {
                warn!("{}: {}", message, number);
            }
        }
        self.conflicts.clone()
    }
}

fn on_did_open(notification: Notification) -> LSPResult {
    debug!("did open intro");
    let params: DidOpenTextDocumentParams = serde_json::from_value(notification.params)?;
    debug!(
        "did open: {:?}: {:?}",
        params.text_document.uri, params.text_document.text
    );
    let mut parser = Parser::default();
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
