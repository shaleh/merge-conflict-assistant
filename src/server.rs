use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
};

use crate::parser::{ConflictRegion, MergeConflict, parse, range_for_diagnostic_conflict};

type LSPResult = anyhow::Result<Option<(lsp_types::Uri, i32)>>;

#[derive(Clone, Default, Debug)]
struct DocumentState {
    content: String,
    version: i32,
    merge_conflict: Option<MergeConflict>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum ServerStatus {
    Running,
    ShutdownRequested,
    ExitReceived,
}

#[derive(Clone, Debug)]
struct ServerState {
    status: ServerStatus,
    sender: Arc<Mutex<crossbeam_channel::Sender<lsp_server::Message>>>,
    documents: Arc<Mutex<HashMap<lsp_types::Uri, Arc<Mutex<DocumentState>>>>>,
}

pub fn main_loop(connection: lsp_server::Connection) -> LSPResult {
    real_main_loop(connection)?;
    tracing::debug!("shutting down server");
    Ok(None)
}

fn real_main_loop(connection: lsp_server::Connection) -> LSPResult {
    let mut state = ServerState {
        status: ServerStatus::Running,
        sender: Arc::new(Mutex::new(connection.sender)),
        documents: Arc::new(Mutex::new(HashMap::new())),
    };
    let mut handles: Vec<thread::JoinHandle<()>> = Vec::new();

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
            if let Some((uri, version)) = state.on_notification_message(notification)? {
                let state = (*state).clone();
                let handle = thread::spawn(move || {
                    tracing::debug!("document update worker started for {:?} version {}", uri, version);
                    let reply = state.on_document_update(&uri, version);
                    if let Ok(message) = reply {
                        if let Some(message) = message {
                            let sender = state.sender.lock().expect("lock on sender");
                            if let Err(e) = sender.send(message.into()) {
                                tracing::error!("Failed to send message: {e}");
                            }
                        }
                    } else {
                        tracing::error!("{reply:?}");
                    }
                    tracing::debug!("document update worker finished for {:?}", uri);
                });
                handles.push(handle);
            }
        }
        lsp_server::Message::Request(request) => {
            let reply = state.on_request(request)?;
            if let Some(message) = reply {
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

impl ServerState {
    fn on_did_open_text_document(&self, notification: lsp_server::Notification) -> LSPResult {
        let lsp_types::DidOpenTextDocumentParams { text_document, .. } =
            serde_json::from_value(notification.params)?;
        tracing::info!("did open: {:?}", text_document.uri);
        tracing::debug!("content: {:?}", text_document.text);
        let mut documents = self
            .documents
            .lock()
            .map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
        // Always insert. Even if there was a previous version, didOpen means a new version of the file opened.
        documents.insert(
            text_document.uri.clone(),
            Arc::new(Mutex::new(DocumentState {
                content: text_document.text.clone(),
                version: text_document.version,
                merge_conflict: None,
            })),
        );
        Ok(Some((text_document.uri, text_document.version)))
    }

    fn on_did_change_text_document(&self, notification: lsp_server::Notification) -> LSPResult {
        let lsp_types::DidChangeTextDocumentParams {
            text_document,
            content_changes,
            ..
        } = serde_json::from_value(notification.params)?;
        tracing::info!("did change: {:?}: version {}", text_document.uri, text_document.version);
        tracing::debug!("content changes: {:?}", content_changes);
        let doc_state = {
            let mut documents = self
                .documents
                .lock()
                .map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
            let Some(doc_state) = documents.get_mut(&text_document.uri) else {
                tracing::debug!("failed to find document: {:?}", text_document.uri);
                return Ok(None);
            };
            Arc::clone(doc_state)
        };
        let mut locked_doc_state = doc_state
            .lock()
            .map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
        if locked_doc_state.version > text_document.version {
            tracing::debug!(
                "Version skew detected! {} v. {}",
                locked_doc_state.version,
                text_document.version
            );
        }
        tracing::debug!("applying changes");
        locked_doc_state.content = apply_changes(
            std::mem::take(&mut locked_doc_state.content),
            &content_changes,
        );
        Ok(Some((text_document.uri.clone(), text_document.version)))
    }

    fn on_did_close_text_document(&self, notification: lsp_server::Notification) -> LSPResult {
        let lsp_types::DidCloseTextDocumentParams { text_document, .. } =
            serde_json::from_value(notification.params)?;
        tracing::info!("did close: {:?}", text_document.uri);
        let mut documents = self
            .documents
            .lock()
            .map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
        if documents.remove(&text_document.uri).is_some() {
            tracing::debug!("Clearing {:?} from list of documents", text_document.uri);
        }
        Ok(None)
    }

    fn on_notification_message(&mut self, notification: lsp_server::Notification) -> LSPResult {
        tracing::debug!("heard notification {notification:?}");
        match notification.method.as_ref() {
            "exit" => {
                tracing::debug!("exit notification received");
                self.status = ServerStatus::ExitReceived;
                Ok(None)
            }
            "textDocument/didOpen" => self.on_did_open_text_document(notification),
            "textDocument/didClose" => self.on_did_close_text_document(notification),
            "textDocument/didChange" => self.on_did_change_text_document(notification),
            unhandled => {
                tracing::debug!("notification: ignored: {unhandled:?}");
                Ok(None)
            }
        }
    }

    fn on_request(
        &mut self,
        request: lsp_server::Request,
    ) -> anyhow::Result<Option<lsp_server::Response>> {
        tracing::debug!("got request: {request:?}");

        if self.status != ServerStatus::Running {
            return Ok(Some(lsp_server::Response::new_err(
                request.id,
                lsp_server::ErrorCode::InvalidRequest as i32,
                "Server is shutting down.".to_owned(),
            )));
        }

        match request.method.as_ref() {
            "shutdown" => self.on_shutdown(request),
            "textDocument/codeAction" => self.on_code_action_request(request),
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

    fn on_shutdown(
        &mut self,
        request: lsp_server::Request,
    ) -> anyhow::Result<Option<lsp_server::Response>> {
        tracing::info!("shutdown requested");
        self.status = ServerStatus::ShutdownRequested;
        Ok(Some(lsp_server::Response::new_ok(
            request.id,
            serde_json::Value::Null,
        )))
    }

    fn on_code_action_request(
        &self,
        request: lsp_server::Request,
    ) -> anyhow::Result<Option<lsp_server::Response>> {
        tracing::debug!("code action");
        let (id, params): (lsp_server::RequestId, lsp_types::CodeActionParams) = request.extract(
            <lsp_types::request::CodeActionRequest as lsp_types::request::Request>::METHOD,
        )?;
        let empty_actions: Vec<lsp_types::CodeAction> = Vec::new();

        let document_state = {
            let documents = self
                .documents
                .lock()
                .map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
            let Some(document_state) = documents.get(&params.text_document.uri) else {
                tracing::debug!("{:?} not found", params.text_document.uri);
                return Ok(Some(lsp_server::Response::new_ok(id, empty_actions)));
            };
            Arc::clone(document_state)
        };

        let locked_document_state = document_state
            .lock()
            .map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
        let Some(merge_conflict) = locked_document_state.merge_conflict.as_ref() else {
            return Ok(Some(lsp_server::Response::new_ok(id, empty_actions)));
        };
        let Some(conflict) = merge_conflict
            .conflicts()
            .find(|conflict| conflict.is_in_range(&params.range))
        else {
            return Ok(Some(lsp_server::Response::new_ok(id, empty_actions)));
        };
        let actions =
            conflict_as_code_actions(conflict, &params.text_document.uri, &locked_document_state);
        Ok(Some(lsp_server::Response::new_ok(id, actions)))
    }

    fn on_document_update(
        &self,
        uri: &lsp_types::Uri,
        version: i32,
    ) -> anyhow::Result<Option<lsp_server::Notification>> {
        let doc_state = {
            let documents = self
                .documents
                .lock()
                .map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
            let Some(doc_state) = documents.get(uri) else {
                tracing::debug!("No entry to {uri:?}");
                return Ok(None);
            };
            Arc::clone(doc_state)
        };

        let mut locked_doc_state = doc_state
            .lock()
            .map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
        if version >= locked_doc_state.version {
            locked_doc_state.version = version;
        } else {
            tracing::debug!("Missed update, skipping.");
            return Ok(None);
        }

        if !locked_doc_state
            .content
            .contains(crate::parser::MARKER_HEAD)
        {
            if locked_doc_state.merge_conflict.is_none() {
                return Ok(None);
            }
            locked_doc_state.merge_conflict.take();
            return prepare_diagnostics(uri, &locked_doc_state);
        }

        let merge_conflict = parse(uri, &locked_doc_state.content)?;
        tracing::info!(
            "{:?}: parsed {} conflict(s)",
            uri,
            merge_conflict.as_ref().map_or(0, |mc| mc.conflicts().count())
        );
        tracing::debug!("Conflicts: {:?}", merge_conflict);

        /*
        previous | new    | action
        ---------+--------+-------
        None     | None   | Nothing
        [data]   | [data] | Nothing
        [data]   | None   | send empty diagnostics, empty state
        [data]   | [new]  | send diagnostics, ensure new value in state
        None     | [new]  | send diagnostics, ensure new value in state
        */
        match (
            locked_doc_state.merge_conflict.as_ref(),
            merge_conflict.as_ref(),
        ) {
            (None, None) => {
                tracing::debug!("No current or previous, nothing to do.");
            }
            (Some(previous), Some(current)) if previous == current => {
                tracing::debug!("Change did not require new diagnostics");
            }
            _ => {
                tracing::debug!("needs update");
                if let Some(current_conflict) = merge_conflict {
                    locked_doc_state.merge_conflict.replace(current_conflict);
                } else {
                    locked_doc_state.merge_conflict.take();
                }
                return prepare_diagnostics(uri, &locked_doc_state);
            }
        }

        Ok(None)
    }
}

fn prepare_diagnostics(
    uri: &lsp_types::Uri,
    doc_state: &DocumentState,
) -> anyhow::Result<Option<lsp_server::Notification>> {
    let diagnostics: Vec<lsp_types::Diagnostic> = match doc_state.merge_conflict.as_ref() {
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
        doc_state.version
    );
    let publish_diagnostics_params = lsp_types::PublishDiagnosticsParams {
        uri: uri.clone(),
        diagnostics,
        version: Some(doc_state.version),
    };
    let notification = lsp_server::Notification::new(
                <lsp_types::notification::PublishDiagnostics as lsp_types::notification::Notification>::METHOD.to_owned(),
                publish_diagnostics_params,
            );

    Ok(Some(notification))
}

fn conflict_as_code_actions(
    conflict: &ConflictRegion,
    uri: &lsp_types::Uri,
    document_state: &DocumentState,
) -> Vec<lsp_types::CodeAction> {
    macro_rules! as_string_with_default {
        ($s:expr, $option:expr, $default:expr) => {
            match $option.as_ref() {
                Some(value) => format!($s, value),
                None => format!($s, $default),
            }
        };
    }

    let diagnostic = lsp_types::Diagnostic::from(conflict);
    let current_conflict = document_state
        .merge_conflict
        .as_ref()
        .expect("valid merge conflict reference");

    let mut items = vec![
        make_code_action(
            as_string_with_default!("Keep {}", current_conflict.head, "HEAD"),
            uri,
            document_state,
            range_for_diagnostic_conflict(conflict),
            &[conflict.head_range()],
            None,
            diagnostic.clone(),
        ),
        make_code_action(
            as_string_with_default!("Keep {}", current_conflict.branch, "branch"),
            uri,
            document_state,
            range_for_diagnostic_conflict(conflict),
            &[conflict.branch_range()],
            None,
            diagnostic.clone(),
        ),
        make_code_action(
            "Keep both".to_string(),
            uri,
            document_state,
            range_for_diagnostic_conflict(conflict),
            &[conflict.head_range(), conflict.branch_range()],
            None,
            diagnostic.clone(),
        ),
    ];

    if let Some(ancestor_range) = conflict.ancestor_range() {
        items.push(make_code_action(
            as_string_with_default!("Keep {}", current_conflict.ancestor, "ancestor"),
            uri,
            document_state,
            range_for_diagnostic_conflict(conflict),
            &[ancestor_range],
            None,
            diagnostic.clone(),
        ));
    }

    // Always the last item.
    items.push(make_code_action(
        "Drop all".to_string(),
        uri,
        document_state,
        range_for_diagnostic_conflict(conflict),
        &[],
        None,
        diagnostic.clone(),
    ));

    tracing::info!(
        "offering {} code action(s) for conflict at lines {}-{} in {:?}",
        items.len(),
        conflict.head,
        conflict.end,
        uri,
    );
    items
}

fn make_code_action(
    title: String,
    uri: &lsp_types::Uri,
    document_state: &DocumentState,
    range: lsp_types::Range,
    kept_regions: &[(u32, u32)],
    is_preferred: Option<bool>,
    diagnostic: lsp_types::Diagnostic,
) -> lsp_types::CodeAction {
    let offsets = build_line_offsets(&document_state.content);
    let text_len = document_state.content.len();
    let mut lines: Vec<&str> = Vec::with_capacity(kept_regions.len());
    for (start, end) in kept_regions {
        let start = index_for_position_with_offsets(
            &lsp_types::Position {
                // start is the marker, we want the content. Move down one line.
                line: start + 1,
                character: 0,
            },
            &offsets,
            text_len,
        )
        .expect("valid index for start position");
        let end = index_for_position_with_offsets(
            &lsp_types::Position {
                line: *end,
                character: 0,
            },
            &offsets,
            text_len,
        )
        .expect("valid index for end position");
        lines.push(&document_state.content[start..end]);
    }
    let new_text = lines.join("");
    let edit = lsp_types::TextEdit { range, new_text };

    lsp_types::CodeAction {
        title,
        is_preferred,
        kind: Some(lsp_types::CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic]),
        edit: Some(lsp_types::WorkspaceEdit {
            changes: Some(HashMap::from([(uri.clone(), vec![edit])])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn apply_changes(
    mut updated: String,
    changes: &[lsp_types::TextDocumentContentChangeEvent],
) -> String {
    let mut offsets = build_line_offsets(&updated);

    let (full_replacements, mut ranged): (Vec<_>, Vec<_>) =
        changes.iter().partition(|c| c.range.is_none());

    for change in &full_replacements {
        updated.replace_range(.., &change.text);
        offsets = build_line_offsets(&updated);
    }

    let is_ascending = ranged.windows(2).all(|pair| {
        let a = pair[0].range.expect("partitioned into ranged").start;
        let b = pair[1].range.expect("partitioned into ranged").start;
        (a.line, a.character) <= (b.line, b.character)
    });

    if !is_ascending {
        ranged.sort_by(|a, b| {
            let ra = a.range.expect("partitioned into ranged");
            let rb = b.range.expect("partitioned into ranged");
            rb.start
                .line
                .cmp(&ra.start.line)
                .then(rb.start.character.cmp(&ra.start.character))
        });
    }

    for change in &ranged {
        let range = change.range.expect("partitioned into ranged");
        let start = index_for_position_with_offsets(&range.start, &offsets, updated.len());
        let end = index_for_position_with_offsets(&range.end, &offsets, updated.len());
        if let (Some(start), Some(end)) = (start, end) {
            updated.replace_range(start..end, &change.text);
            offsets = build_line_offsets(&updated);
        } else {
            tracing::warn!("Failed to map range to byte indices: start={start:?}, end={end:?}");
        }
    }

    updated
}

fn build_line_offsets(text: &str) -> Vec<usize> {
    let count = 1 + text.bytes().filter(|&b| b == b'\n').count();
    let mut offsets = Vec::with_capacity(count);
    offsets.push(0);
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            offsets.push(idx + 1);
        }
    }
    offsets
}

fn index_for_position_with_offsets(
    position: &lsp_types::Position,
    offsets: &[usize],
    text_len: usize,
) -> Option<usize> {
    let line_start = *offsets.get(position.line as usize)?;
    let index = line_start + position.character as usize;
    if index <= text_len { Some(index) } else { None }
}

#[cfg(test)]
mod test {
    use crossbeam_channel::unbounded;
    use lsp_types::notification::{DidChangeTextDocument, DidOpenTextDocument};
    use rstest::*;

    use super::*;
    use crate::conflict_text;
    #[allow(unused_imports)]
    use crate::test_helpers::init_logging;

    fn index_for_position(position: &lsp_types::Position, value: &str) -> Option<usize> {
        let offsets = build_line_offsets(value);
        index_for_position_with_offsets(position, &offsets, value.len())
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

    macro_rules! remove {
        (line: $line:expr, character: $char:expr, $s:expr) => {
            replace!(line: $line, character: $char, old: $s, new: "")
        };
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

    #[test]
    fn character_position_when_line_is_zero() {
        let position = lsp_types::Position {
            line: 0,
            character: 5,
        };
        assert_eq!(Some(5), index_for_position(&position, "something\nelse"));
    }

    #[test]
    fn position_includes_line_when_line_is_greater_than_zero() {
        let position = lsp_types::Position {
            line: 1,
            character: 5,
        };
        assert_eq!(
            Some(15), // len(something) + 1 for newline + character + 1
            index_for_position(&position, "something\nand then more")
        );
    }

    #[test]
    fn apply_changes_does_mutate_text_at_beginning() {
        let text = "initial text\nline 2\nline 3\nlast line".to_string();
        let range = Range!((0, 0), (0, 1));
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: Some(range),
            range_length: None,
            text: "I".to_string(),
        }];
        let updated = apply_changes(text, &changes);
        let expected = "Initial text\nline 2\nline 3\nlast line";
        assert_eq!(expected, updated);
    }

    #[test]
    fn apply_changes_does_delete_character() {
        let text = "initial text\nline 12\nline 3\nlast line".to_string();
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: Some(Range!((1, 5), (1, 6))),
            range_length: None,
            text: "".to_string(),
        }];
        let updated = apply_changes(text, &changes);
        let expected = "initial text\nline 2\nline 3\nlast line";
        assert_eq!(expected, updated);
    }

    #[test]
    fn apply_changes_does_add_character() {
        let text = "initial text\nline 2\nline 3\nlast line".to_string();
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: Some(Range!((1, 5), (1, 5))),
            range_length: None,
            text: "1".to_string(),
        }];
        let updated = apply_changes(text, &changes);
        let expected = "initial text\nline 12\nline 3\nlast line";
        assert_eq!(expected, updated);
    }

    #[test]
    fn apply_changes_ascending_with_line_shift() {
        // First change inserts a newline, shifting all subsequent lines down.
        // Second change targets a line using post-shift positions.
        // This validates that byte offsets are recalculated between ascending edits.
        let text = "aa\nbb\ncc\n".to_string();
        let changes = [
            // Insert "xx\n" at start — "bb" moves from line 1 to line 2.
            lsp_types::TextDocumentContentChangeEvent {
                range: Some(Range!((0, 0), (0, 0))),
                range_length: None,
                text: "xx\n".to_string(),
            },
            // Replace "bb" on its new line (2). Without offset rebuild this
            // would hit "cc" (line 2 in the original offsets).
            lsp_types::TextDocumentContentChangeEvent {
                range: Some(Range!((2, 0), (2, 2))),
                range_length: None,
                text: "BB".to_string(),
            },
        ];
        let updated = apply_changes(text, &changes);
        assert_eq!("xx\naa\nBB\ncc\n", updated);
    }

    #[test]
    fn apply_changes_does_mutate_text() {
        let text = "initial text\nline 2\nline 3\nlast line".to_string();

        let changes = [
            lsp_types::TextDocumentContentChangeEvent {
                range: Some(Range!((1, 5), (1, 5))),
                range_length: None,
                text: "1".to_string(),
            },
            lsp_types::TextDocumentContentChangeEvent {
                range: Some(Range!((1, 6), (1, 6))),
                range_length: None,
                text: "2".to_string(),
            },
            lsp_types::TextDocumentContentChangeEvent {
                range: Some(Range!((2, 5), (2, 5))),
                range_length: None,
                text: "2".to_string(),
            },
        ];

        let updated = apply_changes(text, &changes);
        let expected = "initial text\nline 122\nline 23\nlast line".to_string();
        assert_eq!(expected, updated);
    }

    #[fixture]
    fn uri() -> lsp_types::Uri {
        "file://foo.txt".parse().unwrap()
    }

    #[fixture]
    fn version(#[default(0)] value: i32) -> i32 {
        value
    }

    #[fixture]
    fn state() -> ServerState {
        let (_, reader_receiver) = unbounded::<lsp_server::Message>();
        let (writer_sender, _) = unbounded::<lsp_server::Message>();
        let connection = lsp_server::Connection {
            sender: writer_sender,
            receiver: reader_receiver,
        };
        ServerState {
            status: ServerStatus::Running,
            sender: Arc::new(Mutex::new(connection.sender)),
            documents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[fixture]
    fn populated_state(
        version: i32,
        #[default("")] text: &str,
        #[default(None)] merge_conflict: Option<MergeConflict>,
    ) -> ServerState {
        let state = state();
        {
            let mut documents = state.documents.lock().unwrap();
            documents.insert(
                uri(),
                Arc::new(Mutex::new(DocumentState {
                    version,
                    content: text.to_string(),
                    merge_conflict,
                })),
            );
        }
        state
    }

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

    static TEXT1_RESOLVED: &str = "
This is some
plain old
text.
Nothing to see here.
";

    static TEXT1_WITH_CONFLICTS: &str = concat!(
        "\nThis is some\n",
        conflict_text!("OURS", "plain old", "THEIRS", "new and improved"),
        "text.\n",
        conflict_text!("OURS", "Nothing to see here.", "THEIRS", "Cool stuff."),
        "\nFinal text",
    );

    static TEXT2_WITH_CONFLICTS: &str = concat!(
        "\nThis is some\n",
        conflict_text!("plain old", "new and improved"),
        "text.\n",
        conflict_text!("Nothing to see here.", "Cool stuff."),
        "\nFinal text\n",
    );

    static TEXT2_RESOLVED: &str = "
This is some
plain old
text.
Cool stuff.
";

    #[fixture]
    #[once]
    fn conflicts_for_text2_with_conflicts() -> MergeConflict {
        MergeConflict {
            head: None,
            branch: None,
            ancestor: None,
            conflicts: vec![
                ConflictRegion {
                    head: 2,
                    branch: 4,
                    end: 6,
                    ancestor: None,
                },
                ConflictRegion {
                    head: 8,
                    branch: 10,
                    end: 12,
                    ancestor: None,
                },
            ],
        }
    }

    #[rstest]
    fn open_document_with_no_markers_returns_document_data(
        uri: lsp_types::Uri,
        state: ServerState,
        #[with(1, TEXT1_RESOLVED)] did_open: lsp_server::Notification,
    ) {
        let result = state.on_did_open_text_document(did_open);
        let (_uri, version) = result.unwrap().unwrap();
        let documents = state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(1, version);
        assert_eq!(1, locked_document_state.version);
        assert_eq!(TEXT1_RESOLVED, locked_document_state.content);
        assert!(locked_document_state.merge_conflict.is_none());
    }

    #[rstest]
    fn open_document_with_markers_returns_document_data(
        uri: lsp_types::Uri,
        state: ServerState,
        #[with(5, TEXT1_WITH_CONFLICTS)] did_open: lsp_server::Notification,
    ) {
        let result = state.on_did_open_text_document(did_open);
        let (_uri, version) = result.unwrap().unwrap();
        let documents = state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(5, version);
        assert_eq!(5, locked_document_state.version);
        assert_eq!(TEXT1_WITH_CONFLICTS, locked_document_state.content);
        assert!(locked_document_state.merge_conflict.is_none());
    }

    #[rstest]
    fn change_document_with_no_markers_returns_document_data(
        #[with(2, TEXT2_RESOLVED)] populated_state: ServerState,
        #[with(3, TEXT2_RESOLVED)] did_change_whole_document: lsp_server::Notification,
    ) {
        let result = populated_state.on_did_change_text_document(did_change_whole_document);
        let (_uri, version) = result.unwrap().unwrap();
        assert_eq!(3, version);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(2, locked_document_state.version);
        assert_eq!(TEXT2_RESOLVED, locked_document_state.content);
        assert!(locked_document_state.merge_conflict.is_none());
    }

    #[rstest]
    fn change_document_with_no_markers_replaced_with_markers_returns_diagnostics(
        #[with(1, TEXT2_RESOLVED)] populated_state: ServerState,
        #[with(2, TEXT2_WITH_CONFLICTS)] did_change_whole_document: lsp_server::Notification,
    ) {
        let result = populated_state.on_did_change_text_document(did_change_whole_document);
        let (_uri, version) = result.unwrap().unwrap();
        assert_eq!(2, version);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(1, locked_document_state.version);
        assert_eq!(TEXT2_WITH_CONFLICTS, locked_document_state.content);
        assert!(locked_document_state.merge_conflict.is_none());
    }

    #[rstest]
    fn change_document_with_markers_incrementally_changed_outside_of_markers_returns_document_data(
        uri: lsp_types::Uri,
        #[with(1, TEXT2_WITH_CONFLICTS, Some(conflicts_for_text2_with_conflicts()))]
        populated_state: ServerState,
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
        let (_uri, version) = populated_state
            .on_did_change_text_document(did_change_incrementally)
            .unwrap()
            .unwrap();
        assert_eq!(2, version);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(1, locked_document_state.version);
        assert_eq!(
            format!("!\n# Just a comment.\n{}@", TEXT2_WITH_CONFLICTS),
            locked_document_state.content
        );
        assert_eq!(
            locked_document_state.merge_conflict,
            Some(conflicts_for_text2_with_conflicts())
        );
    }

    #[rstest]
    fn change_document_with_markers_incrementally_changed_using_replace_outside_of_markers_returns_document_data(
        #[with(1, TEXT2_WITH_CONFLICTS, None)] populated_state: ServerState,
        #[with( 2, &[replace!(line: 7, character: 0, old: "text.", new: "words!")])]
        did_change_incrementally: lsp_server::Notification,
    ) {
        let result = populated_state.on_did_change_text_document(did_change_incrementally);
        let (_uri, version) = result.unwrap().unwrap();
        assert_eq!(2, version);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        let new_text = TEXT2_WITH_CONFLICTS.replace("text.", "words!");
        assert_eq!(1, locked_document_state.version);
        assert_eq!(new_text, locked_document_state.content);
        assert!(locked_document_state.merge_conflict.is_none());
    }

    #[rstest]
    fn change_document_with_markers_incrementally_changed_using_remove_outside_of_markers_returns_document_data(
        #[with(1, TEXT2_WITH_CONFLICTS, None)] populated_state: ServerState,
        #[with(2, &[remove!(line: 7, character: 0, "text.\n")])]
        did_change_incrementally: lsp_server::Notification,
    ) {
        let result = populated_state.on_did_change_text_document(did_change_incrementally);
        let (_uri, version) = result.unwrap().unwrap();
        assert_eq!(2, version);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        let new_text = TEXT2_WITH_CONFLICTS.replace("text.\n", "");
        assert_eq!(1, locked_document_state.version);
        assert_eq!(new_text, locked_document_state.content);
        assert!(locked_document_state.merge_conflict.is_none());
    }

    #[rstest]
    fn on_document_update_when_document_without_conflicts_opened_no_notification_sent(
        uri: lsp_types::Uri,
        #[with(2, TEXT2_RESOLVED, None)] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(3, locked_document_state.version);
        let notification = result.unwrap();
        assert!(notification.is_none());
    }

    #[rstest]
    fn on_document_update_when_document_has_conflicts_previously_but_not_last_generation_changed_no_notification_sent(
        uri: lsp_types::Uri,
        #[with(2, TEXT2_RESOLVED)] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(3, locked_document_state.version);
        assert!(
            locked_document_state.merge_conflict.is_none(),
            "{:?}",
            locked_document_state.merge_conflict
        );
        let notification = result.unwrap();
        assert!(notification.is_none());
    }

    #[rstest]
    fn on_document_update_version_missed_no_notification_sent(
        uri: lsp_types::Uri,
        #[with(6, TEXT2_RESOLVED)] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(6, locked_document_state.version);
        assert!(locked_document_state.merge_conflict.is_none());
        let notification = result.unwrap();
        assert!(notification.is_none());
    }

    #[rstest]
    fn on_document_update_version_initial_version_no_conflicts_no_notification_sent(
        uri: lsp_types::Uri,
        #[with(0, TEXT2_RESOLVED, None)] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 0);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(0, locked_document_state.version);
        assert!(locked_document_state.merge_conflict.is_none());
        let notification = result.unwrap();
        assert!(notification.is_none());
    }

    #[rstest]
    fn on_document_update_version_initial_version_with_conflicts_notification_sent(
        uri: lsp_types::Uri,
        #[with(0, TEXT2_WITH_CONFLICTS, None)] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 0);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(0, locked_document_state.version);
        let merge_conflict = conflicts_for_text2_with_conflicts();
        assert_eq!(
            Some(merge_conflict.clone()),
            locked_document_state.merge_conflict
        );
        let notification = result.unwrap().unwrap();
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        let notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(notification.params).unwrap();
        let diagnostics = notification_params.diagnostics;
        assert_eq!(
            merge_conflict
                .conflicts()
                .map(lsp_types::Diagnostic::from)
                .collect::<Vec<_>>(),
            diagnostics,
        );
    }

    #[rstest]
    fn on_document_update_when_document_has_conflicts_previously_changed_empty_notification_sent(
        uri: lsp_types::Uri,
        #[with(2, TEXT2_RESOLVED, Some(conflicts_for_text2_with_conflicts()))]
        populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(3, locked_document_state.version);
        assert_eq!(locked_document_state.merge_conflict, None);
        let notification = result.unwrap().unwrap();
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        let notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(notification.params).unwrap();
        let diagnostics = notification_params.diagnostics;
        assert!(diagnostics.is_empty());
    }

    #[rstest]
    fn on_document_update_when_document_has_conflicts_notification_sent(
        uri: lsp_types::Uri,
        #[with(2, TEXT2_WITH_CONFLICTS, None)] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(3, locked_document_state.version);
        assert_eq!(
            Some(conflicts_for_text2_with_conflicts()),
            locked_document_state.merge_conflict
        );
        let notification = result.unwrap().unwrap();
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        let notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(notification.params).unwrap();
        let diagnostics = notification_params.diagnostics;
        assert_eq!(
            conflicts_for_text2_with_conflicts()
                .conflicts()
                .map(lsp_types::Diagnostic::from)
                .collect::<Vec<_>>(),
            diagnostics
        );
    }

    #[rstest]
    fn on_document_update_when_document_has_conflicts_and_change_affecting_them_updated_notification_sent(
        uri: lsp_types::Uri,
        #[with(2, &format!("new text\n{}", TEXT2_WITH_CONFLICTS), Some(conflicts_for_text2_with_conflicts()))]
        populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(3, locked_document_state.version);
        let merge_conflict = MergeConflict {
            head: None,
            branch: None,
            ancestor: None,
            conflicts: vec![
                ConflictRegion {
                    head: 3,
                    branch: 5,
                    end: 7,
                    ancestor: None,
                },
                ConflictRegion {
                    head: 9,
                    branch: 11,
                    end: 13,
                    ancestor: None,
                },
            ],
        };
        assert_eq!(
            Some(merge_conflict.clone()),
            locked_document_state.merge_conflict
        );
        let notification = result.unwrap().unwrap();
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        let notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(notification.params).unwrap();
        let diagnostics = notification_params.diagnostics;
        assert_eq!(
            merge_conflict
                .conflicts()
                .map(lsp_types::Diagnostic::from)
                .collect::<Vec<_>>(),
            diagnostics,
        );
    }

    #[rstest]
    fn code_action_request_returns_correct_replacement_text(state: ServerState) {
        let uri_value = uri();
        let merge_conflict = parse(&uri_value, TEXT1_WITH_CONFLICTS)
            .expect("successful parse")
            .unwrap();
        assert_eq!(merge_conflict.conflicts.len(), 2);

        {
            let mut documents = state.documents.lock().unwrap();
            documents.insert(
                uri_value.clone(),
                Arc::new(Mutex::new(DocumentState {
                    version: 0,
                    content: TEXT1_WITH_CONFLICTS.to_string(),
                    merge_conflict: Some(merge_conflict),
                })),
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

        let response = state
            .on_code_action_request(request)
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
    fn code_action_drop_all_produces_empty_replacement(state: ServerState) {
        let uri_value = uri();
        let merge_conflict = parse(&uri_value, TEXT2_WITH_CONFLICTS)
            .expect("successful parse")
            .unwrap();

        {
            let mut documents = state.documents.lock().unwrap();
            documents.insert(
                uri_value.clone(),
                Arc::new(Mutex::new(DocumentState {
                    version: 0,
                    content: TEXT2_WITH_CONFLICTS.to_string(),
                    merge_conflict: Some(merge_conflict),
                })),
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

        let response = state
            .on_code_action_request(request)
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
