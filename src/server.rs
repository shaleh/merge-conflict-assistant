use std::collections::HashMap;

use crate::parser::{Conflict, Parser};

type LSPResult = anyhow::Result<Option<lsp_server::Notification>>;

pub struct MergeAssistant {
    connection: lsp_server::Connection,
}

#[derive(Default)]
struct DocumentState {
    content: String,
    version: i32,
    conflicts: Vec<Conflict>,
}

#[derive(Default)]
struct ServerState {
    documents: HashMap<lsp_types::Uri, DocumentState>,
}

impl std::fmt::Debug for MergeAssistant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MergeAssistant").finish()
    }
}

impl MergeAssistant {
    pub fn main_loop(connection: lsp_server::Connection) -> LSPResult {
        let mut server = MergeAssistant { connection };
        server.real_main_loop()?;
        log::info!("shutting down server");
        Ok(None)
    }

    fn real_main_loop(&mut self) -> LSPResult {
        let mut state = ServerState::default();

        for msg in &self.connection.receiver {
            log::debug!("got msg: {msg:?}");
            match msg {
                lsp_server::Message::Notification(notification) => {
                    if let Some(reply) = self.on_notification_message(&mut state, notification)? {
                        _ = self.connection.sender.send(reply.into());
                    }
                }
                lsp_server::Message::Request(req) => {
                    if self.connection.handle_shutdown(&req)? {
                        return Ok(None);
                    }
                    self.on_new_request(&state, req)?;
                }
                lsp_server::Message::Response(resp) => {
                    log::debug!("got response: {resp:?}");
                }
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

    fn on_did_open_text_document(
        &self,
        state: &mut ServerState,
        notification: lsp_server::Notification,
    ) -> LSPResult {
        let lsp_types::DidOpenTextDocumentParams { text_document, .. } =
            serde_json::from_value(notification.params)?;
        log::debug!(
            "did open: {:?}: {:?}",
            text_document.uri,
            text_document.text
        );
        let content = text_document.text.clone();
        let conflicts = Parser::parse(&text_document.uri, &content);
        let doc_state = DocumentState {
            content,
            version: text_document.version,
            conflicts: conflicts.clone(),
        };
        state.documents.insert(text_document.uri.clone(), doc_state);
        log::debug!("Conflicts: {:?}", conflicts);
        if conflicts.is_empty() {
            return Ok(None);
        }
        self.prepare_diagnostics(&text_document.uri, text_document.version, &conflicts)
    }

    fn prepare_diagnostics(
        &self,
        uri: &lsp_types::Uri,
        version: i32,
        conflicts: &[Conflict],
    ) -> LSPResult {
        let diagnostics: Vec<lsp_types::Diagnostic> =
            conflicts.iter().map(lsp_types::Diagnostic::from).collect();
        let publish_diagnostics_params = lsp_types::PublishDiagnosticsParams {
            uri: uri.clone(),
            diagnostics,
            version: Some(version),
        };
        let notification = lsp_server::Notification::new(
            <lsp_types::notification::PublishDiagnostics as lsp_types::notification::Notification>::METHOD.to_owned(),
            publish_diagnostics_params,
        );

        Ok(Some(notification))
    }

    fn on_did_change_text_document(
        &self,
        state: &mut ServerState,
        notification: lsp_server::Notification,
    ) -> LSPResult {
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
        if let Some(doc_state) = state.documents.get_mut(&text_document.uri) {
            if doc_state.version > text_document.version {
                log::debug!(
                    "Version skew detected! {} v. {}",
                    doc_state.version,
                    text_document.version
                );
            }
            let previous_conflicts = doc_state.conflicts.clone();
            log::debug!("applying changes");
            apply_changes(&mut doc_state.content, &content_changes);
            doc_state.version = text_document.version;
            log::debug!("parsing updated document");
            let conflicts = Parser::parse(&text_document.uri, &doc_state.content);
            log::debug!("Conflicts: {:?}", conflicts);
            let needs_update = match (previous_conflicts.is_empty(), conflicts.is_empty()) {
                (false, false) => previous_conflicts != conflicts,
                (true, true) => false,
                (true, false) => true,
                (false, true) => true,
            };
            if needs_update {
                doc_state.conflicts = conflicts.clone();
                return self.prepare_diagnostics(
                    &text_document.uri,
                    text_document.version,
                    &conflicts,
                );
            } else {
                log::debug!("Change did not require new diagnostics");
            }
        } else {
            log::debug!("failed to find document: {:?}", text_document.uri);
        }
        Ok(None)
    }

    fn on_did_close_text_document(
        &self,
        state: &mut ServerState,
        notification: lsp_server::Notification,
    ) -> LSPResult {
        let lsp_types::DidCloseTextDocumentParams { text_document, .. } =
            serde_json::from_value(notification.params)?;
        log::debug!("did close: {:?}", text_document.uri);
        if state.documents.remove(&text_document.uri).is_some() {
            log::debug!("Clearing {:?} from list of documents", text_document.uri);
        }
        Ok(None)
    }

    fn on_notification_message(
        &self,
        state: &mut ServerState,
        notification: lsp_server::Notification,
    ) -> LSPResult {
        log::debug!("heard notification {notification:?}");
        match notification.method.as_ref() {
            "textDocument/didOpen" => self.on_did_open_text_document(state, notification),
            "textDocument/didClose" => self.on_did_close_text_document(state, notification),
            "textDocument/didChange" => self.on_did_change_text_document(state, notification),
            unhandled => {
                log::debug!("notification: ignored: {unhandled:?}");
                Ok(None)
            }
        }
    }

    fn on_new_request(&self, _state: &ServerState, req: lsp_server::Request) -> LSPResult {
        log::debug!("got request: {req:?}");

        Ok(None)
    }
}

fn apply_changes(content: &mut String, changes: &[lsp_types::TextDocumentContentChangeEvent]) {
    for change in changes {
        if let Some(range) = change.range {
            let start = index_for_position(&range.start, content);
            let end = index_for_position(&range.end, content);
            if let (Some(start), Some(end)) = (start, end) {
                content.replace_range(start..end, &change.text);
            } else {
                log::debug!("eh?: {start:?} and {end:?}");
                continue;
            }
        } else {
            content.replace_range(.., &change.text);
        }
    }
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

    macro_rules! insert_new_string {
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

    macro_rules! replace_string {
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

    macro_rules! remove_string {
        (line: $line:expr, character: $char:expr, $s:expr) => {
            replace_string!(line: $line, character: $char, old: $s, new: "")
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
        let mut text = "initial text\nline 2\nline 3\nlast line".to_string();
        let range = Range!((0, 0), (0, 1));
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: Some(range),
            range_length: None,
            text: "I".to_string(),
        }];
        apply_changes(&mut text, &changes);
        let expected = "Initial text\nline 2\nline 3\nlast line".to_string();
        assert_eq!(expected, text);
    }

    #[test]
    fn apply_changes_does_delete_character() {
        let mut text = "initial text\nline 12\nline 3\nlast line".to_string();
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: Some(Range!((1, 5), (1, 6))),
            range_length: None,
            text: "".to_string(),
        }];
        apply_changes(&mut text, &changes);
        let expected = "initial text\nline 2\nline 3\nlast line".to_string();
        assert_eq!(expected, text);
    }

    #[test]
    fn apply_changes_does_add_character() {
        let mut text = "initial text\nline 2\nline 3\nlast line".to_string();
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: Some(Range!((1, 5), (1, 5))),
            range_length: None,
            text: "1".to_string(),
        }];
        apply_changes(&mut text, &changes);
        let expected = "initial text\nline 12\nline 3\nlast line".to_string();
        assert_eq!(expected, text);
    }

    #[test]
    fn apply_changes_does_mutate_text() {
        let mut text = "initial text\nline 2\nline 3\nlast line".to_string();

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

        apply_changes(&mut text, &changes);
        let expected = "initial text\nline 122\nline 23\nlast line".to_string();
        assert_eq!(expected, text);
    }

    #[fixture]
    fn uri() -> lsp_types::Uri {
        "file://foo.txt".parse().unwrap()
    }

    #[fixture]
    fn version() -> i32 {
        0
    }

    #[fixture]
    fn server() -> MergeAssistant {
        let (_, reader_receiver) = unbounded::<lsp_server::Message>();
        let (writer_sender, _) = unbounded::<lsp_server::Message>();
        let connection = lsp_server::Connection {
            sender: writer_sender,
            receiver: reader_receiver,
        };
        MergeAssistant { connection }
    }

    #[fixture]
    fn state() -> ServerState {
        ServerState::default()
    }

    #[fixture]
    fn populated_state(
        #[default("")] text: &str,
        #[default(Vec::new())] conflicts: Vec<Conflict>,
    ) -> ServerState {
        let mut state = ServerState::default();
        state.documents.insert(
            uri(),
            DocumentState {
                version: version(),
                content: text.to_string(),
                conflicts,
            },
        );
        state
    }

    #[fixture]
    fn did_open(#[default("")] text: &str) -> lsp_server::Notification {
        let text_document = lsp_types::TextDocumentItem {
            uri: uri().clone(),
            language_id: "".to_string(),
            version: version(),
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
        #[default(1)] version: i32,
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

    static TEXT_1_CONFLICT_RESOLVED: &str = "
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

    #[rstest]
    fn open_document_with_no_markers_returns_no_diagnostics(
        server: MergeAssistant,
        mut state: ServerState,
        #[with(TEXT_1_CONFLICT_RESOLVED)] did_open: lsp_server::Notification,
    ) {
        let result = server.on_did_open_text_document(&mut state, did_open);
        assert!(result.unwrap().is_none());
    }

    #[rstest]
    fn open_document_with_markers_returns_diagnostics(
        server: MergeAssistant,
        mut state: ServerState,
        #[with(TEXT1_WITH_CONFLICTS)] did_open: lsp_server::Notification,
    ) {
        let result = server.on_did_open_text_document(&mut state, did_open);
        // First, validate the document was parsed and added to state.
        let document_state = state.documents.get(&uri()).unwrap();
        assert_eq!(document_state.version, 0);
        assert_eq!(document_state.content, TEXT1_WITH_CONFLICTS);
        assert_eq!(
            vec![
                Conflict::new((2, 4, None), (4, 6, None)),
                Conflict::new((8, 10, None), (10, 12, None)),
            ],
            document_state.conflicts,
        );
        // Second, validate the response.
        let publish_notification = result.unwrap().unwrap();
        assert_eq!(
            publish_notification.method,
            "textDocument/publishDiagnostics"
        );
        let publish_notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(publish_notification.params).unwrap();
        let diagnostics = publish_notification_params.diagnostics;
        assert_eq!(diagnostics.len(), 2);
    }

    #[rstest]
    fn change_document_with_no_markers_returns_no_diagnostics(
        server: MergeAssistant,
        #[with(TEXT2_RESOLVED)] mut populated_state: ServerState,
        #[with(1, TEXT2_RESOLVED)] did_change_whole_document: lsp_server::Notification,
    ) {
        let document_state = populated_state.documents.get(&uri()).unwrap();
        assert_eq!(document_state.version, 0);
        assert_eq!(document_state.content, TEXT2_RESOLVED);
        assert!(document_state.conflicts.is_empty());
        let result =
            server.on_did_change_text_document(&mut populated_state, did_change_whole_document);
        // First, validate the document was parsed and state was updated.
        let document_state = populated_state.documents.get(&uri()).unwrap();
        assert_eq!(document_state.version, 1);
        assert_eq!(document_state.content, TEXT2_RESOLVED);
        assert_eq!(document_state.conflicts.len(), 0);
        // Second, validate the response.
        assert!(result.unwrap().is_none());
    }

    #[rstest]
    fn change_document_with_no_markers_replaced_with_markers_returns_diagnostics(
        server: MergeAssistant,
        #[with(TEXT2_RESOLVED)] mut populated_state: ServerState,
        #[with(1, TEXT2_WITH_CONFLICTS)] did_change_whole_document: lsp_server::Notification,
    ) {
        tracing_subscriber::fmt::init();
        let result =
            server.on_did_change_text_document(&mut populated_state, did_change_whole_document);
        // First, validate the document was parsed and state was updated.
        let document_state = populated_state.documents.get(&uri()).unwrap();
        assert_eq!(document_state.version, 1);
        assert_eq!(document_state.content, TEXT2_WITH_CONFLICTS);
        assert_eq!(
            vec![
                Conflict::new((2, 4, None), (4, 6, None)),
                Conflict::new((8, 10, None), (10, 12, None)),
            ],
            document_state.conflicts,
        );
        // Second, validate the response.
        let publish_notification = result.unwrap().unwrap();
        assert_eq!(
            publish_notification.method,
            "textDocument/publishDiagnostics"
        );
        let publish_notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(publish_notification.params).unwrap();
        let diagnostics = publish_notification_params.diagnostics;
        assert_eq!(diagnostics.len(), 2);
    }

    #[rstest]
    fn change_document_with_markers_incrementally_changed_outside_of_markers_returns_changed_diagnostics(
        server: MergeAssistant,
        #[with(TEXT2_WITH_CONFLICTS,
               vec![Conflict::new((2, 4, None), (4, 6, None)), Conflict::new((8, 10, None), (10, 12, None))])]
        mut populated_state: ServerState,
        #[with(
            1,
             &[insert_new_string!(line: 0, character: 0, "!"),
               insert_new_string!(line: 0, character: 1, "\n"),
               insert_new_string!(line: 1, character: 0, "# Just a comment."),
               insert_new_string!(line: 1, character: 17, "\n"),
               insert_new_string!(line: 15, character: 0, "@")
              ])
        ]
        did_change_incrementally: lsp_server::Notification,
    ) {
        let document_state = populated_state.documents.get(&uri()).unwrap();
        assert_eq!(document_state.conflicts.len(), 2);
        let result =
            server.on_did_change_text_document(&mut populated_state, did_change_incrementally);
        // First, validate the document was parsed and state was updated.
        let document_state = populated_state.documents.get(&uri()).unwrap();
        assert_eq!(document_state.version, 1);
        assert_eq!(
            document_state.content,
            format!("!\n# Just a comment.\n{}@", TEXT2_WITH_CONFLICTS)
        );
        // the original conflicts are still in place.
        assert_eq!(document_state.conflicts.len(), 2);
        // Second, validate the response.
        let publish_notification = result.unwrap().unwrap();
        assert_eq!(
            publish_notification.method,
            "textDocument/publishDiagnostics"
        );
        let publish_notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(publish_notification.params).unwrap();
        let diagnostics = publish_notification_params.diagnostics;
        assert_eq!(
            diagnostics.iter().map(|d| d.range).collect::<Vec<_>>(),
            [Range!((4, 0), (9, 0)), Range!((10, 0), (15, 0))],
        );
    }

    #[rstest]
    fn change_document_with_markers_incrementally_changed_inside_of_markers_returns_changed_diagnostics(
        server: MergeAssistant,
        #[with(TEXT2_WITH_CONFLICTS,
               vec![Conflict::new((2, 4, None), (4, 6, None)), Conflict::new((8, 10, None), (10, 12, None))])]
        mut populated_state: ServerState,
        #[with(
            1,
             &[insert_new_string!(line: 3, character: 0, "!"),
               insert_new_string!(line: 3, character: 1, "\n"),
               insert_new_string!(line: 4, character: 0, "# Just a comment."),
               insert_new_string!(line: 4, character: 17, "\n"),
               replace_string!(line: 5, character: 0, old: "plain", new: "extra"),
               remove_string!(line: 13, character: 4, " stuff"),
               insert_new_string!(line: 13, character: 0, "@")
              ])
        ]
        did_change_incrementally: lsp_server::Notification,
    ) {
        let document_state = populated_state.documents.get(&uri()).unwrap();
        assert_eq!(document_state.conflicts.len(), 2);
        let result =
            server.on_did_change_text_document(&mut populated_state, did_change_incrementally);
        // First, validate the document was parsed and state was updated.
        let document_state = populated_state.documents.get(&uri()).unwrap();
        assert_eq!(document_state.version, 1);
        assert_eq!(
            document_state.content,
            "
This is some
<<<<<<<
!
# Just a comment.
extra old
=======
new and improved
>>>>>>>
text.
<<<<<<<
Nothing to see here.
=======
@Cool.
>>>>>>>
"
        );
        // the original conflicts are still in place.
        assert_eq!(document_state.conflicts.len(), 2);
        // Second, validate the response.
        let publish_notification = result.unwrap().unwrap();
        assert_eq!(
            publish_notification.method,
            "textDocument/publishDiagnostics"
        );
        let publish_notification_params: lsp_types::PublishDiagnosticsParams =
            serde_json::from_value(publish_notification.params).unwrap();
        let diagnostics = publish_notification_params.diagnostics;
        assert_eq!(
            diagnostics.iter().map(|d| d.range).collect::<Vec<_>>(),
            [Range!((2, 0), (9, 0)), Range!((10, 0), (15, 0))],
        );
    }
}
