//! Macros for assembling conflict marker text in tests without literal markers in source.
//!
//! Literal markers in `.rs` files would confuse the parser if it ever scanned its own source.
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
