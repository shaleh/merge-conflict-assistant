use std::{env, fs::read_to_string};

use common::parser::{ConflictRegion, MergeConflict, parse};

fn main() -> anyhow::Result<()> {
    let debug = false;

    let args: Vec<String> = env::args().collect();

    tracing_subscriber::fmt::fmt()
        .with_max_level(if debug {
            tracing::Level::DEBUG
        } else {
            tracing::Level::INFO
        })
        .with_writer(std::io::stderr)
        .without_time()
        .with_ansi(false)
        .init();

    let Some(filename) = args.last() else {
        anyhow::bail!("No file to parse!");
    };
    let contents = read_to_string(filename)?;
    let uri = filename.parse()?;
    let result = parse(&uri, &contents)?;

    if let Some(merge_conflict) = result {
        println!("Parsed");
        for item in merge_conflict.conflicts() {
            println!("{{");
            print_as_conflict(&merge_conflict, item);
            print_as_diagnostic(item);
            println!("}}");
        }
    }

    Ok(())
}

fn print_as_conflict(merge_conflict: &MergeConflict, conflict: &ConflictRegion) {
    let name = match merge_conflict.head.as_ref() {
        Some(value) => value.clone(),
        None => String::from("head"),
    };
    println!("  {}: {:?}", name, conflict.head_range());
    let name = match merge_conflict.branch.as_ref() {
        Some(value) => value.clone(),
        None => String::from("branch"),
    };
    println!("  {}: {:?}", name, conflict.branch_range(),);
    if let Some(ancestor) = conflict.ancestor.as_ref() {
        let name = match merge_conflict.ancestor.as_ref() {
            Some(value) => value.clone(),
            None => String::from("ancestor"),
        };
        println!("  {}: {:?}", name, ancestor);
    }
}

fn print_as_diagnostic(conflict: &ConflictRegion) {
    let diagnostic: lsp_types::Diagnostic = conflict.into();
    println!("  {:?} {:?}", diagnostic.range.start, diagnostic.range.end);
}
