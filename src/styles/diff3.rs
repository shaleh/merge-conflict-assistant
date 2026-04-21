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
}
