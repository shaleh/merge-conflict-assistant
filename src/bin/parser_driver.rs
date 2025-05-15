use std::{env, fs::read_to_string};

use common::parser::{Conflict, Parser};

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
    let result = Parser::parse(&uri, &contents)?;

    if let Some(conflicts) = result {
        println!("Parsed");
        for item in conflicts {
            println!("{{");
            print_as_conflict(&item);
            print_as_diagnostic(&item);
            println!("}}");
        }
    }

    Ok(())
}

fn print_as_conflict(conflict: &Conflict) {
    let name = match conflict.ours.name.as_ref() {
        Some(value) => value.clone(),
        None => String::from("ours"),
    };
    println!("  {}: {} {}", name, conflict.ours.start, conflict.ours.end);
    let name = match conflict.theirs.name.as_ref() {
        Some(value) => value.clone(),
        None => String::from("theirs"),
    };
    println!(
        "  {}: {} {}",
        name, conflict.theirs.start, conflict.theirs.end
    );
    if let Some(ancestor) = conflict.ancestor.as_ref() {
        let name = match ancestor.name.as_ref() {
            Some(value) => value.clone(),
            None => String::from("ancestor"),
        };
        println!("  {}: {} {}", name, ancestor.start, ancestor.end);
    }
}

fn print_as_diagnostic(conflict: &Conflict) {
    let diagnostic: lsp_types::Diagnostic = conflict.into();
    println!("  {:?} {:?}", diagnostic.range.start, diagnostic.range.end);
}
