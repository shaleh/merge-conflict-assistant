//! LSP protocol handler.
//!
//! Manages open documents and their cached conflict state behind `Arc<Mutex<>>`.
//! Document updates are processed on spawned threads to keep the main message
//! loop responsive. Publishes diagnostics and generates quickfix code actions.

use std::{
    sync::{Arc, Mutex},
    thread,
};

use crate::{
    parser::MergeConflict,
    state::{ServerState, ServerStatus},
};

pub type LSPResult = anyhow::Result<Option<(lsp_types::Uri, i32)>>;

pub fn main_loop(connection: lsp_server::Connection) -> LSPResult {
    let mut state = ServerState::new(connection.sender);
    let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();

    send_log_message(
        state.sender.clone(),
        lsp_types::MessageType::INFO,
        format!("{} {} ready", env!("CARGO_PKG_NAME"), env!("FULL_VERSION")),
    );

    for msg in &connection.receiver {
        // Clean up finished handles periodically.
        handles.retain(|h| !h.is_finished());
        handle_message(&mut handles, &mut state, msg)?;
        if state.status == ServerStatus::ExitReceived {
            break;
        }
    }

    for handle in handles {
        let _ = handle.join();
    }

    tracing::debug!("shutting down server");
    Ok(None)
}

fn handle_message(
    handles: &mut Vec<thread::JoinHandle<()>>,
    state: &mut ServerState,
    message: lsp_server::Message,
) -> LSPResult {
    tracing::debug!("got msg: {message:?}");
    match message {
        lsp_server::Message::Notification(notification) => {
            if let Some((uri, version)) = on_notification_message(state, notification)? {
                let state = (*state).clone();
                let handle = thread::spawn(move || document_update_thread(uri, version, state));
                handles.push(handle);
            }
        }
        lsp_server::Message::Request(request) => {
            if let Some(message) = on_request(state, request)? {
                let sender = state.sender.lock().expect("lock on sender");
                if let Err(e) = sender.send(message.into()) {
                    tracing::error!("Failed to send message: {e}");
                }
            }
        }
        lsp_server::Message::Response(response) => {
            tracing::debug!("got response: {response:?}");
        }
    }
    Ok(None)
}

fn on_notification_message(
    state: &mut ServerState,
    notification: lsp_server::Notification,
) -> LSPResult {
    tracing::debug!("heard notification {notification:?}");
    match notification.method.as_ref() {
        "exit" => {
            tracing::debug!("exit notification received");
            state.status = ServerStatus::ExitReceived;
            Ok(None)
        }
        "textDocument/didOpen" => on_did_open_text_document(state, notification),
        "textDocument/didClose" => on_did_close_text_document(state, notification),
        "textDocument/didChange" => on_did_change_text_document(state, notification),
        unhandled => {
            tracing::debug!("notification: ignored: {unhandled:?}");
            Ok(None)
        }
    }
}

fn on_did_open_text_document(
    state: &mut ServerState,
    notification: lsp_server::Notification,
) -> LSPResult {
    let lsp_types::DidOpenTextDocumentParams { text_document, .. } =
        serde_json::from_value(notification.params)?;
    tracing::info!("did open: {:?}", text_document.uri);
    send_log_message(
        state.sender.clone(),
        lsp_types::MessageType::INFO,
        format!("opened: {}", text_document.uri.as_str()),
    );
    state.add_document(text_document)
}

fn on_did_close_text_document(
    state: &mut ServerState,
    notification: lsp_server::Notification,
) -> LSPResult {
    let lsp_types::DidCloseTextDocumentParams { text_document, .. } =
        serde_json::from_value(notification.params)?;
    tracing::info!("did close: {:?}", text_document.uri);
    state.remove_document(text_document)
}

fn on_did_change_text_document(
    state: &mut ServerState,
    notification: lsp_server::Notification,
) -> LSPResult {
    let lsp_types::DidChangeTextDocumentParams {
        text_document,
        content_changes,
        ..
    } = serde_json::from_value(notification.params)?;
    tracing::info!(
        "did change: {:?}: version {}",
        text_document.uri,
        text_document.version
    );
    state.document_did_change(text_document, content_changes)
}

fn on_request(
    state: &mut ServerState,
    request: lsp_server::Request,
) -> anyhow::Result<Option<lsp_server::Response>> {
    tracing::debug!("got request: {request:?}");

    if state.status != ServerStatus::Running {
        return Ok(Some(lsp_server::Response::new_err(
            request.id,
            lsp_server::ErrorCode::InvalidRequest as i32,
            "Server is shutting down.".to_owned(),
        )));
    }

    match request.method.as_ref() {
        "textDocument/codeAction" => on_code_action_request(state, request),
        "shutdown" => on_shutdown(state, request),
        unhandled => {
            tracing::debug!("request: ignored: {unhandled:?}");
            Ok(Some(lsp_server::Response::new_err(
                request.id,
                lsp_server::ErrorCode::MethodNotFound as i32,
                format!("method not found: {unhandled}"),
            )))
        }
    }
}

fn on_code_action_request(
    state: &mut ServerState,
    request: lsp_server::Request,
) -> anyhow::Result<Option<lsp_server::Response>> {
    tracing::debug!("code action");
    let (id, params): (lsp_server::RequestId, lsp_types::CodeActionParams) = request
        .extract(<lsp_types::request::CodeActionRequest as lsp_types::request::Request>::METHOD)?;
    let actions = state.code_action(params)?;
    Ok(Some(lsp_server::Response::new_ok(id, actions)))
}

fn on_shutdown(
    state: &mut ServerState,
    request: lsp_server::Request,
) -> anyhow::Result<Option<lsp_server::Response>> {
    tracing::info!("shutdown requested");
    state.status = ServerStatus::ShutdownRequested;
    Ok(Some(lsp_server::Response::new_ok(
        request.id,
        serde_json::Value::Null,
    )))
}

fn document_update_thread(uri: lsp_types::Uri, version: i32, state: ServerState) {
    tracing::debug!(
        "document update worker started for {:?} version {}",
        uri,
        version
    );
    match state.on_document_update(&uri, version) {
        Ok(conflicts) => {
            let count = conflicts.as_ref().map_or(0, |mc| mc.conflicts().count());
            tracing::info!("{:?}: parsed {} conflict(s)", uri, count);
            tracing::debug!("Conflicts: {:?}", conflicts);
            if count > 0 {
                send_log_message(
                    state.sender.clone(),
                    lsp_types::MessageType::INFO,
                    format!("{}: found {count} merge conflict(s)", uri.as_str()),
                );
            }
            let message = prepare_diagnostics(&uri, version, &conflicts);
            let sender = state.sender.lock().expect("lock on sender");
            if let Err(e) = sender.send(message.into()) {
                tracing::error!("Failed to send message: {e}");
            }
        }
        Err(err) => {
            tracing::error!("From on_document_update: {err:?}");
        }
    }
    tracing::debug!("document update worker finished for {:?}", uri);
}

fn prepare_diagnostics(
    uri: &lsp_types::Uri,
    version: i32,
    merge_conflict: &Option<MergeConflict>,
) -> lsp_server::Notification {
    let diagnostics: Vec<lsp_types::Diagnostic> = match merge_conflict {
        Some(current_conflict) => current_conflict
            .conflicts()
            .map(lsp_types::Diagnostic::from)
            .collect(),
        None => Vec::new(),
    };
    tracing::info!(
        "publishing {} diagnostic(s) for {:?} version {}",
        diagnostics.len(),
        uri,
        version
    );
    let publish_diagnostics_params = lsp_types::PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics,
        version: Some(version),
    };
    lsp_server::Notification::new(
                <lsp_types::notification::PublishDiagnostics as lsp_types::notification::Notification>::METHOD.to_owned(),
                publish_diagnostics_params,
            )
}

pub fn server_capabilities() -> lsp_types::ServerCapabilities {
    let text_document_sync = Some(lsp_types::TextDocumentSyncCapability::Options(
        lsp_types::TextDocumentSyncOptions {
            open_close: Some(true),
            change: Some(lsp_types::TextDocumentSyncKind::INCREMENTAL),
            ..Default::default()
        },
    ));
    let code_action_provider = Some(lsp_types::CodeActionProviderCapability::Options(
        lsp_types::CodeActionOptions {
            code_action_kinds: Some(vec![lsp_types::CodeActionKind::QUICKFIX]),
            ..Default::default()
        },
    ));
    lsp_types::ServerCapabilities {
        text_document_sync,
        code_action_provider,
        ..Default::default()
    }
}

pub fn send_log_message(
    sender: Arc<Mutex<crossbeam_channel::Sender<lsp_server::Message>>>,
    typ: lsp_types::MessageType,
    message: impl Into<String>,
) {
    let params = lsp_types::LogMessageParams {
        typ,
        message: message.into(),
    };
    let notification = lsp_server::Notification::new(
        <lsp_types::notification::LogMessage as lsp_types::notification::Notification>::METHOD
            .to_owned(),
        params,
    );
    let locked_sender = sender.lock().expect("lock on sender");
    if let Err(e) = locked_sender.send(notification.into()) {
        tracing::error!("Failed to send logMessage: {e}");
    }
}

#[cfg(test)]
mod test {
    use lsp_types::notification::{DidChangeTextDocument, DidOpenTextDocument};
    use rstest::*;

    use super::*;

    use crate::test_helpers::{
        TEXT1_RESOLVED, TEXT1_WITH_CONFLICTS, TEXT2_RESOLVED, TEXT2_WITH_CONFLICTS,
        conflicts_for_text2_with_conflicts, populated_state, state, uri, version,
    };
    use crate::{parser::parse, state::DocumentState};

    #[fixture]
    fn did_open(version: i32, #[default("")] text: &str) -> lsp_server::Notification {
        let text_document = lsp_types::TextDocumentItem {
            uri: uri().clone(),
            language_id: "".to_string(),
            version,
            text: text.to_string(),
        };
        let params = lsp_types::DidOpenTextDocumentParams { text_document };
        lsp_server::Notification {
            method: <DidOpenTextDocument as lsp_types::notification::Notification>::METHOD
                .to_owned(),
            params: serde_json::to_value(params).unwrap(),
        }
    }

    #[fixture]
    fn did_change_whole_document(
        #[with(1)] version: i32,
        #[default("")] text: &str,
    ) -> lsp_server::Notification {
        let text_document = lsp_types::VersionedTextDocumentIdentifier {
            uri: uri(),
            version,
        };
        let content_changes = vec![lsp_types::TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: text.to_string(),
        }];
        let params = lsp_types::DidChangeTextDocumentParams {
            text_document,
            content_changes,
        };
        lsp_server::Notification {
            method: <DidChangeTextDocument as lsp_types::notification::Notification>::METHOD
                .to_owned(),
            params: serde_json::to_value(params).unwrap(),
        }
    }

    #[fixture]
    fn did_change_incrementally(
        #[default(1)] version: i32,
        #[default(&[])] content_changes: &[(lsp_types::Position, lsp_types::Position, &str)],
    ) -> lsp_server::Notification {
        let text_document = lsp_types::VersionedTextDocumentIdentifier {
            uri: uri(),
            version,
        };
        let mut changes = Vec::with_capacity(content_changes.len());
        for (start, end, text) in content_changes {
            let event = lsp_types::TextDocumentContentChangeEvent {
                range: Some(lsp_types::Range {
                    start: *start,
                    end: *end,
                }),
                range_length: None,
                text: text.to_string(),
            };
            changes.push(event);
        }
        let params = lsp_types::DidChangeTextDocumentParams {
            text_document,
            content_changes: changes,
        };
        lsp_server::Notification {
            method: <DidChangeTextDocument as lsp_types::notification::Notification>::METHOD
                .to_owned(),
            params: serde_json::to_value(params).unwrap(),
        }
    }

    #[rstest]
    fn open_document_with_no_markers_returns_document_data(
        uri: lsp_types::Uri,
        mut state: ServerState,
        #[with(1, TEXT1_RESOLVED)] did_open: lsp_server::Notification,
    ) {
        let result = on_did_open_text_document(&mut state, did_open);
        let (_uri, version) = result.unwrap().unwrap();
        let documents = state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(1, version);
        assert_eq!(1, locked_document_state.version());
        assert_eq!(TEXT1_RESOLVED, locked_document_state.content());
        assert!(locked_document_state.merge_conflict.is_none());
    }

    #[rstest]
    fn open_document_with_markers_returns_document_data(
        uri: lsp_types::Uri,
        mut state: ServerState,
        #[with(5, TEXT1_WITH_CONFLICTS)] did_open: lsp_server::Notification,
    ) {
        let result = on_did_open_text_document(&mut state, did_open);
        let (_uri, version) = result.unwrap().unwrap();
        let documents = state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(5, version);
        assert_eq!(5, locked_document_state.version());
        assert_eq!(TEXT1_WITH_CONFLICTS, locked_document_state.content());
        assert!(locked_document_state.merge_conflict.is_none());
    }

    #[rstest]
    fn change_document_with_no_markers_returns_document_data(
        #[with(2, TEXT2_RESOLVED)] mut populated_state: ServerState,
        #[with(3, TEXT2_RESOLVED)] did_change_whole_document: lsp_server::Notification,
    ) {
        let result = on_did_change_text_document(&mut populated_state, did_change_whole_document);
        let (_uri, version) = result.unwrap().unwrap();
        assert_eq!(3, version);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(3, locked_document_state.version());
        assert_eq!(TEXT2_RESOLVED, locked_document_state.content());
        assert!(locked_document_state.merge_conflict.is_none());
    }

    #[rstest]
    fn change_document_with_no_markers_replaced_with_markers_returns_diagnostics(
        #[with(1, TEXT2_RESOLVED)] mut populated_state: ServerState,
        #[with(2, TEXT2_WITH_CONFLICTS)] did_change_whole_document: lsp_server::Notification,
    ) {
        let result = on_did_change_text_document(&mut populated_state, did_change_whole_document);
        let (_uri, version) = result.unwrap().unwrap();
        assert_eq!(2, version);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(2, locked_document_state.version());
        assert_eq!(TEXT2_WITH_CONFLICTS, locked_document_state.content());
        assert!(locked_document_state.merge_conflict.is_none());
    }

    macro_rules! insert {
        (line: $line:expr, character: $char:expr, $s:expr) => {
            (
                lsp_types::Position {
                    line: $line,
                    character: $char,
                },
                lsp_types::Position {
                    line: $line,
                    character: $char,
                },
                $s,
            )
        };
    }

    #[rstest]
    fn change_document_with_markers_incrementally_changed_outside_of_markers_returns_document_data(
        uri: lsp_types::Uri,
        #[with(1, TEXT2_WITH_CONFLICTS, Some(conflicts_for_text2_with_conflicts()))]
        mut populated_state: ServerState,
        #[with(
                2,
                 &[insert!(line: 0, character: 0, "!"),
                   insert!(line: 0, character: 1, "\n"),
                   insert!(line: 1, character: 0, "# Just a comment."),
                   insert!(line: 1, character: 17, "\n"),
                   insert!(line: 17, character: 0, "@")
                  ])
            ]
        did_change_incrementally: lsp_server::Notification,
    ) {
        {
            let documents = populated_state.documents.lock().unwrap();
            let document_state = documents.get(&uri).unwrap();
            let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
            assert_eq!(
                locked_document_state
                    .merge_conflict
                    .as_ref()
                    .unwrap()
                    .conflicts
                    .len(),
                2
            );
        }
        let (_uri, version) =
            on_did_change_text_document(&mut populated_state, did_change_incrementally)
                .unwrap()
                .unwrap();
        assert_eq!(2, version);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(2, locked_document_state.version());
        assert_eq!(
            format!("!\n# Just a comment.\n{}@", TEXT2_WITH_CONFLICTS),
            locked_document_state.content()
        );
        assert_eq!(
            locked_document_state.merge_conflict,
            Some(conflicts_for_text2_with_conflicts())
        );
    }

    macro_rules! replace {
        (line: $line:expr, character: $char:expr, old: $old_s:expr, new: $new_s:expr) => {
            (
                lsp_types::Position {
                    line: $line,
                    character: $char,
                },
                lsp_types::Position {
                    line: $line,
                    character: $char + (($old_s).len() as u32),
                },
                $new_s,
            )
        };
    }

    #[rstest]
    fn change_document_with_markers_incrementally_changed_using_replace_outside_of_markers_returns_document_data(
        #[with(1, TEXT2_WITH_CONFLICTS, None)] mut populated_state: ServerState,
        #[with( 2, &[replace!(line: 7, character: 0, old: "text.", new: "words!")])]
        did_change_incrementally: lsp_server::Notification,
    ) {
        let result = on_did_change_text_document(&mut populated_state, did_change_incrementally);
        let (_uri, version) = result.unwrap().unwrap();
        assert_eq!(2, version);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        let new_text = TEXT2_WITH_CONFLICTS.replace("text.", "words!");
        assert_eq!(2, locked_document_state.version());
        assert_eq!(new_text, locked_document_state.content());
        assert!(locked_document_state.merge_conflict.is_none());
    }

    macro_rules! remove {
        (line: $line:expr, character: $char:expr, $s:expr) => {
            replace!(line: $line, character: $char, old: $s, new: "")
        };
    }

    #[rstest]
    fn change_document_with_markers_incrementally_changed_using_remove_outside_of_markers_returns_document_data(
        #[with(1, TEXT2_WITH_CONFLICTS, None)] mut populated_state: ServerState,
        #[with(2, &[remove!(line: 7, character: 0, "text.\n")])]
        did_change_incrementally: lsp_server::Notification,
    ) {
        let result = on_did_change_text_document(&mut populated_state, did_change_incrementally);
        let (_uri, version) = result.unwrap().unwrap();
        assert_eq!(2, version);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        let new_text = TEXT2_WITH_CONFLICTS.replace("text.\n", "");
        assert_eq!(2, locked_document_state.version());
        assert_eq!(new_text, locked_document_state.content());
        assert!(locked_document_state.merge_conflict.is_none());
    }

    macro_rules! Range {
        (($start_line:expr, $start_char:expr), ($end_line:expr, $end_char:expr)) => {
            lsp_types::Range {
                start: lsp_types::Position {
                    line: $start_line,
                    character: $start_char,
                },
                end: lsp_types::Position {
                    line: $end_line,
                    character: $end_char,
                },
            }
        };
    }

    #[rstest]
    fn code_action_request_returns_correct_replacement_text(mut state: ServerState) {
        let uri_value = uri();
        let merge_conflict = parse(TEXT1_WITH_CONFLICTS)
            .expect("successful parse")
            .unwrap();
        assert_eq!(merge_conflict.conflicts.len(), 2);

        {
            let mut documents = state.documents.lock().unwrap();
            documents.insert(
                uri_value.clone(),
                Arc::new(Mutex::new(DocumentState::new_with_conflict(
                    TEXT1_WITH_CONFLICTS.to_string(),
                    0,
                    merge_conflict,
                ))),
            );
        }

        let params = lsp_types::CodeActionParams {
            text_document: lsp_types::TextDocumentIdentifier { uri: uri_value },
            range: Range!((2, 0), (2, 1)),
            context: lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let request = lsp_server::Request {
            id: 1.into(),
            method: <lsp_types::request::CodeActionRequest as lsp_types::request::Request>::METHOD
                .to_owned(),
            params: serde_json::to_value(params).unwrap(),
        };

        let response = on_code_action_request(&mut state, request)
            .expect("successful response")
            .expect("a response");
        let actions: Vec<lsp_types::CodeAction> =
            serde_json::from_value(response.result.unwrap()).unwrap();

        assert_eq!(4, actions.len());

        let replacement = |action: &lsp_types::CodeAction| -> String {
            // the HashMap definition for `changes` is not owned by this project. It comes from the LSP crate.
            #[allow(clippy::mutable_key_type)]
            let changes = action
                .edit
                .as_ref()
                .expect("valid action")
                .changes
                .as_ref()
                .expect("valid changes");
            let item = changes.values().next().expect("there is an initial change");
            item[0].new_text.clone()
        };

        assert_eq!("Keep OURS", actions[0].title);
        assert_eq!("plain old\n", replacement(&actions[0]));

        assert_eq!("Keep THEIRS", actions[1].title);
        assert_eq!("new and improved\n", replacement(&actions[1]));

        assert_eq!("Keep both", actions[2].title);
        assert_eq!("plain old\nnew and improved\n", replacement(&actions[2]));
    }

    #[rstest]
    fn code_action_drop_all_produces_empty_replacement(mut state: ServerState) {
        let uri_value = uri();
        let merge_conflict = parse(TEXT2_WITH_CONFLICTS)
            .expect("successful parse")
            .unwrap();

        {
            let mut documents = state.documents.lock().unwrap();
            documents.insert(
                uri_value.clone(),
                Arc::new(Mutex::new(DocumentState::new_with_conflict(
                    TEXT2_WITH_CONFLICTS.to_string(),
                    0,
                    merge_conflict,
                ))),
            );
        }

        let params = lsp_types::CodeActionParams {
            text_document: lsp_types::TextDocumentIdentifier {
                uri: uri_value.clone(),
            },
            range: Range!((3, 0), (3, 1)),
            context: lsp_types::CodeActionContext {
                diagnostics: vec![],
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: Default::default(),
            partial_result_params: Default::default(),
        };
        let request = lsp_server::Request {
            id: 1.into(),
            method: <lsp_types::request::CodeActionRequest as lsp_types::request::Request>::METHOD
                .to_owned(),
            params: serde_json::to_value(params).unwrap(),
        };

        let response = on_code_action_request(&mut state, request)
            .expect("successful response")
            .expect("a response");
        let actions: Vec<lsp_types::CodeAction> =
            serde_json::from_value(response.result.unwrap()).unwrap();

        let drop_all = actions.last().expect("at least one action");
        assert_eq!("Drop all", drop_all.title);

        #[allow(clippy::mutable_key_type)]
        let changes = drop_all
            .edit
            .as_ref()
            .expect("valid action")
            .changes
            .as_ref()
            .expect("valid changes");
        let edits = changes.values().next().expect("there is a change");
        assert_eq!("", edits[0].new_text);
    }
}
