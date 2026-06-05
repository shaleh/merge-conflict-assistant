//! Support for Jujutsu aka JJ snapshot style conflicts.
// https://www.jj-vcs.dev/latest/conflicts/#alternative-conflict-marker-styles

use crate::parser::{
    MARKER_END, MARKER_JJ_SNAPSHOT_BASE, MARKER_JJ_SNAPSHOT_SIDE, ParseError, strip_marker,
};

/// One side of a jj snapshot conflict, introduced by a `+++++++` marker.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Side {
    label: String,
    content_start_line: u32,
    content_end_line: u32,
}

/// The merge base of a jj snapshot conflict, introduced by a `-------` marker.
#[derive(Clone, Debug, Eq, PartialEq)]
struct Base {
    label: String,
    content_start_line: u32,
    content_end_line: u32,
}

/// A single jj snapshot conflict region within a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConflictRegion {
    head_line: u32,
    end_line: u32,
    sides: Vec<Side>,
    bases: Vec<Base>,
}

/// Parse result for a jj snapshot conflict document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VcsInfo {
    pub conflicts: Vec<ConflictRegion>,
}

pub fn build_region(
    head_line: u32,
    end_line: u32,
    sides: Vec<(String, u32, u32)>,
    bases: Vec<(String, u32, u32)>,
) -> ConflictRegion {
    ConflictRegion {
        head_line,
        end_line,
        sides: sides
            .into_iter()
            .map(|(label, content_start_line, content_end_line)| Side {
                label,
                content_start_line,
                content_end_line,
            })
            .collect(),
        bases: bases
            .into_iter()
            .map(|(label, content_start_line, content_end_line)| Base {
                label,
                content_start_line,
                content_end_line,
            })
            .collect(),
    }
}

/// Running state of a snapshot block parse. Accumulates sides and bases as the
/// parser walks the lines between `<<<<<<<` and `>>>>>>>`, maintaining the
/// currently-open section (either a side or the base).
#[derive(Debug)]
struct Parser {
    head_line: u32,
    sides: Vec<(String, u32, u32)>,
    bases: Vec<(String, u32, u32)>,
    open_marker_line: u32,
    open_section_label: String,
    open_is_base: bool,
}

impl Parser {
    fn new(head_line: u32, first_side_marker_line: u32, first_side_label: String) -> Self {
        Self {
            head_line,
            sides: Vec::new(),
            bases: Vec::new(),
            open_marker_line: first_side_marker_line,
            open_section_label: first_side_label,
            open_is_base: false,
        }
    }

    fn close_open_section(&mut self, end_line: u32) {
        let entry = (
            std::mem::take(&mut self.open_section_label),
            self.open_marker_line + 1,
            end_line,
        );
        if self.open_is_base {
            self.bases.push(entry);
        } else {
            self.sides.push(entry);
        }
    }

    fn on_side_marker(&mut self, marker_line: u32, label: String) {
        self.close_open_section(marker_line);
        self.open_marker_line = marker_line;
        self.open_section_label = label;
        self.open_is_base = false;
    }

    fn on_base_marker(&mut self, marker_line: u32, label: String) -> Result<(), ParseError> {
        if self.open_is_base {
            return Err(ParseError::Incomplete {
                state: "consecutive base markers".to_string(),
            });
        }
        self.close_open_section(marker_line);
        self.open_marker_line = marker_line;
        self.open_section_label = label;
        self.open_is_base = true;
        Ok(())
    }

    fn finalize(mut self, end_line: u32) -> Result<ConflictRegion, ParseError> {
        if self.open_is_base {
            return Err(ParseError::Incomplete {
                state: "block ends on base marker".to_string(),
            });
        }
        self.close_open_section(end_line);
        Ok(build_region(
            self.head_line,
            end_line,
            self.sides,
            self.bases,
        ))
    }
}

/// Parse one jj snapshot conflict block starting at `head_index`.
///
/// `head_index` is the 0-based line index of the `<<<<<<<` marker.
///
/// Returns `(region, next_index)` where `next_index` is the line just past the `>>>>>>>`.
pub fn parse_block(
    lines: &[&str],
    head_index: usize,
) -> Result<(ConflictRegion, usize), ParseError> {
    let head_line: u32 = head_index.try_into()?;

    let first_inner = head_index + 1;
    let Some(first_line) = lines.get(first_inner).copied() else {
        return Err(ParseError::Incomplete {
            state: format!("snapshot block at line {head_line} has no content after head marker"),
        });
    };
    let Some(first_label) = strip_marker(first_line, MARKER_JJ_SNAPSHOT_SIDE) else {
        return Err(ParseError::Incomplete {
            state: format!(
                "snapshot block at line {head_line} first inner line is not a side marker"
            ),
        });
    };
    let first_marker_line: u32 = first_inner.try_into()?;
    let first_label = first_label.to_string();

    let mut parser = Parser::new(head_line, first_marker_line, first_label);

    for (offset, line) in lines[first_inner + 1..].iter().enumerate() {
        let lineno = first_inner + 1 + offset;
        let first = line.as_bytes().first();

        if first == Some(&b'+')
            && let Some(label) = strip_marker(line, MARKER_JJ_SNAPSHOT_SIDE)
        {
            parser.on_side_marker(lineno.try_into()?, label.to_string());
        } else if first == Some(&b'-')
            && let Some(label) = strip_marker(line, MARKER_JJ_SNAPSHOT_BASE)
        {
            parser.on_base_marker(lineno.try_into()?, label.to_string())?;
        } else if first == Some(&b'>') && strip_marker(line, MARKER_END).is_some() {
            let region = parser.finalize(lineno.try_into()?)?;
            return Ok((region, lineno + 1));
        }
        // Any other line is content of the currently-open section. jj snapshot
        // conflicts only use `+++++++`, `-------`, and `>>>>>>>` structurally, so
        // diff3-shaped lines such as `=======` or `|||||||` are ordinary content.
    }

    Err(ParseError::Incomplete {
        state: format!("{parser:?}"),
    })
}

impl ConflictRegion {
    /// Returns true if the given LSP range overlaps with this snapshot conflict.
    ///
    /// The range must start within the conflict region. A range that begins
    /// before the conflict is rejected — this avoids matching when the user
    /// has selected across multiple conflicts or only part of one.
    fn is_in_range(&self, range: &lsp_types::Range) -> bool {
        tracing::debug!(
            "is_in_range: range: {:?}, head_line: {}, end_line: {}",
            range,
            self.head_line,
            self.end_line
        );
        self.head_line <= range.start.line
            && self.end_line >= range.start.line
            && self.end_line + 1 >= range.end.line
    }
}

impl VcsInfo {
    fn find_region_at(&self, range: &lsp_types::Range) -> Option<&ConflictRegion> {
        self.conflicts.iter().find(|c| c.is_in_range(range))
    }

    pub fn code_actions_at(
        &self,
        range: &lsp_types::Range,
        uri: &lsp_types::Uri,
        document: &lsp_textdocument::FullTextDocument,
    ) -> Vec<lsp_types::CodeAction> {
        let Some(region) = self.find_region_at(range) else {
            return Vec::new();
        };

        let diagnostic = lsp_types::Diagnostic::from(region);
        let conflict_range = range_for_diagnostic_conflict(region);

        let mut items: Vec<lsp_types::CodeAction> = region
            .sides
            .iter()
            .map(|side| {
                // make_text_edit expects a marker-line start; Side/Base store the
                // content-line start, so we subtract 1 to align the contracts.
                let edit = crate::state::make_text_edit(
                    document,
                    conflict_range,
                    &[(side.content_start_line - 1, side.content_end_line)],
                );
                crate::state::make_code_action(
                    format!("Keep '{}'", side.label),
                    uri,
                    edit,
                    diagnostic.clone(),
                )
            })
            .collect();

        for base in &region.bases {
            // make_text_edit expects a marker-line start; Side/Base store the
            // content-line start, so we subtract 1 to align the contracts.
            let edit = crate::state::make_text_edit(
                document,
                conflict_range,
                &[(base.content_start_line - 1, base.content_end_line)],
            );
            items.push(crate::state::make_code_action(
                format!("Keep '{}'", base.label),
                uri,
                edit,
                diagnostic.clone(),
            ));
        }

        let all_side_ranges: Vec<(u32, u32)> = region
            .sides
            .iter()
            .map(|side| (side.content_start_line - 1, side.content_end_line))
            .collect();
        let keep_all_edit =
            crate::state::make_text_edit(document, conflict_range, &all_side_ranges);
        items.push(crate::state::make_code_action(
            "Keep all sides".to_string(),
            uri,
            keep_all_edit,
            diagnostic.clone(),
        ));

        let drop_edit = crate::state::make_text_edit(document, conflict_range, &[]);
        items.push(crate::state::make_code_action(
            "Drop all".to_string(),
            uri,
            drop_edit,
            diagnostic.clone(),
        ));

        tracing::info!(
            "offering {} code action(s) for snapshot conflict at lines {}-{} in {:?}",
            items.len(),
            region.head_line,
            region.end_line,
            uri,
        );
        items
    }
}

fn range_for_diagnostic_conflict(region: &ConflictRegion) -> lsp_types::Range {
    let start = lsp_types::Position {
        line: region.head_line,
        character: 0,
    };
    let end = lsp_types::Position {
        line: region.end_line + 1,
        character: 0,
    };
    lsp_types::Range { start, end }
}

impl From<&ConflictRegion> for lsp_types::Diagnostic {
    fn from(region: &ConflictRegion) -> Self {
        let range = range_for_diagnostic_conflict(region);
        Self {
            range,
            message: "jj snapshot conflict".to_owned(),
            source: Some("merge".to_owned()),
            severity: Some(lsp_types::DiagnosticSeverity::ERROR),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use parking_lot::Mutex;
    use rstest::*;

    use super::{Base, ConflictRegion, Side, VcsInfo};
    use crate::parser::{self, parse};
    use crate::state::{DocumentState, ServerState};
    #[allow(unused_imports)]
    use crate::test_helpers::init_logging;
    use crate::test_helpers::{
        TEXT_JJ_SNAPSHOT_2SIDED, TEXT_JJ_SNAPSHOT_3SIDED, TEXT_JJ_SNAPSHOT_4SIDED,
    };

    fn snapshot_2sided_conflict() -> parser::MergeConflict {
        parser::MergeConflict::JjSnapshot(VcsInfo {
            conflicts: vec![ConflictRegion {
                head_line: 1,
                end_line: 11,
                sides: vec![
                    Side {
                        label: r#"abc12345 "first change""#.to_string(),
                        content_start_line: 3,
                        content_end_line: 5,
                    },
                    Side {
                        label: r#"def67890 "second change""#.to_string(),
                        content_start_line: 9,
                        content_end_line: 11,
                    },
                ],
                bases: vec![Base {
                    label: r#"base11111 "merge base""#.to_string(),
                    content_start_line: 6,
                    content_end_line: 8,
                }],
            }],
        })
    }

    fn snapshot_3sided_conflict() -> parser::MergeConflict {
        parser::MergeConflict::JjSnapshot(VcsInfo {
            conflicts: vec![ConflictRegion {
                head_line: 1,
                end_line: 13,
                sides: vec![
                    Side {
                        label: r#"change0 abc12345 "change A""#.to_string(),
                        content_start_line: 3,
                        content_end_line: 4,
                    },
                    Side {
                        label: r#"change1 def67890 "change B""#.to_string(),
                        content_start_line: 7,
                        content_end_line: 8,
                    },
                    Side {
                        label: r#"change2 ghi11111 "change C""#.to_string(),
                        content_start_line: 11,
                        content_end_line: 13,
                    },
                ],
                bases: vec![
                    Base {
                        label: r#"base-label0 abc00000 "base A""#.to_string(),
                        content_start_line: 5,
                        content_end_line: 6,
                    },
                    Base {
                        label: r#"base-label1 def00000 "base B""#.to_string(),
                        content_start_line: 9,
                        content_end_line: 10,
                    },
                ],
            }],
        })
    }

    fn range_inside_conflict(head_line: u32) -> lsp_types::Range {
        lsp_types::Range {
            start: lsp_types::Position {
                line: head_line + 1,
                character: 0,
            },
            end: lsp_types::Position {
                line: head_line + 1,
                character: 1,
            },
        }
    }

    fn snapshot_uri() -> lsp_types::Uri {
        "file://snapshot_test.txt".parse().unwrap()
    }

    fn code_actions_for_snapshot(
        text: &'static str,
        conflict: parser::MergeConflict,
        range: lsp_types::Range,
        uri: lsp_types::Uri,
    ) -> Vec<lsp_types::CodeAction> {
        let (_, rx) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let (tx, _) = crossbeam_channel::unbounded::<lsp_server::Message>();
        let conn = lsp_server::Connection {
            sender: tx,
            receiver: rx,
        };
        let state = ServerState::new(conn.sender);
        {
            let mut docs = state.documents.lock();
            docs.insert(
                uri.clone(),
                Arc::new(Mutex::new(DocumentState::new_with_conflict(
                    text.to_string(),
                    0,
                    conflict,
                ))),
            );
        }
        state
            .code_action(lsp_types::CodeActionParams {
                text_document: lsp_types::TextDocumentIdentifier { uri },
                range,
                context: lsp_types::CodeActionContext {
                    diagnostics: vec![],
                    only: None,
                    trigger_kind: None,
                },
                work_done_progress_params: lsp_types::WorkDoneProgressParams {
                    work_done_token: None,
                },
                partial_result_params: lsp_types::PartialResultParams {
                    partial_result_token: None,
                },
            })
            .expect("code_action must not error")
    }

    fn extract_edit(action: &lsp_types::CodeAction) -> lsp_types::TextEdit {
        let edits = action
            .edit
            .as_ref()
            .expect("action must have a workspace edit")
            .changes
            .as_ref()
            .expect("workspace edit must have changes")
            .values()
            .next()
            .expect("changes must include the document URI");
        assert_eq!(1, edits.len(), "expected exactly one text edit");
        edits[0].clone()
    }

    /// Parser recognizes a 2-sided snapshot conflict, captures verbatim side
    /// and base labels (including spaces and quoted substrings), records
    /// 0-based content-range line numbers, and routes a `<<<<<<<` followed by
    /// `+++++++` to the snapshot parser rather than the diff3-style path.
    #[rstest]
    fn finds_conflict_2_sided() {
        let result = parse(TEXT_JJ_SNAPSHOT_2SIDED).expect("parse should not error");
        let mc = result.expect("should find a conflict");
        let parser::MergeConflict::JjSnapshot(snap) = mc else {
            panic!("expected JjSnapshot, got {:?}", mc);
        };
        assert_eq!(1, snap.conflicts.len());
        let region = &snap.conflicts[0];
        assert_eq!(1, region.head_line, "head_line");
        assert_eq!(2, region.sides.len(), "sides count");
        assert_eq!(1, region.bases.len(), "bases count");
        assert_eq!(
            r#"abc12345 "first change""#, region.sides[0].label,
            "side[0] label"
        );
        assert_eq!(
            r#"def67890 "second change""#, region.sides[1].label,
            "side[1] label"
        );
        let base = &region.bases[0];
        assert_eq!(r#"base11111 "merge base""#, base.label, "base label");
        assert_eq!(
            3, region.sides[0].content_start_line,
            "side[0] content_start"
        );
        assert_eq!(5, region.sides[0].content_end_line, "side[0] content_end");
        assert_eq!(6, base.content_start_line, "base content_start");
        assert_eq!(8, base.content_end_line, "base content_end");
        assert_eq!(
            9, region.sides[1].content_start_line,
            "side[1] content_start"
        );
        assert_eq!(11, region.sides[1].content_end_line, "side[1] content_end");
        assert_eq!(11, region.end_line, "end_line");
    }

    /// A snapshot conflict with three `+++++++` sides and two `-------` bases
    /// produces a region with exactly three sides and two bases.
    #[rstest]
    fn finds_conflict_3_sided() {
        let result = parse(TEXT_JJ_SNAPSHOT_3SIDED).expect("parse should not error");
        let mc = result.expect("should find a conflict");
        let parser::MergeConflict::JjSnapshot(snap) = mc else {
            panic!("expected JjSnapshot, got {:?}", mc);
        };
        assert_eq!(1, snap.conflicts.len());
        let region = &snap.conflicts[0];
        assert_eq!(3, region.sides.len(), "sides count");
        assert_eq!(2, region.bases.len(), "bases count");
        assert_eq!(
            r#"change0 abc12345 "change A""#, region.sides[0].label,
            "side[0] label"
        );
        assert_eq!(
            r#"change1 def67890 "change B""#, region.sides[1].label,
            "side[1] label"
        );
        assert_eq!(
            r#"change2 ghi11111 "change C""#, region.sides[2].label,
            "side[2] label"
        );
        assert_eq!(
            r#"base-label0 abc00000 "base A""#, region.bases[0].label,
            "bases[0] label"
        );
        assert_eq!(
            r#"base-label1 def00000 "base B""#, region.bases[1].label,
            "bases[1] label"
        );
    }

    /// A snapshot conflict with four `+++++++` sides and three `-------` bases
    /// produces a region with exactly four sides and three bases.
    #[rstest]
    fn finds_jj_snapshot_conflict_4_sided() {
        let result = parse(TEXT_JJ_SNAPSHOT_4SIDED).expect("parse should not error");
        let mc = result.expect("should find a conflict");
        let parser::MergeConflict::JjSnapshot(snap) = mc else {
            panic!("expected JjSnapshot, got {:?}", mc);
        };
        assert_eq!(1, snap.conflicts.len());
        let region = &snap.conflicts[0];
        assert_eq!(4, region.sides.len(), "sides count for 4-way conflict");
        assert_eq!(3, region.bases.len(), "bases count for 4-way conflict");
        assert_eq!(r#"side0 "alpha""#, region.sides[0].label, "side[0] label");
        assert_eq!(r#"side3 "delta""#, region.sides[3].label, "side[3] label");
        assert_eq!(
            r#"base0 "ancestor0""#, region.bases[0].label,
            "bases[0] label"
        );
        assert_eq!(
            r#"base2 "ancestor2""#, region.bases[2].label,
            "bases[2] label"
        );
    }

    /// A `+++++++` marker followed immediately by another marker line
    /// (no content between them) is legal; the parser returns Ok and the
    /// resulting side has content_start_line == content_end_line (empty range).
    #[rstest]
    fn conflict_with_empty_side_is_legal() {
        let input = concat!(
            "<<<<<<<",
            " conflict\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " empty_label\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " other_label\n",
            "some content\n",
            ">>>>>>>",
            " conflict ends\n",
        );
        let result = parse(input);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let mc = result.unwrap().expect("should find a conflict");
        let parser::MergeConflict::JjSnapshot(snap) = mc else {
            panic!("expected JjSnapshot, got {:?}", mc);
        };
        assert_eq!(2, snap.conflicts[0].sides.len());
        let empty_side = &snap.conflicts[0].sides[0];
        assert_eq!(
            empty_side.content_start_line, empty_side.content_end_line,
            "empty side must have content_start_line == content_end_line"
        );
        assert_eq!("empty_label", empty_side.label);
    }

    /// A `+++++++` or `-------` marker line with no trailing text after the
    /// token produces an empty label string (`""`). The parser returns Ok.
    #[rstest]
    fn conflict_unlabeled_markers_capture_empty_label() {
        let input = concat!(
            "<<<<<<<",
            "\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            "\n",
            "content\n",
            concat!("-", "-", "-", "-", "-", "-", "-"),
            "\n",
            "base\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            "\n",
            "content2\n",
            ">>>>>>>",
            "\n",
        );
        let result = parse(input);
        assert!(result.is_ok(), "expected Ok, got {:?}", result);
        let mc = result.unwrap().expect("should find a conflict");
        let parser::MergeConflict::JjSnapshot(snap) = mc else {
            panic!("expected JjSnapshot, got {:?}", mc);
        };
        let region = &snap.conflicts[0];
        assert_eq!(
            "", region.sides[0].label,
            "unlabeled side must have empty label"
        );
        assert_eq!(1, region.bases.len(), "must have one base");
        assert_eq!(
            "", region.bases[0].label,
            "unlabeled base must have empty label"
        );
    }

    /// A line starting with `++++++++` (8 pluses) is NOT a valid snapshot
    /// side marker (exactly-7-char rule). When the only inner markers present are
    /// 8-plus lines, no `+++++++` (7) is ever seen, so the state machine never
    /// classifies the block and never finds a terminator; the result is
    /// `Err(ParseError::Incomplete { .. })`.
    /// The symmetric case with `--------` (8 minuses) is also tested.
    #[rstest]
    fn marker_with_8_plus_chars_is_rejected() {
        let input_8plus = concat!(
            "<<<<<<<",
            " conflict\n",
            concat!("+", "+", "+", "+", "+", "+", "+", "+"),
            " label\n",
            "content\n",
            ">>>>>>>",
            " conflict ends\n",
        );
        let result = parse(input_8plus);
        assert!(
            matches!(result, Err(parser::ParseError::Incomplete { .. })),
            "8 pluses: expected Err(ParseError::Incomplete), got {:?}",
            result
        );

        let input_8minus = concat!(
            "<<<<<<<",
            " conflict\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " sideA\n",
            "content_a\n",
            concat!("-", "-", "-", "-", "-", "-", "-", "-"),
            " not-a-base\n",
            "base_content\n",
            ">>>>>>>",
            " conflict ends\n",
        );
        let result2 = parse(input_8minus);
        let mc = result2.expect("8 minuses: parse should succeed (line treated as content)");
        let mc = mc.expect("should find a conflict");
        let parser::MergeConflict::JjSnapshot(snap) = mc else {
            panic!("expected JjSnapshot, got {:?}", mc);
        };
        assert!(
            snap.conflicts[0].bases.is_empty(),
            "8-minus line must not become a base"
        );
    }

    /// A 2-sided jj snapshot conflict with one base returns exactly 5 code
    /// actions (Keep side-A, Keep side-B, Keep base, Keep all sides, Drop all).
    #[rstest]
    fn code_actions_2_sided_returns_5_actions() {
        let actions = code_actions_for_snapshot(
            TEXT_JJ_SNAPSHOT_2SIDED,
            snapshot_2sided_conflict(),
            range_inside_conflict(1),
            snapshot_uri(),
        );
        assert_eq!(
            5,
            actions.len(),
            "expected 5 actions, got {} with titles: {:?}",
            actions.len(),
            actions.iter().map(|a| &a.title).collect::<Vec<_>>()
        );
        let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();
        assert!(
            titles.contains(&r#"Keep 'abc12345 "first change"'"#),
            "titles: {titles:?}"
        );
        assert!(
            titles.contains(&r#"Keep 'def67890 "second change"'"#),
            "titles: {titles:?}"
        );
        assert!(
            titles.contains(&r#"Keep 'base11111 "merge base"'"#),
            "titles: {titles:?}"
        );
        assert!(titles.contains(&"Keep all sides"), "titles: {titles:?}");
        assert!(titles.contains(&"Drop all"), "titles: {titles:?}");
    }

    /// A 3-sided jj snapshot conflict with two bases returns exactly 7 code
    /// actions (Keep × 3 sides + Keep × 2 bases + Keep all sides + Drop all).
    #[rstest]
    fn code_actions_3_sided_returns_7_actions() {
        let actions = code_actions_for_snapshot(
            TEXT_JJ_SNAPSHOT_3SIDED,
            snapshot_3sided_conflict(),
            range_inside_conflict(1),
            snapshot_uri(),
        );
        assert_eq!(
            7,
            actions.len(),
            "expected 7 actions, got {} with titles: {:?}",
            actions.len(),
            actions.iter().map(|a| a.title.as_str()).collect::<Vec<_>>()
        );
        let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();
        assert!(
            titles.contains(&r#"Keep 'change0 abc12345 "change A"'"#),
            "titles: {titles:?}"
        );
        assert!(
            titles.contains(&r#"Keep 'change1 def67890 "change B"'"#),
            "titles: {titles:?}"
        );
        assert!(
            titles.contains(&r#"Keep 'change2 ghi11111 "change C"'"#),
            "titles: {titles:?}"
        );
        assert!(
            titles.contains(&r#"Keep 'base-label0 abc00000 "base A"'"#),
            "titles: {titles:?}"
        );
        assert!(
            titles.contains(&r#"Keep 'base-label1 def00000 "base B"'"#),
            "titles: {titles:?}"
        );
        assert!(titles.contains(&"Keep all sides"), "titles: {titles:?}");
        assert!(titles.contains(&"Drop all"), "titles: {titles:?}");
    }

    /// "Keep '<side-label>'" replaces the entire conflict block with only
    /// that side's content lines (no marker lines remain).
    #[rstest]
    fn keep_side_replaces_with_that_side_content() {
        let actions = code_actions_for_snapshot(
            TEXT_JJ_SNAPSHOT_2SIDED,
            snapshot_2sided_conflict(),
            range_inside_conflict(1),
            snapshot_uri(),
        );

        let keep_side_a = actions
            .iter()
            .find(|a| a.title == r#"Keep 'abc12345 "first change"'"#)
            .expect("Keep side-A action must exist");
        let edit_a = extract_edit(keep_side_a);
        assert_eq!(
            "apple\ngrapefruit\n", edit_a.new_text,
            "Keep side-A must replace with side-A content"
        );

        let keep_side_b = actions
            .iter()
            .find(|a| a.title == r#"Keep 'def67890 "second change"'"#)
            .expect("Keep side-B action must exist");
        let edit_b = extract_edit(keep_side_b);
        assert_eq!(
            "APPLE\nGRAPE\n", edit_b.new_text,
            "Keep side-B must replace with side-B content"
        );

        let actions3 = code_actions_for_snapshot(
            TEXT_JJ_SNAPSHOT_3SIDED,
            snapshot_3sided_conflict(),
            range_inside_conflict(1),
            snapshot_uri(),
        );
        let keep_side_c = actions3
            .iter()
            .find(|a| a.title == r#"Keep 'change2 ghi11111 "change C"'"#)
            .expect("Keep side-C action must exist");
        let edit_c = extract_edit(keep_side_c);
        assert_eq!(
            "gamma\ndelta\n", edit_c.new_text,
            "Keep side-C must replace with side-C content"
        );
    }

    /// "Keep '<base-label>'" replaces the entire conflict block with only
    /// the base content lines (no marker lines remain).
    #[rstest]
    fn keep_base_replaces_with_base_content() {
        let actions = code_actions_for_snapshot(
            TEXT_JJ_SNAPSHOT_2SIDED,
            snapshot_2sided_conflict(),
            range_inside_conflict(1),
            snapshot_uri(),
        );
        let keep_base = actions
            .iter()
            .find(|a| a.title == r#"Keep 'base11111 "merge base"'"#)
            .expect("Keep base action must exist");
        let edit = extract_edit(keep_base);
        assert_eq!(
            "apple\ngrape\n", edit.new_text,
            "Keep base must replace with base content"
        );
    }

    /// "Keep all sides" concatenates every side's content in document order
    /// with empty-string join (no separator inserted), and omits the base.
    #[rstest]
    fn keep_all_sides_concatenates_sides_omits_base() {
        let actions = code_actions_for_snapshot(
            TEXT_JJ_SNAPSHOT_2SIDED,
            snapshot_2sided_conflict(),
            range_inside_conflict(1),
            snapshot_uri(),
        );
        let keep_all = actions
            .iter()
            .find(|a| a.title == "Keep all sides")
            .expect("Keep all sides action must exist");
        let edit = extract_edit(keep_all);
        assert_eq!(
            "apple\ngrapefruit\nAPPLE\nGRAPE\n", edit.new_text,
            "Keep all sides must concatenate side content without separator, omitting base"
        );
    }

    /// 3-sided "Keep all sides" concatenates all three sides in document
    /// order with no separator, and omits the bases.
    #[rstest]
    fn keep_all_sides_3_sided_concatenates_all_sides() {
        let actions = code_actions_for_snapshot(
            TEXT_JJ_SNAPSHOT_3SIDED,
            snapshot_3sided_conflict(),
            range_inside_conflict(1),
            snapshot_uri(),
        );
        let keep_all = actions
            .iter()
            .find(|a| a.title == "Keep all sides")
            .expect("Keep all sides action must exist");
        let edit = extract_edit(keep_all);
        assert_eq!(
            "alpha\nbeta\ngamma\ndelta\n", edit.new_text,
            "Keep all sides (3-sided) must concatenate all three sides without separator, omitting bases"
        );
    }

    /// "Drop all" replaces the entire conflict block with an empty string.
    #[rstest]
    fn drop_all_produces_empty_replacement() {
        let actions = code_actions_for_snapshot(
            TEXT_JJ_SNAPSHOT_2SIDED,
            snapshot_2sided_conflict(),
            range_inside_conflict(1),
            snapshot_uri(),
        );
        let drop_all = actions
            .iter()
            .find(|a| a.title == "Drop all")
            .expect("Drop all action must exist");
        let edit = extract_edit(drop_all);
        assert_eq!("", edit.new_text, "Drop all must replace with empty string");
    }

    /// "Keep '<label>'" on a side whose content is empty (content_start_line
    /// == content_end_line) produces an empty replacement — equivalent to Drop
    /// all for that block.
    #[rstest]
    fn keep_on_empty_side_produces_empty_replacement() {
        let empty_side_text = concat!(
            "before\n",
            "<<<<<<<",
            " conflict\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " empty_side\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " other_side\n",
            "content\n",
            ">>>>>>>",
            " conflict ends\n",
            "after\n",
        );
        let conflict = parser::MergeConflict::JjSnapshot(VcsInfo {
            conflicts: vec![ConflictRegion {
                head_line: 1,
                end_line: 5,
                sides: vec![
                    Side {
                        label: "empty_side".to_string(),
                        content_start_line: 3,
                        content_end_line: 3,
                    },
                    Side {
                        label: "other_side".to_string(),
                        content_start_line: 4,
                        content_end_line: 5,
                    },
                ],
                bases: vec![],
            }],
        });
        let actions = code_actions_for_snapshot(
            empty_side_text,
            conflict,
            range_inside_conflict(1),
            snapshot_uri(),
        );
        let keep_empty = actions
            .iter()
            .find(|a| a.title == "Keep 'empty_side'")
            .expect("Keep 'empty_side' action must exist");
        let edit = extract_edit(keep_empty);
        assert_eq!(
            "", edit.new_text,
            "Keep on empty side must produce empty replacement"
        );
    }

    /// A code action request with a range entirely outside any snapshot
    /// conflict returns an empty Vec.
    #[rstest]
    fn code_action_out_of_range_returns_empty() {
        let outside_range = lsp_types::Range {
            start: lsp_types::Position {
                line: 0,
                character: 0,
            },
            end: lsp_types::Position {
                line: 0,
                character: 1,
            },
        };
        let actions = code_actions_for_snapshot(
            TEXT_JJ_SNAPSHOT_2SIDED,
            snapshot_2sided_conflict(),
            outside_range,
            snapshot_uri(),
        );
        assert!(
            actions.is_empty(),
            "expected empty actions for out-of-range request, got {actions:?}"
        );
    }

    /// Code action titles use the verbatim label from the marker line,
    /// including spaces and quoted substrings.
    #[rstest]
    fn action_labels_are_verbatim_from_markers() {
        let actions = code_actions_for_snapshot(
            TEXT_JJ_SNAPSHOT_2SIDED,
            snapshot_2sided_conflict(),
            range_inside_conflict(1),
            snapshot_uri(),
        );
        let titles: Vec<&str> = actions.iter().map(|a| a.title.as_str()).collect();
        assert!(
            titles.contains(&r#"Keep 'abc12345 "first change"'"#),
            "side-A title with spaces and quotes must be verbatim; titles: {titles:?}"
        );
        assert!(
            titles.contains(&r#"Keep 'def67890 "second change"'"#),
            "side-B title with spaces and quotes must be verbatim; titles: {titles:?}"
        );
        assert!(
            titles.contains(&r#"Keep 'base11111 "merge base"'"#),
            "base title with spaces and quotes must be verbatim; titles: {titles:?}"
        );
    }

    /// A snapshot block that ends with a `-------` marker (no closing side) is
    /// malformed; the parser must return `Err(ParseError::Incomplete { .. })`.
    #[rstest]
    fn conflict_block_ending_on_base_errors() {
        let input = concat!(
            "<<<<<<<",
            " conflict\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " sideA\n",
            "side content\n",
            concat!("-", "-", "-", "-", "-", "-", "-"),
            " base1\n",
            "base content\n",
            ">>>>>>>",
            " conflict ends\n",
        );
        let result = parse(input);
        assert!(
            matches!(result, Err(parser::ParseError::Incomplete { .. })),
            "expected Err(ParseError::Incomplete), got {:?}",
            result
        );
    }

    /// A `=======` (or `|||||||`) line inside snapshot side/base content is
    /// ordinary content — jj snapshots use only `+++++++`, `-------`, and
    /// `>>>>>>>` structurally. The conflict must still be detected.
    #[rstest]
    fn equals_in_side_content_treated_as_content() {
        let input = concat!(
            "<<<<<<<", " conflict\n",
            concat!("+", "+", "+", "+", "+", "+", "+"), " sideA\n",
            "alpha\n",
            concat!("=", "=", "=", "=", "=", "=", "="), "\n",
            "beta\n",
            ">>>>>>>", " ends\n",
        );
        let mc = parse(input)
            .expect("parse should not error")
            .expect("should find a conflict");
        let parser::MergeConflict::JjSnapshot(snap) = mc else {
            panic!("expected JjSnapshot, got {:?}", mc);
        };
        assert_eq!(1, snap.conflicts.len());
        let region = &snap.conflicts[0];
        assert_eq!(1, region.sides.len(), "the `=======` line must not split the side");
        // side content spans alpha, =======, beta (content lines 2..=4).
        assert_eq!(2, region.sides[0].content_start_line);
        assert_eq!(5, region.sides[0].content_end_line);
    }
}
