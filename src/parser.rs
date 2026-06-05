//! Single-pass state-machine parser for merge conflict markers.
//!
//! Recognizes standard, diff3-style, and JJ snapshot conflicts by scanning for the
//! marker prefixes. Branch and ancestor names following the markers are captured when present.
//!
//! All line numbers stored are 0-based indexes (line 100 in the file is stored as 99).
//! Content for a region is the lines *after* its opening marker and *before* its
//! closing marker.

use lsp_types::CodeAction;

use crate::{
    state::DocumentState,
    styles::{diff3, jj_snapshot},
};

pub const MARKER_HEAD: &str = "<<<<<<<";
pub const MARKER_ANCESTOR: &str = "|||||||";
pub const MARKER_SEPARATOR: &str = "=======";
pub const MARKER_END: &str = ">>>>>>>";
pub const MARKER_JJ_SNAPSHOT_SIDE: &str = "+++++++";
pub const MARKER_JJ_SNAPSHOT_BASE: &str = "-------";

/// Typed error returned by [`parse`].
#[derive(Debug)]
pub enum ParseError {
    /// The document contains an incomplete conflict block (e.g. a `<<<<<<<` with
    /// no matching `>>>>>>>`). The `state` field records the parser state at
    /// end-of-input for diagnostic purposes.
    Incomplete { state: String },
    /// The document mixes two different conflict markers. The `second_line` field
    /// is the 0-based line number of the opening `<<<<<<<` that belongs to the
    /// second (conflicting) format.
    MixedFormat { second_line: u32 },
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::Incomplete { state } => {
                write!(f, "incomplete conflict found: {state}")
            }
            ParseError::MixedFormat { second_line } => {
                write!(
                    f,
                    "file contains a mixed of conflict markers \
                     (second format begins at line {second_line})"
                )
            }
        }
    }
}

impl std::error::Error for ParseError {}

impl From<std::num::TryFromIntError> for ParseError {
    fn from(_: std::num::TryFromIntError) -> Self {
        ParseError::Incomplete {
            state: "line number exceeds u32".to_string(),
        }
    }
}

/// Strips exactly the marker prefix from a line, returning the label (if any).
/// Rejects lines where the marker is followed by a non-space character (e.g. 8+ repeated chars).
pub fn strip_marker<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(marker)?;
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(' ') {
        Some(rest.trim())
    } else {
        None
    }
}

/// Parse result for a document: a file-level enum identifying which VCS conflict
/// format was detected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MergeConflict {
    Diff3(diff3::VcsInfo),
    JjSnapshot(jj_snapshot::VcsInfo),
}

impl MergeConflict {
    pub fn count(&self) -> usize {
        match self {
            MergeConflict::Diff3(diff3) => diff3.conflicts().count(),
            MergeConflict::JjSnapshot(snap) => snap.conflicts.len(),
        }
    }

    pub fn diagnostics(&self) -> Vec<lsp_types::Diagnostic> {
        match self {
            MergeConflict::Diff3(diff3) => {
                diff3.conflicts().map(lsp_types::Diagnostic::from).collect()
            }
            MergeConflict::JjSnapshot(snap) => snap
                .conflicts
                .iter()
                .map(lsp_types::Diagnostic::from)
                .collect(),
        }
    }

    pub fn code_actions(
        &self,
        document_state: &DocumentState,
        params: lsp_types::CodeActionParams,
    ) -> Vec<CodeAction> {
        match self {
            MergeConflict::Diff3(diff3) => diff3.code_actions_at(
                &params.range,
                &params.text_document.uri,
                &document_state.document,
            ),
            MergeConflict::JjSnapshot(snapshot) => snapshot.code_actions_at(
                &params.range,
                &params.text_document.uri,
                &document_state.document,
            ),
        }
    }
}

/// Carries the first-seen file-level labels for diff3-style conflicts.
/// First-seen wins across all regions in a file.
pub struct FileLabels {
    pub head: Option<String>,
    pub branch: Option<String>,
    pub ancestor: Option<String>,
}

#[derive(Clone, Copy, PartialEq)]
enum ConflictFormat {
    Diff3,
    JjSnapshot,
}

fn commit_or_mixed(
    committed: &mut Option<ConflictFormat>,
    new_format: ConflictFormat,
    head_line: u32,
) -> Result<(), ParseError> {
    match committed {
        None => {
            *committed = Some(new_format);
            Ok(())
        }
        Some(existing) if *existing == new_format => Ok(()),
        Some(_) => Err(ParseError::MixedFormat {
            second_line: head_line,
        }),
    }
}

/// Classify the block by inspecting only the first line after `<<<<<<<`.
///
/// A jj snapshot conflict always begins with a `+++++++` side marker on the very
/// first line. Anything else is a diff3-style conflict; `diff3::parse_block`
/// validates the rest and reports `Incomplete` if it is malformed. Only the first
/// line is inspected so that marker-shaped lines appearing later in the conflict's
/// *content* (e.g. a `-------` or `=======` reStructuredText underline) are not
/// mistaken for structural markers.
fn classify(lines: &[&str], start: usize) -> Result<ConflictFormat, ParseError> {
    match lines.get(start) {
        Some(line)
            if line.as_bytes().first() == Some(&b'+')
                && strip_marker(line, MARKER_JJ_SNAPSHOT_SIDE).is_some() =>
        {
            Ok(ConflictFormat::JjSnapshot)
        }
        Some(_) => Ok(ConflictFormat::Diff3),
        None => Err(ParseError::Incomplete {
            state: format!("ExpectFirstInnerMarker({})", start.saturating_sub(1)),
        }),
    }
}

/// Parse all merge conflict regions from the given document text.
pub fn parse(text: &str) -> Result<Option<MergeConflict>, ParseError> {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    let mut committed_format: Option<ConflictFormat> = None;
    let mut diff3_regions: Vec<diff3::ConflictRegion> = Vec::new();
    let mut snapshot_regions: Vec<jj_snapshot::ConflictRegion> = Vec::new();
    let mut file_labels = FileLabels {
        head: None,
        branch: None,
        ancestor: None,
    };

    while i < lines.len() {
        let line = lines[i];
        if line.as_bytes().first() != Some(&b'<') {
            i += 1;
            continue;
        }
        let Some(head_label) = strip_marker(line, MARKER_HEAD) else {
            i += 1;
            continue;
        };
        let head_line: u32 = i.try_into()?;

        if !head_label.is_empty() && file_labels.head.is_none() {
            file_labels.head = Some(head_label.to_string());
        }
        tracing::debug!(
            "Found conflict head, {:?}, {:?}",
            file_labels.head,
            head_line
        );

        let format = classify(&lines, i + 1)?;
        commit_or_mixed(&mut committed_format, format, head_line)?;

        match format {
            ConflictFormat::Diff3 => {
                let (region, next) = diff3::parse_block(&lines, i, &mut file_labels)?;
                diff3_regions.push(region);
                i = next;
            }
            ConflictFormat::JjSnapshot => {
                let (region, next) = jj_snapshot::parse_block(&lines, i)?;
                snapshot_regions.push(region);
                i = next;
            }
        }
    }

    match (diff3_regions.is_empty(), snapshot_regions.is_empty()) {
        (true, true) => Ok(None),
        (false, true) => Ok(Some(MergeConflict::Diff3(diff3::VcsInfo {
            head: file_labels.head,
            branch: file_labels.branch,
            ancestor: file_labels.ancestor,
            conflicts: diff3_regions,
        }))),
        (true, false) => Ok(Some(MergeConflict::JjSnapshot(jj_snapshot::VcsInfo {
            conflicts: snapshot_regions,
        }))),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod test {
    use rstest::*;

    use super::*;
    use crate::jj_snapshot_conflict_text;
    use crate::test_helpers::TEXT_MIXED_FORMAT;
    #[allow(unused_imports)]
    use crate::test_helpers::init_logging;
    use crate::{conflict_text, diff3_conflict_text};

    #[rstest]
    fn incomplete_conflict_markers() {
        let text = "foo\n<<<<<<<\nbar\nbaz\n";
        let result = parse(text);
        assert!(result.is_err());
    }

    #[rstest]
    fn incomplete_conflict_returns_typed_parse_error() {
        // A `<<<<<<<` opener with no closing `=======` / `>>>>>>>` — the state machine
        // finishes in a non-Scanning state.
        let input = concat!("<<<<<<<", "\nhead content\n");
        let result = parse(input);
        assert!(
            matches!(result, Err(ParseError::Incomplete { .. })),
            "expected Err(ParseError::Incomplete {{ .. }}), got {:?}",
            result
        );
    }

    #[rstest]
    fn finds_conflict() {
        let input = concat!(
            "some test\n",
            conflict_text!("other text.\nmore text.", "replaced text.\nlast text."),
            "\nthe end.\n"
        );
        let MergeConflict::Diff3(merge_conflict) = parse(input).expect("successful parse").unwrap()
        else {
            panic!("expected MergeConflict::Diff3");
        };
        assert_eq!(1, merge_conflict.conflicts.len());
        let expected = diff3::ConflictRegion {
            head: 1,
            branch: 4,
            end: 7,
            ancestor: None,
        };
        assert_eq!(expected, merge_conflict.conflicts[0]);
    }

    #[rstest]
    fn finds_conflict_with_names() {
        let input = concat!(
            "some test\n",
            conflict_text!(
                "thing1",
                "other text.\nmore text.",
                "thing2",
                "replaced text.\nlast text."
            ),
            "\n",
            conflict_text!("thing1", "abcd\nefg\nhij", "thing2", "123\n456\n789"),
            "\nthe end.\n"
        );
        let MergeConflict::Diff3(merge_conflict) = parse(input)
            .expect("successful parse")
            .expect("a MergeConflict")
        else {
            panic!("expected MergeConflict::Diff3");
        };
        assert_eq!(2, merge_conflict.conflicts.len());
        let expected = diff3::ConflictRegion {
            head: 1,
            branch: 4,
            end: 7,
            ancestor: None,
        };
        assert_eq!(expected, merge_conflict.conflicts[0]);
        let expected = diff3::ConflictRegion {
            head: 9,
            branch: 13,
            end: 17,
            ancestor: None,
        };
        assert_eq!(expected, merge_conflict.conflicts[1]);
    }

    #[rstest]
    fn finds_diff3_conflict() {
        let input = concat!(
            "some test\n",
            diff3_conflict_text!(
                "other text.\nmore text.",
                "original text.",
                "replaced text.\nlast text."
            ),
            "\nthe end.\n",
        );
        tracing::debug!("input: {}", input);
        let MergeConflict::Diff3(merge_conflict) =
            parse(input).expect("unsuccessful parse").unwrap()
        else {
            panic!("expected MergeConflict::Diff3");
        };
        assert_eq!(1, merge_conflict.conflicts.len());
        let expected = diff3::ConflictRegion {
            head: 1,
            ancestor: Some(4),
            branch: 6,
            end: 9,
        };
        assert_eq!(expected, merge_conflict.conflicts[0]);
    }

    #[rstest]
    fn finds_diff3_conflict_with_names() {
        let input = concat!(
            "some test\n",
            diff3_conflict_text!(
                "original",
                "other text.\nmore text.",
                "ancestor",
                "original text.",
                "other",
                "replaced text.\nlast text."
            ),
            "\nthe end.\n",
        );
        let MergeConflict::Diff3(merge_conflict) =
            parse(input).expect("unsuccessful parse").unwrap()
        else {
            panic!("expected MergeConflict::Diff3");
        };
        assert_eq!(1, merge_conflict.conflicts.len());
        let expected = diff3::ConflictRegion {
            head: 1,
            ancestor: Some(4),
            branch: 6,
            end: 9,
        };
        assert_eq!(expected, merge_conflict.conflicts[0]);
    }

    /// A document with two consecutive snapshot blocks produces exactly
    /// two conflict regions.
    #[rstest]
    fn finds_multiple_jj_snapshot_conflicts() {
        let input = concat!(
            "before\n",
            jj_snapshot_conflict_text!(
                side "s1 \"a\"" => "line_a\n",
                base "b1 \"base\"" => "line_b\n",
                side "s2 \"b\"" => "line_c\n",
            ),
            "middle\n",
            jj_snapshot_conflict_text!(
                side "s3 \"c\"" => "line_d\n",
                base "b2 \"base2\"" => "line_e\n",
                side "s4 \"d\"" => "line_f\n",
            ),
            "after\n",
        );
        let result = parse(input).expect("parse should not error");
        let mc = result.expect("should find conflicts");
        let MergeConflict::JjSnapshot(snap) = mc else {
            panic!("expected JjSnapshot, got {:?}", mc);
        };
        assert_eq!(2, snap.conflicts.len());
    }

    /// A snapshot block missing its `>>>>>>>` closing marker returns
    /// `Err(ParseError::Incomplete { .. })`.
    #[rstest]
    fn jj_snapshot_conflict_incomplete_end_marker_errors() {
        // A `<<<<<<<` followed by `+++++++` but no `>>>>>>>`.
        let input = concat!(
            "<<<<<<<",
            " conflict\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " label\n",
            "content\n",
        );
        let result = parse(input);
        assert!(
            matches!(result, Err(ParseError::Incomplete { .. })),
            "expected Err(ParseError::Incomplete), got {:?}",
            result
        );
    }

    /// Two `-------` lines inside one snapshot conflict block are
    /// malformed; the parser must return `Err(ParseError::Incomplete { .. })`.
    #[rstest]
    fn jj_snapshot_conflict_malformed_double_base_errors() {
        let input = concat!(
            "<<<<<<<",
            " conflict\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " sideA\n",
            "content_a\n",
            concat!("-", "-", "-", "-", "-", "-", "-"),
            " base1\n",
            "base_content\n",
            concat!("-", "-", "-", "-", "-", "-", "-"),
            " base2\n",
            "base_content2\n",
            ">>>>>>>",
            " conflict ends\n",
        );
        let result = parse(input);
        assert!(
            matches!(result, Err(ParseError::Incomplete { .. })),
            "expected Err(ParseError::Incomplete), got {:?}",
            result
        );
    }

    /// A `-------` line appearing before any `+++++++` inside a
    /// `<<<<<<<` block is malformed; the parser must return
    /// `Err(ParseError::Incomplete { .. })`.
    #[rstest]
    fn jj_snapshot_conflict_malformed_base_before_side_errors() {
        let input = concat!(
            "<<<<<<<",
            " conflict\n",
            concat!("-", "-", "-", "-", "-", "-", "-"),
            " base_first\n",
            "base_content\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " sideA\n",
            "content_a\n",
            ">>>>>>>",
            " conflict ends\n",
        );
        let result = parse(input);
        assert!(
            matches!(result, Err(ParseError::Incomplete { .. })),
            "expected Err(ParseError::Incomplete), got {:?}",
            result
        );
    }

    /// A `<<<<<<<` whose first inner marker is `=======` is classified as
    /// a diff3-style conflict; the parser returns `MergeConflict::Diff3`, not
    /// `MergeConflict::JjSnapshot`.
    #[rstest]
    fn detection_routes_on_first_inner_marker_diff3() {
        let input = concat!(conflict_text!("head content", "branch content"),);
        let result = parse(input).expect("parse should not error");
        let mc = result.expect("should find a conflict");
        assert!(
            matches!(mc, MergeConflict::Diff3(_)),
            "expected MergeConflict::Diff3, got {:?}",
            mc
        );
    }

    /// A `<<<<<<<` whose first inner marker is `+++++++` is classified as
    /// a jj snapshot conflict; the parser returns `MergeConflict::JjSnapshot`,
    /// never `MergeConflict::Diff3`.
    #[rstest]
    fn detection_routes_on_first_inner_marker_snapshot() {
        let input = concat!(
            "<<<<<<<",
            " conflict\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " sideA\n",
            "content\n",
            ">>>>>>>",
            " conflict ends\n",
        );
        let result = parse(input).expect("parse should not error");
        let mc = result.expect("should find a conflict");
        assert!(
            matches!(mc, MergeConflict::JjSnapshot(_)),
            "expected MergeConflict::JjSnapshot, got {:?}",
            mc
        );
    }

    /// A file containing a diff3-style conflict block followed by a jj snapshot
    /// conflict block must return `Err(ParseError::MixedFormat { second_line })`
    /// where `second_line` is the 0-based line of the second `<<<<<<<` (the
    /// snapshot block opener). The fixture `TEXT_MIXED_FORMAT` places the
    /// snapshot opener at line 7.
    #[rstest]
    fn mixed_format_file_returns_mixed_format_error() {
        let result = parse(TEXT_MIXED_FORMAT);
        assert!(
            matches!(result, Err(ParseError::MixedFormat { second_line: 7 })),
            "expected Err(ParseError::MixedFormat {{ second_line: 7 }}), got {:?}",
            result
        );
    }

    /// A file containing a jj snapshot conflict block followed by a diff3-style
    /// conflict block must return `Err(ParseError::MixedFormat { second_line })`
    /// where `second_line` is the 0-based line of the second `<<<<<<<` (the
    /// diff3 block opener).
    ///
    /// Line layout of the reversed fixture (0-based):
    ///   0  "<<<<<<< conflict\n"            ← snapshot block head
    ///   1  "+++++++ sideA\n"
    ///   2  "snap_content\n"
    ///   3  ">>>>>>> conflict ends\n"       ← snapshot block end
    ///   4  "<<<<<<< HEAD\n"                ← second_line = 4 (diff3 block)
    ///   5  "head content\n"
    ///   6  "=======\n"
    ///   7  "branch content\n"
    ///   8  ">>>>>>> branch\n"
    #[rstest]
    fn mixed_format_file_returns_mixed_format_error_reversed() {
        let input = concat!(
            "<<<<<<<",
            " conflict\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " sideA\n",
            "snap_content\n",
            ">>>>>>>",
            " conflict ends\n",
            conflict_text!("HEAD", "head content", "branch", "branch content"),
        );
        let result = parse(input);
        assert!(
            matches!(result, Err(ParseError::MixedFormat { second_line: 4 })),
            "expected Err(ParseError::MixedFormat {{ second_line: 4 }}), got {:?}",
            result
        );
    }

    /// A `-------` line inside the HEAD content of a diff3 conflict is ordinary
    /// content, not a structural marker. The conflict must still be detected and
    /// the separator must be the real `=======`, not the `-------` line.
    /// (reStructuredText section underlines are a common real-world source.)
    #[rstest]
    fn diff3_dashes_in_head_content_treated_as_content() {
        let input = concat!(
            "<<<<<<<",
            " HEAD\n",
            "Section\n",
            concat!("-", "-", "-", "-", "-", "-", "-"),
            "\n",
            "more head\n",
            concat!("=", "=", "=", "=", "=", "=", "="),
            "\n",
            "branch\n",
            ">>>>>>>",
            " feature\n",
        );
        let MergeConflict::Diff3(info) = parse(input).expect("parse ok").expect("a conflict")
        else {
            panic!("expected diff3");
        };
        assert_eq!(1, info.conflicts.len());
        let r = &info.conflicts[0];
        assert_eq!((r.head, r.branch, r.ancestor, r.end), (0, 4, None, 6));
    }

    /// A `+++++++` line that is NOT the first inner line is HEAD content, so the
    /// block is still classified as diff3 (jj snapshots only begin with a side
    /// marker on the very first line).
    #[rstest]
    fn diff3_plus_in_head_content_treated_as_content() {
        let input = concat!(
            "<<<<<<<",
            " HEAD\n",
            "patch:\n",
            concat!("+", "+", "+", "+", "+", "+", "+"),
            "\n",
            concat!("=", "=", "=", "=", "=", "=", "="),
            "\n",
            "branch\n",
            ">>>>>>>",
            " feature\n",
        );
        let MergeConflict::Diff3(info) = parse(input).expect("parse ok").expect("a conflict")
        else {
            panic!("expected diff3, got non-diff3");
        };
        assert_eq!(1, info.conflicts.len());
        let r = &info.conflicts[0];
        assert_eq!((r.head, r.branch, r.end), (0, 3, 5));
    }
}
