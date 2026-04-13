use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crossbeam_channel::Sender;

use crate::{
    parser::{ConflictRegion, MergeConflict, parse, range_for_diagnostic_conflict},
    server::LSPResult,
};

#[derive(Clone, Default, Debug)]
pub struct DocumentState {
    pub content: String,
    pub version: i32,
    pub merge_conflict: Option<MergeConflict>,
}

impl DocumentState {
    pub fn new(content: String, version: i32) -> Self {
        Self {
            content,
            version,
            merge_conflict: None,
        }
    }

    #[allow(unused)]
    pub fn new_with_conflict(content: String, version: i32, conflict: MergeConflict) -> Self {
        Self {
            content,
            version,
            merge_conflict: Some(conflict),
        }
    }

    pub fn apply_changes(&mut self, changes: &[lsp_types::TextDocumentContentChangeEvent]) {
        let (full_replacements, mut ranged): (Vec<_>, Vec<_>) =
            changes.iter().partition(|c| c.range.is_none());

        for change in &full_replacements {
            self.content.replace_range(.., &change.text);
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
            let start = index_for_position(&range.start, &self.content);
            let end = index_for_position(&range.end, &self.content);
            if let (Some(start), Some(end)) = (start, end) {
                self.content.replace_range(start..end, &change.text);
            } else {
                tracing::warn!("Failed to map range to byte indices: start={start:?}, end={end:?}");
            }
        }
    }

    pub fn process_update(&mut self) -> anyhow::Result<Option<MergeConflict>> {
        if !self.content.contains(crate::parser::MARKER_HEAD) {
            // No conflict marker in new document. Clear out anything that was there previously.
            if self.merge_conflict.is_some() {
                self.merge_conflict.take();
            }
            return Ok(None);
        }

        let merge_conflict = parse(&self.content)?;

        // doc_state has the previous conflicts.
        //
        // previous | new    | action
        // ---------+--------+-------
        // None     | None   | Nothing
        // [data]   | [data] | Nothing
        // [data]   | None   | send empty diagnostics, empty state
        // [data]   | [new]  | send diagnostics, ensure new value in state
        // None     | [new]  | send diagnostics, ensure new value in state
        match (self.merge_conflict.as_ref(), merge_conflict.as_ref()) {
            (None, None) => {
                tracing::debug!("No current or previous, nothing to do.");
            }
            (Some(previous), Some(current)) if previous == current => {
                tracing::debug!("Change did not require new diagnostics");
            }
            _ => {
                tracing::debug!("needs update");
                if let Some(current_conflict) = merge_conflict {
                    self.merge_conflict.replace(current_conflict);
                } else {
                    self.merge_conflict.take();
                }
                return Ok(self.merge_conflict.clone());
            }
        }

        Ok(None)
    }
}

fn index_for_position(position: &lsp_types::Position, value: &str) -> Option<usize> {
    let offsets = build_line_offsets(value);
    index_for_position_with_offsets(position, &offsets, value.len())
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ServerStatus {
    Running,
    ShutdownRequested,
    ExitReceived,
}

#[derive(Clone, Debug)]
pub struct ServerState {
    pub status: ServerStatus,
    pub sender: Arc<Mutex<crossbeam_channel::Sender<lsp_server::Message>>>,
    pub documents: Arc<Mutex<HashMap<lsp_types::Uri, Arc<Mutex<DocumentState>>>>>,
}

impl ServerState {
    pub fn new(sender: Sender<lsp_server::Message>) -> Self {
        Self {
            status: ServerStatus::Running,
            sender: Arc::new(Mutex::new(sender)),
            documents: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn add_document(&self, text_document: lsp_types::TextDocumentItem) -> LSPResult {
        tracing::debug!("content: {:?}", text_document.text);
        let mut documents = self.documents.lock().map_err(|e| {
            tracing::error!("poisoned mutex: {e}");
            anyhow::anyhow!("poisoned mutex: {e}")
        })?;
        // Always insert. Even if there was a previous version, didOpen means a new version of the file opened.
        documents.insert(
            text_document.uri.clone(),
            Arc::new(Mutex::new(DocumentState::new(
                text_document.text.clone(),
                text_document.version,
            ))),
        );
        Ok(Some((text_document.uri, text_document.version)))
    }

    pub fn document_did_change(
        &self,
        text_document: lsp_types::VersionedTextDocumentIdentifier,
        content_changes: Vec<lsp_types::TextDocumentContentChangeEvent>,
    ) -> LSPResult {
        tracing::debug!("content changes: {:?}", content_changes);

        let doc_state = {
            let mut documents = self.documents.lock().map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
            let Some(doc_state) = documents.get_mut(&text_document.uri) else {
                tracing::debug!("failed to find document: {:?}", text_document.uri);
                return Ok(None);
            };
            Arc::clone(doc_state)
        };
        let mut locked_doc_state = doc_state.lock().map_err(|e| {
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
        locked_doc_state.apply_changes(&content_changes);
        Ok(Some((text_document.uri.clone(), text_document.version)))
    }

    pub fn remove_document(&self, text_document: lsp_types::TextDocumentIdentifier) -> LSPResult {
        let mut documents = self.documents.lock().map_err(|e| {
            tracing::error!("poisoned mutex: {e}");
            anyhow::anyhow!("poisoned mutex: {e}")
        })?;
        if documents.remove(&text_document.uri).is_some() {
            tracing::debug!("Clearing {:?} from list of documents", text_document.uri);
        }
        Ok(None)
    }

    pub fn code_action(
        &self,
        params: lsp_types::CodeActionParams,
    ) -> anyhow::Result<Vec<lsp_types::CodeAction>> {
        let document_state = {
            let documents = self.documents.lock().map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
            let Some(document_state) = documents.get(&params.text_document.uri) else {
                tracing::debug!("{:?} not found", params.text_document.uri);
                return Ok(Vec::new());
            };
            Arc::clone(document_state)
        };

        let locked_document_state = document_state.lock().map_err(|e| {
            tracing::error!("poisoned mutex: {e}");
            anyhow::anyhow!("poisoned mutex: {e}")
        })?;
        let Some(merge_conflict) = locked_document_state.merge_conflict.as_ref() else {
            return Ok(Vec::new());
        };
        let Some(conflict) = merge_conflict
            .conflicts()
            .find(|conflict| conflict.is_in_range(&params.range))
        else {
            return Ok(Vec::new());
        };
        let actions = conflict_as_code_actions(
            conflict,
            &params.text_document.uri,
            &locked_document_state.content,
            &locked_document_state.merge_conflict,
        );
        Ok(actions)
    }

    pub fn on_document_update(
        &self,
        uri: &lsp_types::Uri,
        version: i32,
    ) -> anyhow::Result<Option<MergeConflict>> {
        let doc_state = {
            let documents = self.documents.lock().map_err(|e| {
                tracing::error!("poisoned mutex: {e}");
                anyhow::anyhow!("poisoned mutex: {e}")
            })?;
            let Some(doc_state) = documents.get(uri) else {
                tracing::debug!("No entry to {uri:?}");
                return Ok(None);
            };
            Arc::clone(doc_state)
        };

        let mut locked_doc_state = doc_state.lock().map_err(|e| {
            tracing::error!("poisoned mutex: {e}");
            anyhow::anyhow!("poisoned mutex: {e}")
        })?;

        if version >= locked_doc_state.version {
            locked_doc_state.version = version;
        } else {
            tracing::debug!("Missed update, skipping.");
            return Ok(None);
        }

        let _span = tracing::debug_span!("parse", ?uri).entered();
        locked_doc_state.process_update()
    }
}

// Build a vector of the locations of newlines in the provided text.
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

fn conflict_as_code_actions(
    region: &ConflictRegion,
    uri: &lsp_types::Uri,
    content: &str,
    merge_conflict: &Option<MergeConflict>,
) -> Vec<lsp_types::CodeAction> {
    macro_rules! as_string_with_default {
        ($s:expr, $option:expr, $default:expr) => {
            match $option.as_ref() {
                Some(value) => format!($s, value),
                None => format!($s, $default),
            }
        };
    }

    let diagnostic = lsp_types::Diagnostic::from(region);
    let range = range_for_diagnostic_conflict(region);

    let current_conflict = merge_conflict
        .as_ref()
        .expect("valid merge conflict reference");
    let offsets = build_line_offsets(content);

    let mut items = vec![
        {
            let edit = make_text_edit(&offsets, content, range, &[region.head_range()]);
            make_code_action(
                as_string_with_default!("Keep {}", current_conflict.head, "HEAD"),
                uri,
                edit,
                diagnostic.clone(),
            )
        },
        {
            let edit = make_text_edit(&offsets, content, range, &[region.branch_range()]);
            make_code_action(
                as_string_with_default!("Keep {}", current_conflict.branch, "branch"),
                uri,
                edit,
                diagnostic.clone(),
            )
        },
        {
            let edit = make_text_edit(
                &offsets,
                content,
                range,
                &[region.head_range(), region.branch_range()],
            );
            make_code_action("Keep both".to_string(), uri, edit, diagnostic.clone())
        },
    ];

    if let Some(ancestor_range) = region.ancestor_range() {
        let edit = make_text_edit(&offsets, content, range, &[ancestor_range]);
        items.push(make_code_action(
            as_string_with_default!("Keep {}", current_conflict.ancestor, "ancestor"),
            uri,
            edit,
            diagnostic.clone(),
        ));
    }

    let edit = make_text_edit(&offsets, content, range, &[]);
    // Always the last item.
    items.push(make_code_action(
        "Drop all".to_string(),
        uri,
        edit,
        diagnostic.clone(),
    ));

    tracing::info!(
        "offering {} code action(s) for conflict at lines {}-{} in {:?}",
        items.len(),
        region.head,
        region.end,
        uri,
    );
    items
}

// Transform Line number + character position into the position without the file.
fn index_for_position_with_offsets(
    position: &lsp_types::Position,
    offsets: &[usize],
    text_len: usize,
) -> Option<usize> {
    let line_start = *offsets.get(position.line as usize)?;
    let index = line_start + position.character as usize;
    if index > text_len {
        return None;
    }
    Some(index)
}

fn make_text_edit(
    offsets: &[usize],
    content: &str,
    range: lsp_types::Range,
    kept_regions: &[(u32, u32)],
) -> lsp_types::TextEdit {
    let text_len = content.len();
    let mut lines: Vec<&str> = Vec::with_capacity(kept_regions.len());
    for (start, end) in kept_regions {
        let start = index_for_position_with_offsets(
            &lsp_types::Position {
                // start is the marker, we want the content. Move down one line.
                line: start + 1,
                character: 0,
            },
            offsets,
            text_len,
        )
        .expect("valid index for start position");
        let end = index_for_position_with_offsets(
            &lsp_types::Position {
                line: *end,
                character: 0,
            },
            offsets,
            text_len,
        )
        .expect("valid index for end position");
        lines.push(&content[start..end]);
    }
    let new_text = lines.join("");
    lsp_types::TextEdit { range, new_text }
}

fn make_code_action(
    title: String,
    uri: &lsp_types::Uri,
    edit: lsp_types::TextEdit,
    diagnostic: lsp_types::Diagnostic,
) -> lsp_types::CodeAction {
    let is_preferred = None;
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

#[cfg(test)]
mod test {
    use rstest::*;

    use crate::test_helpers::{
        TEXT2_RESOLVED, TEXT2_WITH_CONFLICTS, conflicts_for_text2_with_conflicts, populated_state,
    };

    use super::*;

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

    #[fixture]
    fn uri() -> lsp_types::Uri {
        "file://foo.txt".parse().unwrap()
    }

    #[fixture]
    fn version(#[default(0)] value: i32) -> i32 {
        value
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
    fn apply_changes_replaces_character() {
        let mut doc = DocumentState::new(
            "initial text\nline 2\nline 3\nlast line".to_string(),
            0,
        );
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: Some(Range!((0, 0), (0, 1))),
            range_length: None,
            text: "I".to_string(),
        }];
        doc.apply_changes(&changes);
        assert_eq!("Initial text\nline 2\nline 3\nlast line", doc.content);
    }

    #[test]
    fn apply_changes_deletes_character() {
        let mut doc = DocumentState::new(
            "initial text\nline 12\nline 3\nlast line".to_string(),
            0,
        );
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: Some(Range!((1, 5), (1, 6))),
            range_length: None,
            text: "".to_string(),
        }];
        doc.apply_changes(&changes);
        assert_eq!("initial text\nline 2\nline 3\nlast line", doc.content);
    }

    #[test]
    fn apply_changes_inserts_character() {
        let mut doc = DocumentState::new(
            "initial text\nline 2\nline 3\nlast line".to_string(),
            0,
        );
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: Some(Range!((1, 5), (1, 5))),
            range_length: None,
            text: "1".to_string(),
        }];
        doc.apply_changes(&changes);
        assert_eq!("initial text\nline 12\nline 3\nlast line", doc.content);
    }

    #[test]
    fn apply_changes_full_replacement() {
        let mut doc = DocumentState::new("old content".to_string(), 0);
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "new content".to_string(),
        }];
        doc.apply_changes(&changes);
        assert_eq!("new content", doc.content);
    }

    #[test]
    fn apply_changes_ascending_with_line_shift() {
        // First change inserts a newline, shifting all subsequent lines down.
        // Second change targets a line using post-shift positions.
        // This validates that byte offsets are recalculated between ascending edits.
        let mut doc = DocumentState::new("aa\nbb\ncc\n".to_string(), 0);
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
        doc.apply_changes(&changes);
        assert_eq!("xx\naa\nBB\ncc\n", doc.content);
    }

    #[test]
    fn apply_changes_bottom_to_top() {
        // Changes arrive with highest position first, as most editors send them.
        let mut doc = DocumentState::new(
            "line 1\nline 2\nline 3\n".to_string(),
            0,
        );
        let changes = [
            // Edit on line 2 first (higher line number)
            lsp_types::TextDocumentContentChangeEvent {
                range: Some(Range!((2, 0), (2, 6))),
                range_length: None,
                text: "LINE 3".to_string(),
            },
            // Then line 0 (lower line number)
            lsp_types::TextDocumentContentChangeEvent {
                range: Some(Range!((0, 0), (0, 6))),
                range_length: None,
                text: "LINE 1".to_string(),
            },
        ];
        doc.apply_changes(&changes);
        assert_eq!("LINE 1\nline 2\nLINE 3\n", doc.content);
    }

    #[test]
    fn apply_changes_multiple_ascending_edits() {
        let mut doc = DocumentState::new(
            "initial text\nline 2\nline 3\nlast line".to_string(),
            0,
        );
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
        doc.apply_changes(&changes);
        assert_eq!(
            "initial text\nline 122\nline 23\nlast line",
            doc.content,
        );
    }

    #[test]
    fn apply_changes_does_not_alter_version() {
        let mut doc = DocumentState::new("text".to_string(), 5);
        let changes = [lsp_types::TextDocumentContentChangeEvent {
            range: None,
            range_length: None,
            text: "new text".to_string(),
        }];
        doc.apply_changes(&changes);
        assert_eq!(5, doc.version);
    }

    #[rstest]
    fn on_document_update_when_document_without_conflicts_opened_no_conflicts_returned(
        uri: lsp_types::Uri,
        #[with(2, TEXT2_RESOLVED, None)] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(3, locked_document_state.version);
        let conflict = result.unwrap();
        assert!(conflict.is_none());
    }

    #[rstest]
    fn on_document_update_when_document_has_conflicts_previously_but_not_last_generation_changed_no_conflicts_returned(
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
        let conflict = result.unwrap();
        assert!(conflict.is_none());
    }

    #[rstest]
    fn on_document_update_version_missed_no_conflicts_returned(
        uri: lsp_types::Uri,
        #[with(6, TEXT2_RESOLVED)] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 3);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(6, locked_document_state.version);
        assert!(locked_document_state.merge_conflict.is_none());
        let conflict = result.unwrap();
        assert!(conflict.is_none());
    }

    #[rstest]
    fn on_document_update_version_initial_version_with_no_conflicts_no_conflicts_returned(
        uri: lsp_types::Uri,
        #[with(0, TEXT2_RESOLVED, None)] populated_state: ServerState,
    ) {
        let result = populated_state.on_document_update(&uri, 0);
        let documents = populated_state.documents.lock().unwrap();
        let document_state = documents.get(&uri).unwrap();
        let locked_document_state = document_state.lock().expect("poisoned mutex: {e}");
        assert_eq!(0, locked_document_state.version);
        assert!(locked_document_state.merge_conflict.is_none());
        let conflict = result.unwrap();
        assert!(conflict.is_none());
    }

    #[rstest]
    fn on_document_update_version_initial_version_with_conflicts_returns_conflicts(
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
        let conflict = result.unwrap().unwrap();
        assert_eq!(merge_conflict, conflict);
    }

    #[rstest]
    fn on_document_update_when_document_has_conflicts_previously_and_is_resolved_returns_no_conflicts(
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
        let conflict = result.unwrap();
        assert!(conflict.is_none());
    }

    #[rstest]
    fn on_document_update_when_document_has_conflicts_returns_conflicts(
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
        let conflict = result.unwrap().unwrap();
        assert_eq!(conflicts_for_text2_with_conflicts(), conflict);
    }

    #[rstest]
    fn on_document_update_when_document_has_conflicts_and_change_affecting_them_updated_returns_conflicts(
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
        let conflict = result.unwrap().unwrap();
        assert_eq!(merge_conflict, conflict);
    }
}
