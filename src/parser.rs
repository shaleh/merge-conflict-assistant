// Region in a conflict.
//
// Defined by a start and end.
// Name is the branch or other identifier associated with the conflict marker.
//
// The values are optional to allow partial building by the parser. In reality,
// only the name is truly optional.
//
#[derive(Debug, Clone)]
pub struct ConflictRegion {
    start: Option<u32>,
    end: Option<u32>,
    name: Option<String>,
}

// Merge conflict information.
//
// A conflict has an ours and a theirs and in the case of diff3 also an ancestor.
//
#[derive(Debug, Clone)]
pub struct Conflict {
    ours: ConflictRegion,
    theirs: ConflictRegion,
    ancestor: Option<ConflictRegion>,
}

impl Conflict {
    pub fn start(&self) -> u32 {
        self.ours.start.unwrap()
    }

    pub fn end(&self) -> u32 {
        self.theirs.end.unwrap() + 1
    }

    pub fn is_in_range(&self, range: lsp_types::Range) -> bool {
        self.start() <= range.start.line && self.end() >= range.end.line
    }
}

impl From<&Conflict> for lsp_types::Range {
    fn from(conflict: &Conflict) -> Self {
        Self {
            start: lsp_types::Position {
                line: conflict.start(),
                character: 0,
            },
            end: lsp_types::Position {
                line: conflict.end(),
                character: 0,
            },
        }
    }
}

impl From<&Conflict> for lsp_types::Diagnostic {
    fn from(conflict: &Conflict) -> Self {
        let range = lsp_types::Range::from(conflict);
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
pub struct Parser {
    conflicts: Vec<Conflict>,
    ours: Option<ConflictRegion>,
    theirs: Option<ConflictRegion>,
    ancestor: Option<ConflictRegion>,
}

impl Parser {
    pub fn parse(&mut self, document: &lsp_types::TextDocumentItem) -> Vec<Conflict> {
        log::debug!("parsing: {:?}", document.uri);

        for (number, line) in document.text.lines().enumerate() {
            let result = if let Some(rest) = line.strip_prefix("<<<<<<<") {
                self.on_new_conflict(number.try_into().unwrap(), rest.trim())
            } else if let Some(rest) = line.strip_prefix("|||||||") {
                self.on_enter_ancestor(number.try_into().unwrap(), rest.trim())
            } else if line.starts_with("=======") {
                self.on_enter_theirs(number.try_into().unwrap())
            } else if let Some(rest) = line.strip_prefix(">>>>>>>") {
                self.on_leave_theirs(number.try_into().unwrap(), rest.trim())
            } else {
                Ok(())
            };
            if let Err(message) = result {
                log::warn!("{}: {}", message, number);
            }
        }
        self.conflicts.clone()
    }

    fn on_new_conflict(&mut self, number: u32, name: &str) -> anyhow::Result<()> {
        if self.ours.is_some() {
            self.ours = None;
            anyhow::bail!("found an unterminated conflict marker");
        }
        self.ours.replace(ConflictRegion {
            start: Some(number),
            end: None,
            name: Some(name.to_owned()),
        });
        log::debug!("start ours {}: {:?}", number, self.ours);
        Ok(())
    }

    fn on_leave_ours(&mut self, number: u32) -> anyhow::Result<()> {
        if let Some(ours_) = self.ours.as_mut() {
            if ours_.end.is_none() {
                ours_.end.replace(number);
            }
        } else {
            anyhow::bail!("unexpected end of OURS region");
        }
        Ok(())
    }

    fn on_enter_ancestor(&mut self, number: u32, name: &str) -> anyhow::Result<()> {
        if let Some(ours_) = self.ours.as_mut() {
            ours_.end.replace(number);
        } else {
            anyhow::bail!("Found ancestor marker, but no active conflict");
        }
        self.ancestor.replace(ConflictRegion {
            start: Some(number),
            end: None,
            name: Some(name.to_owned()),
        });
        log::debug!("start ancestor {}: {:?}", number, self.ancestor);
        Ok(())
    }

    fn on_leave_ancestor(&mut self, number: u32) -> anyhow::Result<()> {
        if let Some(ancestor_) = self.ancestor.as_mut() {
            if ancestor_.end.is_none() {
                ancestor_.end.replace(number);
            }
        }

        Ok(())
    }

    fn on_enter_theirs(&mut self, number: u32) -> anyhow::Result<()> {
        self.on_leave_ours(number)?;
        self.on_leave_ancestor(number)?;
        if self.theirs.is_some() {
            anyhow::bail!("found THEIRS marker, expected conflict end marker");
        }
        self.theirs.replace(ConflictRegion {
            start: Some(number),
            end: None,
            name: None,
        });
        log::debug!("start theirs {}", number);
        Ok(())
    }

    fn reset_state(&mut self) {
        self.ours = None;
        self.theirs = None;
        if self.ancestor.is_some() {
            self.ancestor = None;
        }
    }

    fn on_leave_theirs(&mut self, number: u32, name: &str) -> anyhow::Result<()> {
        if let Some(theirs_) = self.theirs.as_mut() {
            theirs_.end.replace(number);
            theirs_.name.replace(name.to_owned());
        } else {
            self.reset_state();
            anyhow::bail!("unexpected end of conflict marker");
        }
        log::debug!("end theirs {}: {:?}", number, self.theirs);
        if let (Some(ours_), Some(theirs_)) = (self.ours.as_ref(), self.theirs.as_ref()) {
            self.conflicts.push(Conflict {
                ours: ours_.clone(),
                theirs: theirs_.clone(),
                ancestor: self.ancestor.clone(),
            });
        }
        self.reset_state();
        Ok(())
    }
}
