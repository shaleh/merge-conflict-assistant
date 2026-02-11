/*
Let's assume this marker is at line 100.
<<<<<<< (HEAD: we record about the line number of this line)
content
content
\\\\\\\ (ANCESTOR: this is diff3 style only. line number is captured)
content
content
======= (BRANCH: we record the line number)
content
content
>>>>>>> (END: we record the line number and the last character position)

in each case if a branch or other name is provided it is remembered and will be shown as part
of the action and possibly diagnostic.

Content is extracted by getting the lines after the first marker and before the last marker.

The numbers stored are 0-based indexes. Line 100 is returned as 99.
*/

// Region in a conflict.
//
// Defined by the line of the relevant marker.
//
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
}

#[derive(Debug)]
enum ParseState {
    Scanning,
    ExpectAncestorOrBranch(u32),
    ExpectEnd(u32, u32),
    ExpectBranchFromAncestor(u32, u32),
    ExpectEndWithAncestor(u32, u32, u32),
}

pub fn parse(_uri: &lsp_types::Uri, text: &str) -> anyhow::Result<Option<MergeConflict>> {
    let mut conflicts = Vec::new();
    let mut state = ParseState::Scanning;

    // Only need to capture the first name for each marker. The names are the same in each region.
    let mut head_name = None;
    let mut ancestor_name = None;
    let mut branch_name = None;

    for (lineno, line) in text.lines().enumerate() {
        match state {
            ParseState::Scanning => {
                if let Some(name) = line.strip_prefix("<<<<<<<").map(str::trim) {
                    let head = lineno.try_into()?;
                    if !name.is_empty() && head_name.is_none() {
                        head_name.replace(name);
                    }
                    log::debug!("Found conflict, {:?}, {:?}", head_name, head);
                    state = ParseState::ExpectAncestorOrBranch(head);
                }
            }
            ParseState::ExpectAncestorOrBranch(head) => {
                if let Some(name) = line.strip_prefix("|||||||").map(str::trim) {
                    let ancestor = lineno.try_into()?;
                    if !name.is_empty() && ancestor_name.is_none() {
                        ancestor_name.replace(name);
                    }
                    log::debug!("Found ancestor, {:?}, {:?}", ancestor_name, ancestor);
                    state = ParseState::ExpectBranchFromAncestor(head, ancestor);
                } else if line == "=======" {
                    let branch = lineno.try_into()?;
                    log::debug!("Found branch, {:?}", branch);
                    state = ParseState::ExpectEnd(head, branch);
                }
            }
            ParseState::ExpectEnd(head, branch) => {
                if let Some(name) = line.strip_prefix(">>>>>>>").map(str::trim) {
                    if !name.is_empty() && branch_name.is_none() {
                        branch_name.replace(name);
                    }
                    log::debug!("Found end, {:?} {:?}", branch_name, lineno);
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
                if line == "=======" {
                    let branch = lineno.try_into()?;
                    log::debug!("Found branch, {:?}", branch);
                    state = ParseState::ExpectEndWithAncestor(head, ancestor, branch);
                }
            }
            ParseState::ExpectEndWithAncestor(head, ancestor, branch) => {
                if let Some(name) = line.strip_prefix(">>>>>>>").map(str::trim) {
                    if !name.is_empty() && branch_name.is_none() {
                        branch_name.replace(name);
                    }
                    log::debug!("Found end, {:?} {:?}", branch_name, lineno);
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
    pub fn is_in_range(&self, range: &lsp_types::Range) -> bool {
        /*
        range is one line: is line inside the conflict?
        range is one line more than the conflict but only slightly
        conflict overlaps with range
        */
        self.head <= range.start.line
            && self.end >= range.start.line
            && self.end + 1 >= range.end.line
    }
}

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
    use super::*;
    use rstest::*;

    #[rstest]
    fn incomplete_conflict_markers(uri: lsp_types::Uri) {
        let text = "
foo
<<<<<<<
bar
baz
";
        let result = parse(&uri, text);
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

    #[fixture]
    fn uri() -> lsp_types::Uri {
        "file://foo.txt".parse().unwrap()
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
    fn finds_conflict(uri: lsp_types::Uri) {
        let input = "some test
<<<<<<<
    other text.
    more text.
=======
    replaced text.
    last text.
>>>>>>>

the end.
";
        let merge_conflict = parse(&uri, input).expect("unsuccessful parse").unwrap();
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
    fn finds_conflict_with_names(uri: lsp_types::Uri) {
        let input = "some test
<<<<<<< thing1
    other text.
    more text.
=======
    replaced text.
    last text.
>>>>>>> thing2

<<<<<<< thing1
    abcd
    efg
    hij
=======
    123
    456
    789
>>>>>>> thing2

the end.
";
        let merge_conflict = parse(&uri, input).expect("unsuccessful parse").unwrap();
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
    fn finds_diff3_conflict(uri: lsp_types::Uri) {
        let input = "some test
<<<<<<<
    other text.
    more text.
|||||||
    original text.
=======
    replaced text.
    last text.
>>>>>>>

the end.
";
        let merge_conflict = parse(&uri, input).expect("unsuccessful parse").unwrap();
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
    fn finds_diff3_conflict_with_names(uri: lsp_types::Uri) {
        let input = "some test
<<<<<<< original
    other text.
    more text.
||||||| ancestor
    original text.
=======
    replaced text.
    last text.
>>>>>>> other

the end.
";
        let merge_conflict = parse(&uri, input).expect("unsuccessful parse").unwrap();
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
