use std::sync::{Arc, Mutex};

use crossbeam_channel::unbounded;
use rstest::*;

use crate::{conflict_text, parser::MergeConflict, state::ServerState, styles::diff3};

pub const TEXT1_RESOLVED: &str = "
This is some
plain old
text.
Nothing to see here.
";

pub const TEXT1_WITH_CONFLICTS: &str = concat!(
    "\nThis is some\n",
    conflict_text!("OURS", "plain old", "THEIRS", "new and improved"),
    "text.\n",
    conflict_text!("OURS", "Nothing to see here.", "THEIRS", "Cool stuff."),
    "\nFinal text",
);

pub const TEXT2_WITH_CONFLICTS: &str = concat!(
    "\nThis is some\n",
    conflict_text!("plain old", "new and improved"),
    "text.\n",
    conflict_text!("Nothing to see here.", "Cool stuff."),
    "\nFinal text\n",
);

pub const TEXT2_RESOLVED: &str = "
This is some
plain old
text.
Cool stuff.
";

#[fixture]
pub fn uri() -> lsp_types::Uri {
    "file://foo.txt".parse().unwrap()
}

#[fixture]
pub fn version(#[default(0)] value: i32) -> i32 {
    value
}

#[fixture]
pub fn state() -> ServerState {
    let (_, reader_receiver) = unbounded::<lsp_server::Message>();
    let (writer_sender, _) = unbounded::<lsp_server::Message>();
    let connection = lsp_server::Connection {
        sender: writer_sender,
        receiver: reader_receiver,
    };
    ServerState::new(connection.sender)
}

#[fixture]
pub fn populated_state(
    version: i32,
    #[default("")] text: &str,
    #[default(None)] merge_conflict: Option<MergeConflict>,
) -> ServerState {
    use crate::state::DocumentState;

    let state = state();
    {
        let mut documents = state.documents.lock().unwrap();
        documents.insert(
            uri(),
            Arc::new(Mutex::new(match merge_conflict {
                Some(conflict) => {
                    DocumentState::new_with_conflict(text.to_string(), version, conflict)
                }
                None => DocumentState::new(text.to_string(), version),
            })),
        );
    }
    state
}

#[fixture]
#[once]
pub fn conflicts_for_text2_with_conflicts() -> MergeConflict {
    MergeConflict::Diff3(diff3::VcsInfo {
        head: None,
        branch: None,
        ancestor: None,
        conflicts: vec![
            diff3::ConflictRegion {
                head: 2,
                branch: 4,
                end: 6,
                ancestor: None,
            },
            diff3::ConflictRegion {
                head: 8,
                branch: 10,
                end: 12,
                ancestor: None,
            },
        ],
    })
}

/// Assembles a jj snapshot conflict block from an arbitrary sequence of side and base entries.
///
/// Each entry is a `(kind, label, content)` triple where `kind` is `"side"` or `"base"`.
/// The macro emits the `+++++++` or `-------` marker line, followed by the content.
/// Surrounding `<<<<<<<` / `>>>>>>>` lines are added automatically.
///
/// Because literal `+++++++` and `-------` tokens in Rust source files would confuse
/// the parser if it ever scanned its own tree, all marker tokens are assembled via
/// `concat!` rather than written verbatim.
///
/// Usage:
/// ```ignore
/// jj_snapshot_conflict_text!(
///     side "abc \"first\"" => "apple\norange\n",
///     base "base123 \"ancestor\"" => "apple\ngrape\n",
///     side "def \"second\"" => "APPLE\nORANGE\n",
/// )
/// ```
#[macro_export]
macro_rules! jj_snapshot_conflict_text {
    // Internal rule: expand a single entry to its marker + content lines.
    (@entry side $label:expr => $content:expr) => {
        concat!(
            concat!("+", "+", "+", "+", "+", "+", "+"),
            " ", $label, "\n",
            $content
        )
    };
    (@entry base $label:expr => $content:expr) => {
        concat!(
            concat!("-", "-", "-", "-", "-", "-", "-"),
            " ", $label, "\n",
            $content
        )
    };
    // Public rule: wrap all entries in the open/close markers.
    ( $( $kind:ident $label:expr => $content:expr ),+ $(,)? ) => {
        concat!(
            "<<<<<<<", " conflict\n",
            $(
                $crate::jj_snapshot_conflict_text!(@entry $kind $label => $content),
            )+
            ">>>>>>>", " conflict ends\n",
        )
    };
}

/// A document that contains one diff3-style conflict block followed by one jj snapshot
/// conflict block. The parser rejects this combination with
/// `Err(ParseError::MixedFormat { second_line: 7 })`.
///
/// Line layout (0-based):
///   0  "context before\n"
///   1  "<<<<<<< HEAD\n"               ← diff3 block head
///   2  "head content\n"
///   3  "=======\n"
///   4  "branch content\n"
///   5  ">>>>>>> branch\n"             ← diff3 block end
///   6  "context between\n"
///   7  "<<<<<<< conflict\n"           ← snapshot block open  (second_line = 7)
///   8  "+++++++ sideA \"snap\"\n"
///   9  "snapshot_a\n"
///  10  ">>>>>>> conflict ends\n"      ← snapshot block end
///  11  "context after\n"
///
/// `second_line` = 7 (the 0-based line of the second `<<<<<<<`, which opens the
/// snapshot block and is the first marker that conflicts with the already-established
/// diff3 format).
pub const TEXT_MIXED_FORMAT: &str = concat!(
    "context before\n",
    conflict_text!("HEAD", "head content", "branch", "branch content"),
    "context between\n",
    "<<<<<<<",
    " conflict\n",
    concat!("+", "+", "+", "+", "+", "+", "+"),
    " sideA \"snap\"\n",
    "snapshot_a\n",
    ">>>>>>>",
    " conflict ends\n",
    "context after\n",
);

/// A 2-sided jj snapshot conflict with one base and surrounding plain text.
///
/// Line layout (0-based):
///   0  "before context\n"
///   1  "<<<<<<< conflict\n"           ← head_line
///   2  "+++++++ abc12345 \"first change\"\n"
///   3  "apple\n"
///   4  "grapefruit\n"
///   5  "-------  base11111 \"merge base\"\n"
///   6  "apple\n"
///   7  "grape\n"
///   8  "+++++++ def67890 \"second change\"\n"
///   9  "APPLE\n"
///  10  "GRAPE\n"
///  11  ">>>>>>> conflict ends\n"       ← end_line
///  12  "after context\n"
pub const TEXT_JJ_SNAPSHOT_2SIDED: &str = concat!(
    "before context\n",
    jj_snapshot_conflict_text!(
        side "abc12345 \"first change\"" => "apple\ngrapefruit\n",
        base "base11111 \"merge base\"" => "apple\ngrape\n",
        side "def67890 \"second change\"" => "APPLE\nGRAPE\n",
    ),
    "after context\n",
);

/// A 3-sided jj snapshot conflict with two bases and surrounding plain text.
///
/// Line layout (0-based):
///   0  "before context\n"
///   1  "<<<<<<< conflict\n"                              ← head_line=1
///   2  "+++++++ change0 abc12345 \"change A\"\n"
///   3  "alpha\n"                                         ← side0 content_start=3
///   4  "------- base-label0 abc00000 \"base A\"\n"      ← side0 content_end=4
///   5  "common_A\n"                                      ← base0 content_start=5
///   6  "+++++++ change1 def67890 \"change B\"\n"        ← base0 content_end=6
///   7  "beta\n"                                          ← side1 content_start=7
///   8  "------- base-label1 def00000 \"base B\"\n"      ← side1 content_end=8
///   9  "common_B\n"                                      ← base1 content_start=9
///  10  "+++++++ change2 ghi11111 \"change C\"\n"        ← base1 content_end=10
///  11  "gamma\n"                                         ← side2 content_start=11
///  12  "delta\n"
///  13  ">>>>>>> conflict ends\n"                         ← end_line=13, side2 content_end=13
///  14  "after context\n"
pub const TEXT_JJ_SNAPSHOT_3SIDED: &str = concat!(
    "before context\n",
    jj_snapshot_conflict_text!(
        side "change0 abc12345 \"change A\"" => "alpha\n",
        base "base-label0 abc00000 \"base A\"" => "common_A\n",
        side "change1 def67890 \"change B\"" => "beta\n",
        base "base-label1 def00000 \"base B\"" => "common_B\n",
        side "change2 ghi11111 \"change C\"" => "gamma\ndelta\n",
    ),
    "after context\n",
);

/// A 4-sided jj snapshot conflict with three bases and surrounding plain text.
///
/// Line layout (0-based):
///   0  "before context\n"
///   1  "<<<<<<< conflict\n"                        ← head_line=1
///   2  "+++++++ side0 \"alpha\"\n"
///   3  "s0line1\n"                                 ← side0 content_start=3
///   4  "------- base0 \"ancestor0\"\n"             ← side0 content_end=4
///   5  "b0line1\n"                                 ← base0 content_start=5
///   6  "+++++++ side1 \"beta\"\n"                  ← base0 content_end=6
///   7  "s1line1\n"                                 ← side1 content_start=7
///   8  "------- base1 \"ancestor1\"\n"             ← side1 content_end=8
///   9  "b1line1\n"                                 ← base1 content_start=9
///  10  "+++++++ side2 \"gamma\"\n"                 ← base1 content_end=10
///  11  "s2line1\n"                                 ← side2 content_start=11
///  12  "------- base2 \"ancestor2\"\n"             ← side2 content_end=12
///  13  "b2line1\n"                                 ← base2 content_start=13
///  14  "+++++++ side3 \"delta\"\n"                 ← base2 content_end=14
///  15  "s3line1\n"                                 ← side3 content_start=15
///  16  ">>>>>>> conflict ends\n"                   ← end_line=16, side3 content_end=16
///  17  "after context\n"
pub const TEXT_JJ_SNAPSHOT_4SIDED: &str = concat!(
    "before context\n",
    jj_snapshot_conflict_text!(
        side "side0 \"alpha\"" => "s0line1\n",
        base "base0 \"ancestor0\"" => "b0line1\n",
        side "side1 \"beta\"" => "s1line1\n",
        base "base1 \"ancestor1\"" => "b1line1\n",
        side "side2 \"gamma\"" => "s2line1\n",
        base "base2 \"ancestor2\"" => "b2line1\n",
        side "side3 \"delta\"" => "s3line1\n",
    ),
    "after context\n",
);

///! Macros for assembling conflict marker text in tests without literal markers in source.
///!
///! Literal markers in `.rs` files would confuse the parser if it ever scanned its own source.
#[macro_export]
macro_rules! conflict_text {
    ($head:expr, $branch:expr) => {
        concat!(
            "<<<<<<<", "\n", $head, "\n", "=======", "\n", $branch, "\n", ">>>>>>>", "\n"
        )
    };
    ($head_name:expr, $head:expr, $branch_name:expr, $branch:expr) => {
        concat!(
            "<<<<<<< ",
            $head_name,
            "\n",
            $head,
            "\n",
            "=======",
            "\n",
            $branch,
            "\n",
            ">>>>>>> ",
            $branch_name,
            "\n"
        )
    };
}

#[macro_export]
macro_rules! diff3_conflict_text {
    ($head:expr, $original:expr, $branch:expr) => {
        concat!(
            "<<<<<<<", "\n", $head, "\n", "|||||||", "\n", $original, "\n", "=======", "\n",
            $branch, "\n", ">>>>>>>", "\n"
        )
    };
    ($head_name:expr, $head:expr, $original_name:expr, $original:expr, $branch_name:expr, $branch:expr) => {
        concat!(
            "<<<<<<< ",
            $head_name,
            "\n",
            $head,
            "\n",
            "||||||| ",
            $original_name,
            "\n",
            $original,
            "\n",
            "=======",
            "\n",
            $branch,
            "\n",
            ">>>>>>> ",
            $branch_name,
            "\n"
        )
    };
}

#[allow(dead_code)]
pub fn init_logging() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::DEBUG)
            .with_test_writer()
            .init();
    });
}
