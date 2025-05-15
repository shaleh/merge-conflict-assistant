use std::iter;
use std::sync::LazyLock;

use itertools::izip;
use regex::Regex;

/*
Let's assume this marker is at line 100.
<<<<<<< (OURS: we record about the line number of this line)
content
content
\\\\\\\ (ANCESTOR: this is diff3 style only. line number is captured)
content
content
======= (THEIRS: we record the line number)
content
content
>>>>>>> (END: we record the line number and the last character position)

in each case if a branch or other name is provided it is remembered and will be shown as part
of the action and possibly diagnostic.

Content is extracted by getting the lines after the first marker and before the last marker.

ours start: line: 100
       end: line: 103
      data: content\ncontent\n
ancestor start: line 103
           end: line 106
          data: content\ncontent\n
theirs start: line 106
         end: line 109
        data: content\content\n
conflict end: line 109, character: 7 + 1 (space) + name
*/

// Region in a conflict.
//
// Defined by a start and end both are line numbers.
// Name is the branch or other identifier associated with the conflict marker.
//
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ConflictRegion {
    // u32 matches Position and Range in lsp_types and the LSP spec.
    pub start: u32,
    pub end: u32,
    pub name: Option<String>,
}

impl From<(u32, u32, &str)> for ConflictRegion {
    fn from((start, end, name): (u32, u32, &str)) -> Self {
        let name = if name.is_empty() {
            None
        } else {
            Some(name.to_string())
        };
        ConflictRegion { start, end, name }
    }
}

// Merge conflict information.
//
// A conflict has an ours and a theirs and in the case of diff3 also an ancestor.
//
// The whole region spans from character zero at line start to the value of last_character on line end.
//
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub ours: ConflictRegion,
    pub theirs: ConflictRegion,
    pub ancestor: Option<ConflictRegion>,
    last_char: u32,
}

impl Conflict {
    pub fn new(
        ours: (u32, u32, &str),
        theirs: (u32, u32, &str),
        last_char: u32,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            ours: ours.into(),
            theirs: theirs.into(),
            ancestor: None,
            last_char,
        })
    }

    pub fn new_with_ancestor(
        ours: (u32, u32, &str),
        theirs: (u32, u32, &str),
        ancestor: (u32, u32, &str),
        last_char: u32,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            ours: ours.into(),
            theirs: theirs.into(),
            ancestor: Some(ancestor.into()),
            last_char,
        })
    }

    pub fn start(&self) -> lsp_types::Position {
        lsp_types::Position {
            line: self.ours.start,
            character: 0,
        }
    }

    pub fn end(&self) -> lsp_types::Position {
        lsp_types::Position {
            line: self.theirs.end,
            character: self.last_char,
        }
    }

    pub fn is_in_range(&self, range: &lsp_types::Range) -> bool {
        /*
        range is one line: is line inside the conflict?
        range is one line more than the conflict but only slightly
        conflict overlaps with range
        */
        if self.ours.start > range.start.line || self.theirs.end < range.start.line {
            return false;
        }
        let end = range.end;
        let conflict_end = self.theirs.end;
        if end.character == 0 && conflict_end >= end.line - 1 {
            return true;
        }
        self.end() >= end
    }
}

pub fn range_for_diagnostic_conflict(conflict: &Conflict) -> lsp_types::Range {
    let mut end = conflict.end();
    // This is a prodoct of the code action not wanting to leave a dangling new line behind.
    end.line += 1;
    end.character = 0;
    lsp_types::Range {
        start: conflict.start(),
        end,
    }
}

impl From<&Conflict> for lsp_types::Diagnostic {
    fn from(conflict: &Conflict) -> Self {
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

#[derive(Debug, Default)]
pub struct Parser {}

static OURS_BEGIN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^<<<<<<<.*$").unwrap());
static THEIRS_BEGIN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^=======.*$").unwrap());
static ANCESTOR_BEGIN_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?m)^\|\|\|\|\|\|\|.*$").unwrap());
static MARKER_BEGIN_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?m)^>>>>>>>.*$").unwrap());

impl Parser {
    pub fn parse(uri: &lsp_types::Uri, text: &str) -> anyhow::Result<Option<Vec<Conflict>>> {
        log::debug!("parsing: {:?}", uri);
        log::debug!("'{}'", text);

        let ours_matches = OURS_BEGIN_RE.find_iter(text);
        let theirs_matches = THEIRS_BEGIN_RE.find_iter(text);
        let ancestor_matches = ANCESTOR_BEGIN_RE.find_iter(text);
        let marker_matches = MARKER_BEGIN_RE.find_iter(text);
        let newlines: Vec<usize> = text
            .chars()
            .enumerate()
            .filter_map(|(i, c)| if c == '\n' { Some(i) } else { None })
            .collect();

        macro_rules! line_from_match {
            ($pos:expr) => {{
                // The regex match returns the character position. We need the line number.
                // newlines has the character position of each newline.
                let tmp = match newlines.binary_search(&$pos) {
                    Ok(value) => value,
                    Err(value) => value,
                };
                tmp.try_into().expect("failed to cast to 32 bit value")
            }};
        }
        let mut conflicts = Vec::new();
        for (ours, ancestor_, theirs, marker) in izip!(
            ours_matches,
            // ancestor is optional, only present in diff3 format.
            ancestor_matches.map(Some).chain(iter::repeat(None)),
            theirs_matches,
            marker_matches,
        ) {
            let ours_start = line_from_match!(ours.start());
            let ours_name = ours.as_str()[7..].trim();
            let theirs_start = line_from_match!(theirs.start());
            let marker_end = line_from_match!(marker.end());
            let theirs_name = marker.as_str()[7..].trim();
            if let Some(ancestor) = ancestor_ {
                let ancestor_start = line_from_match!(ancestor.start());
                let ancestor_name = ancestor.as_str()[7..].trim();
                let conflict = Conflict::new_with_ancestor(
                    (ours_start, ancestor_start, ours_name),
                    (theirs_start, marker_end, theirs_name),
                    (ancestor_start, theirs_start, ancestor_name),
                    marker
                        .as_str()
                        .len()
                        .try_into()
                        .expect("failed to cast to 32 bit value"),
                )?;
                conflicts.push(conflict);
            } else {
                let conflict = Conflict::new(
                    (ours_start, theirs_start, ours_name),
                    (theirs_start, marker_end, theirs_name),
                    marker
                        .as_str()
                        .len()
                        .try_into()
                        .expect("failed to cast to 32 bit value"),
                )?;
                conflicts.push(conflict);
            }
        }

        Ok(Some(conflicts))
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
        let result = Parser::parse(&uri, text);
        let conflicts = result.unwrap().unwrap();
        assert!(conflicts.is_empty());
    }

    #[fixture]
    fn conflict() -> Conflict {
        Conflict::new_with_ancestor((4, 6, ""), (10, 12, ""), (7, 9, ""), 80).unwrap()
    }

    #[fixture]
    fn uri() -> lsp_types::Uri {
        "file://foo.txt".parse().unwrap()
    }

    #[rstest]
    fn range_one_line_is_in_conflict(conflict: Conflict) {
        for x in conflict.start().line..=conflict.end().line {
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
    fn range_one_line_is_not_in_conflict(conflict: Conflict) {
        for x in [conflict.start().line - 1, conflict.end().line + 1] {
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
    fn range_matching_conflict_is_in_conflict(conflict: Conflict) {
        let range = range_for_diagnostic_conflict(&conflict);
        assert!(conflict.is_in_range(&range), "{conflict:?} v. {range:?}");
    }

    #[rstest]
    fn range_wider_than_conflict_is_not_in_conflict(conflict: Conflict) {
        let range = lsp_types::Range {
            start: lsp_types::Position {
                line: conflict.start().line - 3,
                character: 0,
            },
            end: lsp_types::Position {
                line: conflict.end().line + 3,
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
        let conflicts = Parser::parse(&uri, input)
            .expect("unsuccessful parse")
            .unwrap();
        assert_eq!(1, conflicts.len());
        let expected = Conflict::new((1, 4, ""), (4, 7, ""), 7).unwrap();
        assert_eq!(expected, conflicts[0]);
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
        let conflicts = Parser::parse(&uri, input)
            .expect("unsuccessful parse")
            .unwrap();
        assert_eq!(2, conflicts.len());
        let expected = Conflict::new((1, 4, "thing1"), (4, 7, "thing2"), 14).unwrap();
        assert_eq!(expected, conflicts[0]);
        let expected = Conflict::new((9, 13, "thing1"), (13, 17, "thing2"), 14).unwrap();
        assert_eq!(expected, conflicts[1]);
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
        let conflicts = Parser::parse(&uri, input)
            .expect("unsuccessful parse")
            .unwrap();
        assert_eq!(1, conflicts.len());
        let expected = Conflict::new_with_ancestor((1, 4, ""), (6, 9, ""), (4, 6, ""), 7).unwrap();
        assert_eq!(expected, conflicts[0]);
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
        let conflicts = Parser::parse(&uri, input)
            .expect("unsuccessful parse")
            .unwrap();
        assert_eq!(1, conflicts.len());
        let expected = Conflict::new_with_ancestor(
            (1, 4, "original"),
            (6, 9, "other"),
            (4, 6, "ancestor"),
            13,
        )
        .unwrap();
        assert_eq!(expected, conflicts[0]);
    }
}
