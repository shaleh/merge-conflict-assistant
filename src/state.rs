use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};

use crossbeam_channel::Sender;
use lsp_textdocument::FullTextDocument;

use crate::{
    parser::{ConflictRegion, MergeConflict, parse, range_for_diagnostic_conflict},
    server::LSPResult,
};

/// A file open in the editor. Tracks the document and any merge conflicts it might have.
#[derive(Debug)]
pub struct DocumentState {
    pub document: FullTextDocument,
    pub merge_conflict: Option<MergeConflict>,
}

impl DocumentState {
    pub fn new(content: String, version: i32) -> Self {
        Self {
            document: FullTextDocument::new(String::new(), version, content),
            merge_conflict: None,
        }
    }

    #[cfg(test)]
    pub fn new_with_conflict(content: String, version: i32, conflict: MergeConflict) -> Self {
        Self {
            document: FullTextDocument::new(String::new(), version, content),
            merge_conflict: Some(conflict),
        }
    }

    pub fn version(&self) -> i32 {
        self.document.version()
    }

    #[cfg(test)]
    pub fn content(&self) -> &str {
        self.document.get_content(None)
    }

    pub fn process_update(&mut self) -> anyhow::Result<Option<MergeConflict>> {
        let content = self.document.get_content(None);

        // Previous / new here refer to the conflicts on the document.
        //
        // previous | new    | action
        // ---------+--------+-------
        // None     | None   | Nothing
        // [data]   | [data] | Nothing
        // [data]   | None   | send empty diagnostics, empty state
        // [data]   | [new]  | send diagnostics, ensure new value in state
        // None     | [new]  | send diagnostics, ensure new value in state

        if !content.contains(crate::parser::MARKER_HEAD) {
            // No conflict marker in new document. Clear out anything that was there previously.
            self.merge_conflict.take();
            return Ok(None);
        }

        let merge_conflict = parse(content)?;

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
                text_document.text,
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
        if locked_doc_state.version() > text_document.version {
            tracing::debug!(
                "Version skew detected! {} v. {}",
                locked_doc_state.version(),
                text_document.version
            );
        }
        tracing::debug!("applying changes");
        locked_doc_state
            .document
            .update(&content_changes, text_document.version);
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
            &locked_document_state.document,
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

        if version >= locked_doc_state.version() {
            // Update version via a no-op change to keep FullTextDocument in sync.
            locked_doc_state.document.update(&[], version);
        } else {
            tracing::debug!("Missed update, skipping.");
            return Ok(None);
        }

        let _span = tracing::debug_span!("parse", ?uri).entered();
        locked_doc_state.process_update()
    }
}

fn conflict_as_code_actions(
    region: &ConflictRegion,
    uri: &lsp_types::Uri,
    document: &FullTextDocument,
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

    let mut items = vec![
        {
            let edit = make_text_edit(document, range, &[region.head_range()]);
            make_code_action(
                as_string_with_default!("Keep {}", current_conflict.head, "HEAD"),
                uri,
                edit,
                diagnostic.clone(),
            )
        },
        {
            let edit = make_text_edit(document, range, &[region.branch_range()]);
            make_code_action(
                as_string_with_default!("Keep {}", current_conflict.branch, "branch"),
                uri,
                edit,
                diagnostic.clone(),
            )
        },
        {
            let edit = make_text_edit(
                document,
                range,
                &[region.head_range(), region.branch_range()],
            );
            make_code_action("Keep both".to_string(), uri, edit, diagnostic.clone())
        },
    ];

    if let Some(ancestor_range) = region.ancestor_range() {
        let edit = make_text_edit(document, range, &[ancestor_range]);
        items.push(make_code_action(
            as_string_with_default!("Keep {}", current_conflict.ancestor, "ancestor"),
            uri,
            edit,
            diagnostic.clone(),
        ));
    }

    let edit = make_text_edit(document, range, &[]);
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

fn make_text_edit(
    document: &FullTextDocument,
    range: lsp_types::Range,
    kept_regions: &[(u32, u32)],
) -> lsp_types::TextEdit {
    let content = document.get_content(None);
    let mut lines: Vec<&str> = Vec::with_capacity(kept_regions.len());
    for (start, end) in kept_regions {
        let start = document.offset_at(lsp_types::Position {
            // start is the marker, we want the content. Move down one line.
            line: start + 1,
            character: 0,
        }) as usize;
        let end = document.offset_at(lsp_types::Position {
            line: *end,
            character: 0,
        }) as usize;
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

    #[fixture]
    fn uri() -> lsp_types::Uri {
        "file://foo.txt".parse().unwrap()
    }

    #[fixture]
    fn version(#[default(0)] value: i32) -> i32 {
        value
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
        assert_eq!(3, locked_document_state.version());
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
        assert_eq!(3, locked_document_state.version());
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
        assert_eq!(6, locked_document_state.version());
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
        assert_eq!(0, locked_document_state.version());
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
        assert_eq!(0, locked_document_state.version());
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
        assert_eq!(3, locked_document_state.version());
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
        assert_eq!(3, locked_document_state.version());
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
        assert_eq!(3, locked_document_state.version());
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
