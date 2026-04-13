//! Single-pass state-machine parser for merge conflict markers.
//!
//! Recognizes standard and diff3-style conflicts by scanning for the four
//! marker prefixes (`<<<<<<<`, `|||||||`, `=======`, `>>>>>>>`). Branch and
//! ancestor names following the markers are captured when present.
//!
//! All line numbers stored are 0-based indexes (line 100 in the file is stored as 99).
//! Content for a region is the lines *after* its opening marker and *before* its
//! closing marker.

pub const MARKER_HEAD: &str = "<<<<<<<";
pub const MARKER_ANCESTOR: &str = "|||||||";
pub const MARKER_SEPARATOR: &str = "=======";
pub const MARKER_END: &str = ">>>>>>>";

/// Strips exactly the marker prefix from a line, returning the label (if any).
/// Rejects lines where the marker is followed by a non-space character (e.g. 8+ repeated chars).
fn strip_marker<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(marker)?;
    if rest.is_empty() {
        Some("")
    } else if rest.starts_with(' ') {
        Some(rest.trim())
    } else {
        None
    }
}

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
    pub fn head_range(&self) -> (u32, u32) {
        let end = self.ancestor.unwrap_or(self.branch);
        (self.head, end)
    }

    pub fn branch_range(&self) -> (u32, u32) {
        (self.branch, self.end)
    }

    pub fn ancestor_range(&self) -> Option<(u32, u32)> {
        self.ancestor.map(|pos| (pos, self.branch))
    }
}

/// Parse result for a document: the branch/ancestor names and all conflict regions found.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MergeConflict {
    pub head: Option<String>,
    pub branch: Option<String>,
    pub ancestor: Option<String>,
    pub conflicts: Vec<ConflictRegion>,
}

impl MergeConflict {
    pub fn conflicts(&self) -> impl Iterator<Item = &ConflictRegion> {
        self.conflicts.iter()
    }

    #[allow(unused)]
    pub fn exists(&self) -> bool {
        !self.conflicts.is_empty()
    }
}

#[derive(Debug)]
enum ParseState {
    Scanning,
    ExpectAncestorOrBranch(u32),
    ExpectEnd(u32, u32),
    ExpectBranchFromAncestor(u32, u32),
    ExpectEndWithAncestor(u32, u32, u32),
}

/// Parse all merge conflict regions from the given document text.
pub fn parse(text: &str) -> anyhow::Result<Option<MergeConflict>> {
    let mut conflicts = Vec::new();
    let mut state = ParseState::Scanning;

    // Only need to capture the first name for each marker. The names are the same in each region.
    let mut head_name = None;
    let mut ancestor_name = None;
    let mut branch_name = None;

    for (lineno, line) in text.lines().enumerate() {
        let first = line.as_bytes().first();
        match state {
            ParseState::Scanning => {
                if first == Some(&b'<')
                    && let Some(name) = strip_marker(line, MARKER_HEAD)
                {
                    let head = lineno.try_into()?;
                    if !name.is_empty() && head_name.is_none() {
                        head_name.replace(name);
                    }
                    tracing::debug!("Found conflict, {:?}, {:?}", head_name, head);
                    state = ParseState::ExpectAncestorOrBranch(head);
                }
            }
            ParseState::ExpectAncestorOrBranch(head) => {
                if first == Some(&b'|')
                    && let Some(name) = strip_marker(line, MARKER_ANCESTOR)
                {
                    let ancestor = lineno.try_into()?;
                    if !name.is_empty() && ancestor_name.is_none() {
                        ancestor_name.replace(name);
                    }
                    tracing::debug!("Found ancestor, {:?}, {:?}", ancestor_name, ancestor);
                    state = ParseState::ExpectBranchFromAncestor(head, ancestor);
                } else if first == Some(&b'=') && line == MARKER_SEPARATOR {
                    let branch = lineno.try_into()?;
                    tracing::debug!("Found branch, {:?}", branch);
                    state = ParseState::ExpectEnd(head, branch);
                }
            }
            ParseState::ExpectEnd(head, branch) => {
                if first == Some(&b'>')
                    && let Some(name) = strip_marker(line, MARKER_END)
                {
                    if !name.is_empty() && branch_name.is_none() {
                        branch_name.replace(name);
                    }
                    tracing::debug!("Found end, {:?} {:?}", branch_name, lineno);
                    conflicts.push(ConflictRegion {
                        head,
                        branch,
                        ancestor: None,
                        end: lineno.try_into()?,
                    });
                    state = ParseState::Scanning;
                }
            }
            ParseState::ExpectBranchFromAncestor(head, ancestor) => {
                if first == Some(&b'=') && line == "=======" {
                    let branch = lineno.try_into()?;
                    tracing::debug!("Found branch, {:?}", branch);
                    state = ParseState::ExpectEndWithAncestor(head, ancestor, branch);
                }
            }
            ParseState::ExpectEndWithAncestor(head, ancestor, branch) => {
                if first == Some(&b'>')
                    && let Some(name) = strip_marker(line, MARKER_END)
                {
                    if !name.is_empty() && branch_name.is_none() {
                        branch_name.replace(name);
                    }
                    tracing::debug!("Found end, {:?} {:?}", branch_name, lineno);
                    conflicts.push(ConflictRegion {
                        head,
                        branch,
                        ancestor: Some(ancestor),
                        end: lineno.try_into()?,
                    });
                    state = ParseState::Scanning;
                }
            }
        }
    }
    if !matches!(state, ParseState::Scanning) {
        tracing::warn!("incomplete conflict found: {:?}", state);
        anyhow::bail!("Error: incomplete conflict found: {:?}", state);
    }

    if conflicts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(MergeConflict {
            head: head_name.map(String::from),
            branch: branch_name.map(String::from),
            ancestor: ancestor_name.map(String::from),
            conflicts,
        }))
    }
}

impl ConflictRegion {
    /// Returns true if the given LSP range overlaps with this conflict.
    ///
    /// The range must start within the conflict region. A range that begins
    /// before the conflict is rejected — this avoids matching when the user
    /// has selected across multiple conflicts or only part of one.
    pub fn is_in_range(&self, range: &lsp_types::Range) -> bool {
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

/// Build the LSP range covering the entire conflict, including the end marker line.
///
/// The range extends to `end + 1` so that applying a replacement removes the
/// trailing newline of the end marker rather than leaving a blank line behind.
pub fn range_for_diagnostic_conflict(conflict: &ConflictRegion) -> lsp_types::Range {
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

#[cfg(test)]
mod test {
    use rstest::*;

    use super::*;
    #[allow(unused_imports)]
    use crate::test_helpers::init_logging;
    use crate::{conflict_text, diff3_conflict_text};

    #[rstest]
    fn incomplete_conflict_markers() {
        let text = "foo\n<<<<<<<\nbar\nbaz\n";
        let result = parse(text);
        assert!(result.is_err());
    }

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

    #[rstest]
    fn finds_conflict() {
        let input = concat!(
            "some test\n",
            conflict_text!("other text.\nmore text.", "replaced text.\nlast text."),
            "\nthe end.\n"
        );
        let merge_conflict = parse(input).expect("successful parse").unwrap();
        assert_eq!(1, merge_conflict.conflicts.len());
        let expected = ConflictRegion {
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
        let merge_conflict = parse(input)
            .expect("successful parse")
            .expect("a MergeConflict");
        assert_eq!(2, merge_conflict.conflicts.len());
        let expected = ConflictRegion {
            head: 1,
            branch: 4,
            end: 7,
            ancestor: None,
        };
        assert_eq!(expected, merge_conflict.conflicts[0]);
        let expected = ConflictRegion {
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
        let merge_conflict = parse(input).expect("unsuccessful parse").unwrap();
        assert_eq!(1, merge_conflict.conflicts.len());
        let expected = ConflictRegion {
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
        let merge_conflict = parse(input).expect("unsuccessful parse").unwrap();
        assert_eq!(1, merge_conflict.conflicts.len());
        let expected = ConflictRegion {
            head: 1,
            ancestor: Some(4),
            branch: 6,
            end: 9,
        };
        assert_eq!(expected, merge_conflict.conflicts[0]);
    }
}
