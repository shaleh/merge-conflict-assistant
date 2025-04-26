use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
};

use crate::parser::{Conflict, ConflictRegion, Parser, range_for_diagnostic_conflict};

type LSPResult = anyhow::Result<Option<(lsp_types::Uri, i32)>>;

#[derive(Clone, Default, Debug)]
struct DocumentState {
    content: String,
    version: i32,
    conflicts: Option<Vec<Conflict>>,
}

#[derive(Clone, Debug)]
struct ServerState {
    shutdown_requested: bool,
    sender: Arc<Mutex<crossbeam_channel::Sender<lsp_server::Message>>>,
    documents: Arc<Mutex<HashMap<lsp_types::Uri, DocumentState>>>,
}

pub struct MergeConflictAssistant {}

impl std::fmt::Debug for MergeConflictAssistant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeAssistant").finish()
    }
}

impl MergeConflictAssistant {
    pub fn main_loop(connection: lsp_server::Connection) -> LSPResult {
        let mut server = MergeConflictAssistant {};
        server.real_main_loop(connection)?;
        log::info!("shutting down server");
        Ok(None)
    }

    fn real_main_loop(&mut self, connection: lsp_server::Connection) -> LSPResult {
        let mut state = ServerState {
            shutdown_requested: false,
            sender: Arc::new(Mutex::new(connection.sender)),
            documents: Arc::new(Mutex::new(HashMap::new())),
        };

        for msg in &connection.receiver {
            self.handle_message(&mut state, msg)?;
        }
        Ok(None)
    }

    fn handle_message(&self, state: &mut ServerState, message: lsp_server::Message) -> LSPResult {
        log::debug!("got msg: {message:?}");
        match message {
            lsp_server::Message::Notification(notification) => {
                if let Some((uri, version)) = state.on_notification_message(notification)? {
                    let state = (*state).clone();
                    thread::spawn(move || {
                        let reply = state.on_document_update(&uri, version);
                        if let Ok(message) = reply {
                            if let Some(message) = message {
                                let sender = state.sender.lock().unwrap();
                                _ = sender.send(message.into());
                            }
                        } else {
                            log::error!("{reply:?}");
                        }
                    });
                }
            }
            lsp_server::Message::Request(request) => {
                let reply = state.on_request(request)?;
                if let Some(message) = reply {
                    let sender = state.sender.lock().unwrap();
                    _ = sender.send(message.into());
                }
            }
            lsp_server::Message::Response(response) => {
                log::debug!("got response: {response:?}");
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
}

impl ServerState {
    fn on_did_open_text_document(&self, notification: lsp_server::Notification) -> LSPResult {
        let lsp_types::DidOpenTextDocumentParams { text_document, .. } =
            serde_json::from_value(notification.params)?;
        log::debug!(
            "did open: {:?}: {:?}",
            text_document.uri,
            text_document.text
        );
        let mut documents = self.documents.lock().unwrap();
        documents
            .entry(text_document.uri.clone())
            .or_insert(DocumentState {
                content: text_document.text.clone(),
                version: text_document.version,
                conflicts: None,
            });
        Ok(Some((text_document.uri, text_document.version)))
    }

    fn on_did_change_text_document(&self, notification: lsp_server::Notification) -> LSPResult {
        let lsp_types::DidChangeTextDocumentParams {
            text_document,
            content_changes,
            ..
        } = serde_json::from_value(notification.params)?;
        log::debug!(
            "did change: {:?}: {}, {:?}",
            text_document.uri,
            text_document.version,
            content_changes
        );
        let mut documents = self.documents.lock().unwrap();
        if let Some(doc_state) = documents.get_mut(&text_document.uri) {
            if doc_state.version > text_document.version {
                log::debug!(
                    "Version skew detected! {} v. {}",
                    doc_state.version,
                    text_document.version
                );
            }
            log::debug!("applying changes");
            doc_state.content = apply_changes(&doc_state.content, &content_changes);
            return Ok(Some((text_document.uri.clone(), text_document.version)));
        } else {
            log::debug!("failed to find document: {:?}", text_document.uri);
        }
        Ok(None)
    }

    fn on_did_close_text_document(&self, notification: lsp_server::Notification) -> LSPResult {
        let lsp_types::DidCloseTextDocumentParams { text_document, .. } =
            serde_json::from_value(notification.params)?;
        log::debug!("did close: {:?}", text_document.uri);
        let mut documents = self.documents.lock().unwrap();
        if documents.remove(&text_document.uri).is_some() {
            log::debug!("Clearing {:?} from list of documents", text_document.uri);
        }
        Ok(None)
    }

    fn on_notification_message(&self, notification: lsp_server::Notification) -> LSPResult {
        log::debug!("heard notification {notification:?}");
        match notification.method.as_ref() {
            "textDocument/didOpen" => self.on_did_open_text_document(notification),
            "textDocument/didClose" => self.on_did_close_text_document(notification),
            "textDocument/didChange" => self.on_did_change_text_document(notification),
            unhandled => {
                log::debug!("notification: ignored: {unhandled:?}");
                Ok(None)
            }
        }
    }

    fn on_request(
        &mut self,
        request: lsp_server::Request,
    ) -> anyhow::Result<Option<lsp_server::Response>> {
        log::debug!("got request: {request:?}");

        if self.shutdown_requested {
            return self.on_shutdown(request);
        }

        match request.method.as_ref() {
            "shutdown" => self.on_shutdown(request),
            "textDocument/codeAction" => self.on_code_action_request(request),
            unhandled => {
                log::debug!("request: ignored: {unhandled:?}");
                Ok(None)
            }
        }
    }

    fn on_shutdown(
        &mut self,
        request: lsp_server::Request,
    ) -> anyhow::Result<Option<lsp_server::Response>> {
        self.shutdown_requested = true;
        Ok(Some(lsp_server::Response::new_err(
            request.id.clone(),
            lsp_server::ErrorCode::InvalidRequest as i32,
            "Shutdown already requested.".to_owned(),
        )))
    }

    fn on_code_action_request(
        &self,
        request: lsp_server::Request,
    ) -> anyhow::Result<Option<lsp_server::Response>> {
        log::debug!("code action");
        let (id, params): (lsp_server::RequestId, lsp_types::CodeActionParams) = request.extract(
            <lsp_types::request::CodeActionRequest as lsp_types::request::Request>::METHOD,
        )?;
        macro_rules! unwrap_or_return {
            ($option:expr) => {
                match $option {
                    Some(value) => value,
                    None => {
                        return Ok(None);
                    }
                }
            };
        }
        let documents = self.documents.lock().unwrap();
        let document_state = unwrap_or_return!(documents.get(&params.text_document.uri));
        let conflicts = unwrap_or_return!(document_state.conflicts.as_ref());
        let conflict = unwrap_or_return!(
            conflicts
                .iter()
                .find(|conflict| conflict.is_in_range(&params.range))
        );
        let actions = conflict_as_code_actions(conflict, &params.text_document.uri, document_state);
        Ok(Some(lsp_server::Response::new_ok(id, actions)))
    }

    fn on_document_update(
        &self,
        uri: &lsp_types::Uri,
        version: i32,
    ) -> anyhow::Result<Option<lsp_server::Notification>> {
        let mut documents = self.documents.lock().unwrap();
        let Some(doc_state) = documents.get_mut(uri) else {
            log::debug!("No entry to {uri:?}");
            return Ok(None);
        };

        if version >= doc_state.version {
            doc_state.version = version;
        } else {
            log::debug!("Missed update, skipping.");
            return Ok(None);
        }

        let conflicts = Parser::parse(uri, &doc_state.content)?.unwrap_or_else(Vec::new);
        log::debug!("Conflicts: {:?}", conflicts);

        /*
        previous | new   | action
        -------------------------
        None     | None  | Nothing
        None     | []    | Nothing
        []       | []    | set previous to None
        []       | None  | set previous to None
        [data]   | None  | send empty diagnostics, empty state
        [data]   | []    | send empty diagnostics, empty state
        [data]   | [new] | send diagnostics, ensure new value in state
        []       | [new] | send diagnostics, ensure new value in state
        None     | [new] | send diagnostics, ensure new value in state
        */
        let previous_conflicts = doc_state.conflicts.as_ref();
        let needs_update = if let Some(cs) = previous_conflicts {
            if cs.is_empty() && conflicts.is_empty() {
                doc_state.conflicts.take();
                false
            } else {
                *cs != conflicts
            }
        } else {
            !conflicts.is_empty()
        };
        log::debug!("needs update: {needs_update}");
        if needs_update {
            doc_state.conflicts.replace(conflicts);
            return prepare_diagnostics(uri, doc_state);
        } else {
            log::debug!("Change did not require new diagnostics");
        }

        Ok(None)
    }
}

fn prepare_diagnostics(
    uri: &lsp_types::Uri,
    doc_state: &DocumentState,
) -> anyhow::Result<Option<lsp_server::Notification>> {
    if let Some(conflicts) = doc_state.conflicts.as_ref() {
        log::debug!("conflicts to send");
        let diagnostics: Vec<lsp_types::Diagnostic> =
            conflicts.iter().map(lsp_types::Diagnostic::from).collect();
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
    } else {
        log::debug!("no conflicts");
        Ok(None)
    }
}

fn conflict_as_code_actions(
    conflict: &Conflict,
    uri: &lsp_types::Uri,
    document_state: &DocumentState,
) -> Vec<lsp_types::CodeAction> {
    macro_rules! as_string_with_default {
        ($s:expr, $option:expr, $default:expr) => {
            format!(
                $s,
                match $option.as_ref() {
                    Some(value) => value.clone(),
                    None => $default.to_string(),
                }
            )
        };
    }

    let diagnostic = lsp_types::Diagnostic::from(conflict);

    let mut items = vec![
        make_code_action(
            as_string_with_default!("Keep {}", conflict.ours.name, "OURS"),
            uri,
            document_state,
            range_for_diagnostic_conflict(conflict),
            &[&conflict.ours],
            diagnostic.clone(),
        ),
        make_code_action(
            as_string_with_default!("Keep {}", conflict.theirs.name, "THEIRS"),
            uri,
            document_state,
            range_for_diagnostic_conflict(conflict),
            &[&conflict.theirs],
            diagnostic.clone(),
        ),
        make_code_action(
            "Keep both".to_string(),
            uri,
            document_state,
            range_for_diagnostic_conflict(conflict),
            &[&conflict.ours, &conflict.theirs],
            diagnostic.clone(),
        ),
    ];

    if let Some(ancestor) = conflict.ancestor.as_ref() {
        items.push(make_code_action(
            as_string_with_default!("Keep {}", ancestor.name, "ancestor"),
            uri,
            document_state,
            range_for_diagnostic_conflict(conflict),
            &[ancestor],
            diagnostic.clone(),
        ));
    }

    items
}

fn make_code_action(
    title: String,
    uri: &lsp_types::Uri,
    document_state: &DocumentState,
    range: lsp_types::Range,
    kept_regions: &[&ConflictRegion],
    diagnostic: lsp_types::Diagnostic,
) -> lsp_types::CodeAction {
    let mut lines: Vec<&str> = Vec::with_capacity(kept_regions.len());
    for region in kept_regions {
        let start = index_for_position(
            &lsp_types::Position {
                // start is the marker, we want the content. Move down one line.
                line: region.start + 1,
                character: 0,
            },
            &document_state.content,
        )
        .unwrap();
        let end = index_for_position(
            &lsp_types::Position {
                line: region.end,
                character: 0,
            },
            &document_state.content,
        )
        .unwrap();
        lines.push(&document_state.content[(start as usize)..(end as usize)]);
    }
    let new_text = lines.join("");
    let edit = lsp_types::TextEdit { range, new_text };

    lsp_types::CodeAction {
        title,
        kind: Some(lsp_types::CodeActionKind::QUICKFIX),
        is_preferred: Some(true),
        diagnostics: Some(vec![diagnostic]),
        edit: Some(lsp_types::WorkspaceEdit {
            changes: Some(HashMap::from([(uri.clone(), vec![edit])])),
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn apply_changes(content: &str, changes: &[lsp_types::TextDocumentContentChangeEvent]) -> String {
    let mut updated = content.to_string();
    for change in changes {
        if let Some(range) = change.range {
            let start = index_for_position(&range.start, &updated);
            let end = index_for_position(&range.end, &updated);
            if let (Some(start), Some(end)) = (start, end) {
                updated.replace_range(start..end, &change.text);
            } else {
                log::debug!("eh?: {start:?} and {end:?}");
                continue;
            }
        } else {
            updated.replace_range(.., &change.text);
        }
    }

    updated
}

fn index_for_position(position: &lsp_types::Position, value: &str) -> Option<usize> {
    let index = if position.line == 0 {
        Some(0)
    } else {
        value
            .match_indices('\n')
            // The first newline starts the second line. nth is zero based. Step back one here.
            .nth(position.line as usize - 1)
            // then restore the proper count here.
            .map(|(idx, _)| idx + 1)
    };
    index.map(|idx| idx + (position.character as usize))
}

#[cfg(test)]
mod test {
    use super::*;
    use crossbeam_channel::unbounded;
    use lsp_types::notification::{DidChangeTextDocument, DidOpenTextDocument};
    use rstest::*;

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
        let text = "initial text\nline 2\nline 3\nlast line";
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
        let text = "initial text\nline 12\nline 3\nlast line";
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
        let text = "initial text\nline 2\nline 3\nlast line";
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
    fn apply_changes_does_mutate_text() {
        let text = "initial text\nline 2\nline 3\nlast line";

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
    fn server() -> MergeConflictAssistant {
        MergeConflictAssistant {}
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
            shutdown_requested: false,
            sender: Arc::new(Mutex::new(connection.sender)),
            documents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[fixture]
    fn populated_state(
        version: i32,
        #[default("")] text: &str,
        #[default(None)] conflicts: Option<Vec<Conflict>>,
    ) -> ServerState {
        let state = state();
        {
            let mut documents = state.documents.lock().unwrap();
            documents.insert(
                uri(),
                DocumentState {
                    version,
                    content: text.to_string(),
                    conflicts,
                },
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
    static TEXT1_WITH_CONFLICTS: &str = "
This is some
<<<<<<<
plain old
=======
new and improved
>>>>>>>
text.
<<<<<<<
Nothing to see here.
=======
Cool stuff.
>>>>>>>
";

    static TEXT2_WITH_CONFLICTS: &str = "
This is some
<<<<<<<
plain old
=======
new and improved
>>>>>>>
text.
<<<<<<<
Nothing to see here.
=======
Cool stuff.
>>>>>>>
";
    static TEXT2_RESOLVED: &str = "
This is some
plain old
text.
Cool stuff.
";

    #[fixture]
    #[once]
    fn conflicts_for_text2_with_conflicts() -> Vec<Conflict> {
        vec![
            Conflict::new((2, 4, ""), (4, 6, ""), 7).unwrap(),
            Conflict::new((8, 10, ""), (10, 12, ""), 7).unwrap(),
        ]
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
        assert_eq!(1, version);
        assert_eq!(1, document_state.version);
        assert_eq!(TEXT1_RESOLVED, document_state.content);
        assert!(document_state.conflicts.is_none());
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
        assert_eq!(5, version);
        assert_eq!(5, document_state.version);
        assert_eq!(TEXT1_WITH_CONFLICTS, document_state.content);
        assert!(document_state.conflicts.is_none());
    }

    #[rstest]
    fn change_document_with_no_markers_returns_document_data(
        #[with(2, TEXT2_RESOLVED)] populated_state: ServerState,
        #[with(3, TEXT2_RESOLVED)] did_change_whole_document: lsp_server::Notification,
    ) {
        let result = populated_state.on_did_change_text_document(did_change_whole_document);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let (_uri, version) = result.unwrap().unwrap();
        assert_eq!(3, version);
        assert_eq!(2, document_state.version);
        assert_eq!(TEXT2_RESOLVED, document_state.content);
        assert!(document_state.conflicts.is_none());
    }

    // tracing_subscriber::fmt::init();

    #[rstest]
    fn change_document_with_no_markers_replaced_with_markers_returns_diagnostics(
        #[with(1, TEXT2_RESOLVED)] populated_state: ServerState,
        #[with(2, TEXT2_WITH_CONFLICTS)] did_change_whole_document: lsp_server::Notification,
    ) {
        let result = populated_state.on_did_change_text_document(did_change_whole_document);
        let (_uri, version) = result.unwrap().unwrap();
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        assert_eq!(2, version);
        assert_eq!(1, document_state.version);
        assert_eq!(TEXT2_WITH_CONFLICTS, document_state.content);
        assert!(document_state.conflicts.is_none());
    }

    #[rstest]
    fn change_document_with_markers_incrementally_changed_outside_of_markers_returns_document_data(
        #[with(1, TEXT2_WITH_CONFLICTS, Some(conflicts_for_text2_with_conflicts()))]
        populated_state: ServerState,
        #[with(
                2,
                 &[insert!(line: 0, character: 0, "!"),
                   insert!(line: 0, character: 1, "\n"),
                   insert!(line: 1, character: 0, "# Just a comment."),
                   insert!(line: 1, character: 17, "\n"),
                   insert!(line: 15, character: 0, "@")
                  ])
            ]
        did_change_incrementally: lsp_server::Notification,
    ) {
        {
            let documents = populated_state.documents.lock().unwrap();
            let document_state = documents.get(&uri()).unwrap();
            assert_eq!(document_state.conflicts.as_ref().unwrap().len(), 2);
        }
        let result = populated_state.on_did_change_text_document(did_change_incrementally);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let (_uri, version) = result.unwrap().unwrap();
        assert_eq!(2, version);
        assert_eq!(1, document_state.version);
        assert_eq!(
            format!("!\n# Just a comment.\n{}@", TEXT2_WITH_CONFLICTS),
            document_state.content
        );
        assert_eq!(
            document_state.conflicts,
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
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let (_uri, version) = result.unwrap().unwrap();
        let new_text = TEXT2_WITH_CONFLICTS.replace("text.", "words!");
        assert_eq!(2, version);
        assert_eq!(1, document_state.version);
        assert_eq!(new_text, document_state.content);
        assert!(document_state.conflicts.is_none());
    }

    #[rstest]
    fn change_document_with_markers_incrementally_changed_using_remove_outside_of_markers_returns_document_data(
        #[with(1, TEXT2_WITH_CONFLICTS, None)] populated_state: ServerState,
        #[with(2, &[remove!(line: 7, character: 0, "text.\n")])]
        did_change_incrementally: lsp_server::Notification,
    ) {
        let result = populated_state.on_did_change_text_document(did_change_incrementally);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri()).unwrap();
        let (_uri, version) = result.unwrap().unwrap();
        let new_text = TEXT2_WITH_CONFLICTS.replace("text.\n", "");
        assert_eq!(2, version);
        assert_eq!(1, document_state.version);
        assert_eq!(new_text, document_state.content);
        assert!(document_state.conflicts.is_none());
    }

    #[rstest]
    fn on_document_update_when_document_without_conflicts_opened_no_notification_sent(
        uri: lsp_types::Uri,
        #[with(2, TEXT2_RESOLVED, None)] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        assert_eq!(3, document_state.version);
        let notification = result.unwrap();
        assert!(notification.is_none());
    }

    #[rstest]
    fn on_document_update_when_document_has_conflicts_previously_but_not_last_generation_changed_no_notification_sent(
        uri: lsp_types::Uri,
        #[with(2, TEXT2_RESOLVED, Some(vec![]))] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        assert_eq!(3, document_state.version);
        assert!(
            document_state.conflicts.is_none(),
            "{:?}",
            document_state.conflicts
        );
        let notification = result.unwrap();
        assert!(notification.is_none());
    }

    #[rstest]
    fn on_document_update_version_missed_no_notification_sent(
        uri: lsp_types::Uri,
        #[with(6, TEXT2_RESOLVED, Some(vec![]))] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        assert_eq!(6, document_state.version);
        assert!(document_state.conflicts.is_some());
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
        assert_eq!(0, document_state.version);
        assert!(document_state.conflicts.is_none());
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
        assert_eq!(0, document_state.version);
        let conflicts = conflicts_for_text2_with_conflicts();
        assert_eq!(Some(conflicts.clone()), document_state.conflicts);
        let notification = result.unwrap().unwrap();
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        let notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(notification.params).unwrap();
        let diagnostics = notification_params.diagnostics;
        assert_eq!(
            conflicts
                .iter()
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
        assert_eq!(3, document_state.version);
        assert_eq!(document_state.conflicts, Some(Vec::new()));
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
        assert_eq!(3, document_state.version);
        assert_eq!(
            Some(conflicts_for_text2_with_conflicts()),
            document_state.conflicts
        );
        let notification = result.unwrap().unwrap();
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        let notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(notification.params).unwrap();
        let diagnostics = notification_params.diagnostics;
        assert_eq!(
            conflicts_for_text2_with_conflicts()
                .iter()
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
        assert_eq!(3, document_state.version);
        let conflicts = vec![
            Conflict::new((3, 5, ""), (5, 7, ""), 7).unwrap(),
            Conflict::new((9, 11, ""), (11, 13, ""), 7).unwrap(),
        ];
        assert_eq!(Some(conflicts.clone()), document_state.conflicts);
        let notification = result.unwrap().unwrap();
        assert_eq!(notification.method, "textDocument/publishDiagnostics");
        let notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(notification.params).unwrap();
        let diagnostics = notification_params.diagnostics;
        assert_eq!(
            conflicts
                .iter()
                .map(lsp_types::Diagnostic::from)
                .collect::<Vec<_>>(),
            diagnostics,
        );
    }
}
