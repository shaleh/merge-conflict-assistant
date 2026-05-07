//! Support for diff2 or diff3 style conflicts.
// The labelling is "diff3" but both diff2 and diff3 style are supported.

use crate::parser::{
    FileLabels, ParseError, MARKER_ANCESTOR, MARKER_END, MARKER_SEPARATOR, strip_marker,
};

/// A single conflict region within a file.
///
/// Each field holds the 0-based line number of the corresponding marker.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictRegion {
    pub head: u32,
    pub branch: u32,
    pub ancestor: Option<u32>,
    pub end: u32,
}

impl ConflictRegion {
    fn head_range(&self) -> (u32, u32) {
        let end = self.ancestor.unwrap_or(self.branch);
        (self.head, end)
    }

    fn branch_range(&self) -> (u32, u32) {
        (self.branch, self.end)
    }

    fn ancestor_range(&self) -> Option<(u32, u32)> {
        self.ancestor.map(|pos| (pos, self.branch))
    }

    /// Returns true if the given LSP range overlaps with this conflict.
    ///
    /// The range must start within the conflict region. A range that begins
    /// before the conflict is rejected — this avoids matching when the user
    /// has selected across multiple conflicts or only part of one.
    fn is_in_range(&self, range: &lsp_types::Range) -> bool {
        tracing::debug!(
            "is_in_range: range: {:?}, head: {}, end: {}",
            range,
            self.head,
            self.end
        );
        self.head <= range.start.line
            && self.end >= range.start.line
            && self.end + 1 >= range.end.line
    }
}

/// Parse result for a diff3-style merge conflict document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcsInfo {
    pub head: Option<String>,
    pub branch: Option<String>,
    pub ancestor: Option<String>,
    pub conflicts: Vec<ConflictRegion>,
}

impl VcsInfo {
    pub fn conflicts(&self) -> impl Iterator<Item = &ConflictRegion> {
        self.conflicts.iter()
    }

    fn find_region_at(&self, range: &lsp_types::Range) -> Option<&ConflictRegion> {
        self.conflicts.iter().find(|c| c.is_in_range(range))
    }

    pub fn code_actions_at(
        &self,
        range: &lsp_types::Range,
        uri: &lsp_types::Uri,
        document: &lsp_textdocument::FullTextDocument,
    ) -> Vec<lsp_types::CodeAction> {
        macro_rules! as_string_with_default {
            ($s:expr, $option:expr, $default:expr) => {
                match $option.as_ref() {
                    Some(value) => format!($s, value),
                    None => format!($s, $default),
                }
            };
        }

        let Some(region) = self.find_region_at(range) else {
            return Vec::new();
        };

        let diagnostic = lsp_types::Diagnostic::from(region);
        let conflict_range = range_for_diagnostic_conflict(region);

        let mut items = vec![
            {
                let edit =
                    crate::state::make_text_edit(document, conflict_range, &[region.head_range()]);
                crate::state::make_code_action(
                    as_string_with_default!("Keep {}", self.head, "HEAD"),
                    uri,
                    edit,
                    diagnostic.clone(),
                )
            },
            {
                let edit = crate::state::make_text_edit(
                    document,
                    conflict_range,
                    &[region.branch_range()],
                );
                crate::state::make_code_action(
                    as_string_with_default!("Keep {}", self.branch, "branch"),
                    uri,
                    edit,
                    diagnostic.clone(),
                )
            },
            {
                let edit = crate::state::make_text_edit(
                    document,
                    conflict_range,
                    &[region.head_range(), region.branch_range()],
                );
                crate::state::make_code_action(
                    "Keep both".to_string(),
                    uri,
                    edit,
                    diagnostic.clone(),
                )
            },
        ];

        if let Some(ancestor_range) = region.ancestor_range() {
            let edit = crate::state::make_text_edit(document, conflict_range, &[ancestor_range]);
            items.push(crate::state::make_code_action(
                as_string_with_default!("Keep {}", self.ancestor, "ancestor"),
                uri,
                edit,
                diagnostic.clone(),
            ));
        }

        let edit = crate::state::make_text_edit(document, conflict_range, &[]);
        items.push(crate::state::make_code_action(
            "Drop all".to_string(),
            uri,
            edit,
            diagnostic.clone(),
        ));

        if self.conflicts.len() >= 2 {
            items.push(make_bulk_action(
                as_string_with_default!("Keep {} in remaining conflicts", self.head, "HEAD"),
                uri,
                document,
                &self.conflicts,
                |region| region.head_range(),
                diagnostic.clone(),
            ));
            items.push(make_bulk_action(
                as_string_with_default!("Keep {} in remaining conflicts", self.branch, "branch"),
                uri,
                document,
                &self.conflicts,
                |region| region.branch_range(),
                diagnostic.clone(),
            ));
        }

        tracing::info!(
            "offering {} code action(s) for conflict at lines {}-{} in {:?}",
            items.len(),
            region.head,
            region.end,
            uri,
        );
        items
    }
}

fn make_bulk_action(
    title: String,
    uri: &lsp_types::Uri,
    document: &lsp_textdocument::FullTextDocument,
    regions: &[ConflictRegion],
    keep: impl Fn(&ConflictRegion) -> (u32, u32),
    diagnostic: lsp_types::Diagnostic,
) -> lsp_types::CodeAction {
    // Edits are emitted in reverse document order so that clients applying
    // them as a sequence of incremental changes don't see earlier line
    // numbers shifted by later edits.
    let edits: Vec<lsp_types::TextEdit> = regions
        .iter()
        .rev()
        .map(|region| {
            let region_range = range_for_diagnostic_conflict(region);
            crate::state::make_text_edit(document, region_range, &[keep(region)])
        })
        .collect();
    lsp_types::CodeAction {
        title,
        kind: Some(lsp_types::CodeActionKind::QUICKFIX),
        diagnostics: Some(vec![diagnostic]),
        edit: Some(lsp_types::WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([(uri.clone(), edits)])),
            ..Default::default()
        }),
        is_preferred: None,
        ..Default::default()
    }
}

/// Build the LSP range covering the entire conflict, including the end marker line.
///
/// The range extends to `end + 1` so that applying a replacement removes the
/// trailing newline of the end marker rather than leaving a blank line behind.
fn range_for_diagnostic_conflict(conflict: &ConflictRegion) -> lsp_types::Range {
    let start = lsp_types::Position {
        line: conflict.head,
        character: 0,
    };
    let end = lsp_types::Position {
        // This is a product of the code action not wanting to leave a dangling new line behind.
        line: conflict.end + 1,
        character: 0,
    };
    lsp_types::Range { start, end }
}

impl From<&ConflictRegion> for lsp_types::Diagnostic {
    fn from(conflict: &ConflictRegion) -> Self {
        let range = range_for_diagnostic_conflict(conflict);
        let message = "merge conflict";
        let source = "merge";
        Self {
            range,
            message: message.to_owned(),
            source: Some(source.to_owned()),
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            ..Default::default()
        }
    }
}

#[derive(Debug)]
enum Diff3State {
    End(u32, u32),
    BranchFromAncestor(u32, u32),
    EndWithAncestor(u32, u32, u32),
}

/// Parse one diff3-style conflict block starting at `head_index`.
///
/// `head_index` is the 0-based line index of the `<<<<<<<` marker.
/// `file_labels` accumulates the first-seen head/branch/ancestor labels across all regions.
///
/// Returns `(region, next_index)` where `next_index` is the line just past the `>>>>>>>`.
pub fn parse_block(
    lines: &[&str],
    head_index: usize,
    file_labels: &mut FileLabels,
) -> Result<(ConflictRegion, usize), ParseError> {
    let head: u32 = head_index.try_into()?;
    let mut state = None::<Diff3State>;

    for (offset, line) in lines[head_index + 1..].iter().enumerate() {
        let lineno = head_index + 1 + offset;
        let first = line.as_bytes().first();

        match &state {
            None => {
                if first == Some(&b'|') && let Some(name) = strip_marker(line, MARKER_ANCESTOR) {
                    let ancestor: u32 = lineno.try_into()?;
                    if !name.is_empty() && file_labels.ancestor.is_none() {
                        file_labels.ancestor = Some(name.to_string());
                    }
                    state = Some(Diff3State::BranchFromAncestor(head, ancestor));
                } else if first == Some(&b'=') && *line == MARKER_SEPARATOR {
                    let branch: u32 = lineno.try_into()?;
                    state = Some(Diff3State::End(head, branch));
                }
            }
            Some(Diff3State::End(h, branch)) => {
                let (h, branch) = (*h, *branch);
                if first == Some(&b'>') && let Some(name) = strip_marker(line, MARKER_END) {
                    if !name.is_empty() && file_labels.branch.is_none() {
                        file_labels.branch = Some(name.to_string());
                    }
                    let region = ConflictRegion {
                        head: h,
                        branch,
                        ancestor: None,
                        end: lineno.try_into()?,
                    };
                    return Ok((region, lineno + 1));
                }
            }
            Some(Diff3State::BranchFromAncestor(h, ancestor)) => {
                let (h, ancestor) = (*h, *ancestor);
                if first == Some(&b'=') && *line == MARKER_SEPARATOR {
                    let branch: u32 = lineno.try_into()?;
                    state = Some(Diff3State::EndWithAncestor(h, ancestor, branch));
                }
            }
            Some(Diff3State::EndWithAncestor(h, ancestor, branch)) => {
                let (h, ancestor, branch) = (*h, *ancestor, *branch);
                if first == Some(&b'>') && let Some(name) = strip_marker(line, MARKER_END) {
                    if !name.is_empty() && file_labels.branch.is_none() {
                        file_labels.branch = Some(name.to_string());
                    }
                    let region = ConflictRegion {
                        head: h,
                        branch,
                        ancestor: Some(ancestor),
                        end: lineno.try_into()?,
                    };
                    return Ok((region, lineno + 1));
                }
            }
        }
    }

    Err(ParseError::Incomplete {
        state: format!("{:?}", state.unwrap_or(Diff3State::End(head, head))),
    })
}

#[cfg(test)]
mod tests {
    use rstest::*;

    use super::*;

    #[fixture]
    fn conflict() -> ConflictRegion {
        ConflictRegion {
            head: 4,
            branch: 10,
            ancestor: Some(6),
            end: 12,
        }
    }

    #[rstest]
    fn range_one_line_is_in_conflict(conflict: ConflictRegion) {
        for x in conflict.head..=conflict.end {
            let range = lsp_types::Range {
                start: lsp_types::Position {
                    line: x,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: x,
                    character: 1,
                },
            };
            assert!(conflict.is_in_range(&range), "{range:?}");
        }
    }

    #[rstest]
    fn range_one_line_is_not_in_conflict(conflict: ConflictRegion) {
        for x in [conflict.head - 1, conflict.end + 1] {
            let range = lsp_types::Range {
                start: lsp_types::Position {
                    line: x,
                    character: 0,
                },
                end: lsp_types::Position {
                    line: x,
                    character: 1,
                },
            };
            assert!(!conflict.is_in_range(&range), "{range:?}");
        }
    }

    #[rstest]
    fn range_matching_conflict_is_in_conflict(conflict: ConflictRegion) {
        let range = range_for_diagnostic_conflict(&conflict);
        assert!(conflict.is_in_range(&range), "{conflict:?} v. {range:?}");
    }

    #[rstest]
    fn range_wider_than_conflict_is_not_in_conflict(conflict: ConflictRegion) {
        let range = lsp_types::Range {
            start: lsp_types::Position {
                line: conflict.head - 3,
                character: 0,
            },
            end: lsp_types::Position {
                line: conflict.end + 3,
                character: 1,
            },
        };
        assert!(!conflict.is_in_range(&range), "{range:?}");
    }

    use crate::parser::{MergeConflict, parse};
    use lsp_textdocument::FullTextDocument;

    const TEXT_3_CONFLICTS: &str = concat!(
        "before\n",
        crate::conflict_text!("h1", "b1"),
        "between1\n",
        crate::diff3_conflict_text!("h2", "anc2", "b2"),
        "between2\n",
        crate::conflict_text!("h3", "b3"),
        "after\n",
    );

    const TEXT_1_CONFLICT: &str = concat!(
        "before\n",
        crate::conflict_text!("only-head", "only-branch"),
        "after\n",
    );

    const TEXT_3_CONFLICTS_LABELLED: &str = concat!(
        "before\n",
        crate::conflict_text!("main", "h1", "feature", "b1"),
        "between1\n",
        crate::conflict_text!("main", "h2", "feature", "b2"),
        "between2\n",
        crate::conflict_text!("main", "h3", "feature", "b3"),
        "after\n",
    );

    fn parse_diff3(text: &str) -> VcsInfo {
        match parse(text).expect("parse ok").expect("has conflicts") {
            MergeConflict::Diff3(info) => info,
            other => panic!("expected diff3, got {other:?}"),
        }
    }

    fn range_at(line: u32) -> lsp_types::Range {
        lsp_types::Range {
            start: lsp_types::Position { line, character: 0 },
            end: lsp_types::Position {
                line,
                character: 1,
            },
        }
    }

    fn uri() -> lsp_types::Uri {
        "file://test.txt".parse().unwrap()
    }

    fn document(text: &str) -> FullTextDocument {
        FullTextDocument::new(String::new(), 0, text.to_string())
    }

    fn extract_edits(action: &lsp_types::CodeAction) -> Vec<lsp_types::TextEdit> {
        #[allow(clippy::mutable_key_type)]
        let changes = action
            .edit
            .as_ref()
            .expect("edit")
            .changes
            .as_ref()
            .expect("changes");
        changes.values().next().expect("entry").clone()
    }

    fn apply_edits(text: &str, edits: &[lsp_types::TextEdit]) -> String {
        let mut doc = document(text);
        let changes: Vec<lsp_types::TextDocumentContentChangeEvent> = edits
            .iter()
            .map(|e| lsp_types::TextDocumentContentChangeEvent {
                range: Some(e.range),
                range_length: None,
                text: e.new_text.clone(),
            })
            .collect();
        doc.update(&changes, 1);
        doc.get_content(None).to_string()
    }

    #[test]
    fn bulk_actions_appear_after_per_site_actions_when_file_has_multiple_conflicts() {
        let info = parse_diff3(TEXT_3_CONFLICTS);
        assert_eq!(info.conflicts.len(), 3);
        let doc = document(TEXT_3_CONFLICTS);

        let actions = info.code_actions_at(&range_at(2), &uri(), &doc);

        let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();
        let bulk_idx = titles
            .iter()
            .position(|t| t.ends_with(" in remaining conflicts"))
            .expect("at least one bulk action");
        let bulk_titles: Vec<&str> = titles[bulk_idx..].to_vec();
        assert_eq!(
            bulk_titles,
            vec![
                "Keep HEAD in remaining conflicts",
                "Keep branch in remaining conflicts",
            ]
        );
        assert!(
            titles[..bulk_idx]
                .iter()
                .all(|t| !t.ends_with(" in remaining conflicts")),
            "bulk actions must come after per-site, got titles: {titles:?}",
        );
    }

    #[test]
    fn single_conflict_file_does_not_offer_bulk_actions() {
        let info = parse_diff3(TEXT_1_CONFLICT);
        assert_eq!(info.conflicts.len(), 1);
        let doc = document(TEXT_1_CONFLICT);

        let actions = info.code_actions_at(&range_at(2), &uri(), &doc);

        assert!(
            !actions.is_empty(),
            "per-site actions must still be offered"
        );
        for action in &actions {
            assert!(
                !action.title.ends_with(" in remaining conflicts"),
                "single-conflict file must not offer bulk action: {}",
                action.title,
            );
        }
    }

    #[test]
    fn cursor_outside_every_conflict_returns_no_actions() {
        let info = parse_diff3(TEXT_3_CONFLICTS);
        let doc = document(TEXT_3_CONFLICTS);

        for line in [0u32, 6, 14, 20] {
            let actions = info.code_actions_at(&range_at(line), &uri(), &doc);
            assert!(
                actions.is_empty(),
                "expected no actions at line {line}, got {actions:?}",
            );
        }
    }

    #[test]
    fn bulk_head_action_resolves_every_conflict_to_head_side() {
        let info = parse_diff3(TEXT_3_CONFLICTS);
        let doc = document(TEXT_3_CONFLICTS);

        let actions = info.code_actions_at(&range_at(2), &uri(), &doc);
        let bulk_head = actions
            .iter()
            .find(|a| a.title == "Keep HEAD in remaining conflicts")
            .expect("bulk HEAD action");

        let bulk_edits = extract_edits(bulk_head);
        assert_eq!(bulk_edits.len(), 3, "one edit per conflict region");

        let bulk_result = apply_edits(TEXT_3_CONFLICTS, &bulk_edits);

        let mut per_site_edits: Vec<lsp_types::TextEdit> = Vec::new();
        for region_idx in 0..info.conflicts.len() {
            let region = &info.conflicts[region_idx];
            let region_actions =
                info.code_actions_at(&range_at(region.head), &uri(), &doc);
            let keep_head = region_actions
                .iter()
                .find(|a| a.title == "Keep HEAD")
                .expect("Keep HEAD per-site");
            per_site_edits.extend(extract_edits(keep_head));
        }
        per_site_edits.sort_by(|a, b| b.range.start.line.cmp(&a.range.start.line));
        let per_site_result = apply_edits(TEXT_3_CONFLICTS, &per_site_edits);

        assert_eq!(bulk_result, per_site_result);
    }

    #[test]
    fn bulk_branch_action_resolves_every_conflict_to_branch_side() {
        let info = parse_diff3(TEXT_3_CONFLICTS);
        let doc = document(TEXT_3_CONFLICTS);

        let actions = info.code_actions_at(&range_at(2), &uri(), &doc);
        let bulk_branch = actions
            .iter()
            .find(|a| a.title == "Keep branch in remaining conflicts")
            .expect("bulk branch action");

        let bulk_edits = extract_edits(bulk_branch);
        assert_eq!(bulk_edits.len(), 3);

        let bulk_result = apply_edits(TEXT_3_CONFLICTS, &bulk_edits);

        let mut per_site_edits: Vec<lsp_types::TextEdit> = Vec::new();
        for region_idx in 0..info.conflicts.len() {
            let region = &info.conflicts[region_idx];
            let region_actions =
                info.code_actions_at(&range_at(region.head), &uri(), &doc);
            let keep_branch = region_actions
                .iter()
                .find(|a| a.title == "Keep branch")
                .expect("Keep branch per-site");
            per_site_edits.extend(extract_edits(keep_branch));
        }
        per_site_edits.sort_by(|a, b| b.range.start.line.cmp(&a.range.start.line));
        let per_site_result = apply_edits(TEXT_3_CONFLICTS, &per_site_edits);

        assert_eq!(bulk_result, per_site_result);
    }

    #[test]
    fn bulk_action_titles_use_file_labels_when_present() {
        let info = parse_diff3(TEXT_3_CONFLICTS_LABELLED);
        assert_eq!(info.head.as_deref(), Some("main"));
        assert_eq!(info.branch.as_deref(), Some("feature"));
        let doc = document(TEXT_3_CONFLICTS_LABELLED);

        let actions = info.code_actions_at(&range_at(2), &uri(), &doc);
        let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();
        assert!(
            titles.contains(&"Keep main in remaining conflicts"),
            "missing labelled head bulk action; titles: {titles:?}"
        );
        assert!(
            titles.contains(&"Keep feature in remaining conflicts"),
            "missing labelled branch bulk action; titles: {titles:?}"
        );
    }

    #[test]
    fn bulk_action_diagnostics_field_carries_only_cursor_region_diagnostic() {
        let info = parse_diff3(TEXT_3_CONFLICTS);
        let doc = document(TEXT_3_CONFLICTS);

        let cursor_region = &info.conflicts[1];
        let cursor_diag = lsp_types::Diagnostic::from(cursor_region);

        let actions = info.code_actions_at(&range_at(cursor_region.head), &uri(), &doc);
        let bulks: Vec<_> = actions
            .iter()
            .filter(|a| a.title.ends_with(" in remaining conflicts"))
            .collect();
        assert_eq!(bulks.len(), 2);
        for action in bulks {
            let diags = action.diagnostics.as_ref().expect("diagnostics");
            assert_eq!(diags.len(), 1, "{}", action.title);
            assert_eq!(diags[0], cursor_diag, "{}", action.title);
        }
    }
}
