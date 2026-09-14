//! The argv corpus: real linker command lines captured from gcc, clang,
//! rustc, CMake and Meson builds (`tests/corpus/*.txt`), fed through
//! [`qld::args::parse_gnu`].
//!
//! Every command line must parse without an unknown-option error, and must
//! match the expectations written in its `#!` header lines. While the parser
//! returns `Unimplemented`, each file is reported as skipped.
//!
//! File format: `#` lines are comments; `#! key = value` lines are
//! expectations; every other line is one argument (argv[0] excluded).

mod common;

use std::path::{Path, PathBuf};

use qld::args::{LinkOptions, ParseOutcome, parse_gnu};
use qld::error::Error;

struct CorpusFile {
    name: String,
    args: Vec<String>,
    expectations: Vec<(String, String, usize)>,
}

fn load(path: &Path) -> Result<CorpusFile, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut file = CorpusFile {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        args: vec!["ld".to_string()],
        expectations: Vec::new(),
    };
    for (index, line) in text.lines().enumerate() {
        let line_number = index + 1;
        if let Some(expectation) = line.strip_prefix("#!") {
            let (key, value) = expectation.split_once('=').ok_or_else(|| {
                format!(
                    "{}:{line_number}: expected `#! key = value`",
                    path.display()
                )
            })?;
            file.expectations.push((
                key.trim().to_string(),
                value.trim().to_string(),
                line_number,
            ));
        } else if !line.starts_with('#') {
            file.args
                .push(line.strip_suffix('\r').unwrap_or(line).to_string());
        }
    }
    if file.args.len() < 2 {
        return Err(format!("{}: no arguments", path.display()));
    }
    Ok(file)
}

fn corpus_files() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/corpus");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "txt"))
        .collect();
    files.sort();
    files
}

/// Checks one `#! key = value` expectation against parsed options.
fn check_expectation(options: &LinkOptions, key: &str, expected: &str) -> Result<(), String> {
    let display = |value: Option<String>| value.unwrap_or_else(|| "(none)".to_string());
    let actual = match key {
        "output" => display(options.output.as_ref().map(|p| p.display().to_string())),
        "kind" => format!("{:?}", options.kind),
        "soname" => display(options.soname.clone()),
        "entry" => display(options.entry.clone()),
        "dynamic_linker" => display(
            options
                .dynamic_linker
                .as_ref()
                .map(|p| p.display().to_string()),
        ),
        "gc_sections" => options.gc_sections.to_string(),
        "export_dynamic" => options.export_dynamic.to_string(),
        "bind_now" => options.bind_now.to_string(),
        "inputs" => options.inputs.len().to_string(),
        _ => return Err(format!("unknown expectation key `{key}`")),
    };
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{key}: expected {expected:?}, parsed {actual:?}"))
    }
}

#[test]
fn corpus_command_lines_parse() {
    let files = corpus_files();
    assert!(
        files.len() >= 5,
        "the corpus should have at least 5 command lines"
    );
    let (mut passed, mut skipped) = (0, 0);
    let mut failures = Vec::new();
    for path in &files {
        let file = match load(path) {
            Ok(file) => file,
            Err(error) => {
                failures.push(error);
                continue;
            }
        };
        match parse_gnu(&file.args) {
            Err(Error::Unimplemented(what)) => {
                skipped += 1;
                common::tools::skip(format!("{}: not implemented yet: {what}", file.name));
            }
            Err(error) => failures.push(format!("{}: parse error: {error}", file.name)),
            Ok(ParseOutcome::Help | ParseOutcome::Version) => {
                failures.push(format!("{}: parsed as --help/--version", file.name));
            }
            Ok(ParseOutcome::Link(options)) => {
                let problems: Vec<String> = file
                    .expectations
                    .iter()
                    .filter_map(|(key, value, line)| {
                        check_expectation(&options, key, value)
                            .err()
                            .map(|e| format!("  line {line}: {e}"))
                    })
                    .collect();
                if problems.is_empty() {
                    passed += 1;
                    println!("PASS    {}", file.name);
                } else {
                    failures.push(format!("{}:\n{}", file.name, problems.join("\n")));
                }
            }
        }
    }
    println!(
        "corpus: {passed} passed, {skipped} skipped, {} failed",
        failures.len()
    );
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}

/// The corpus files and their headers are well-formed on every host.
#[test]
fn corpus_files_are_valid() {
    for path in corpus_files() {
        let file = load(&path).unwrap_or_else(|e| panic!("{e}"));
        let empty = LinkOptions::new();
        for (key, value, line) in &file.expectations {
            if let Err(error) = check_expectation(&empty, key, value)
                && error.starts_with("unknown expectation key")
            {
                panic!("{}:{line}: {error}", path.display());
            }
        }
        for arg in &file.args {
            assert!(
                !arg.contains("/home/") || arg.contains("/home/user/"),
                "{}: home directory not normalized in {arg:?}",
                path.display()
            );
        }
    }
}
