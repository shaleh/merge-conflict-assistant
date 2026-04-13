use std::sync::{Arc, Mutex};

use crossbeam_channel::unbounded;
use rstest::*;

use crate::{
    conflict_text,
    parser::{ConflictRegion, MergeConflict},
    state::ServerState,
};

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
    MergeConflict {
        head: None,
        branch: None,
        ancestor: None,
        conflicts: vec![
            ConflictRegion {
                head: 2,
                branch: 4,
                end: 6,
                ancestor: None,
            },
            ConflictRegion {
                head: 8,
                branch: 10,
                end: 12,
                ancestor: None,
            },
        ],
    }
}

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
