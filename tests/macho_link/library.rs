//! The library options with a Mach-O link: inputs from an
//! `InputProvider`, the image in an `OutputBuffer`, and cancellation.

use std::sync::Arc;

use qld::args::{CancelToken, OutputBuffer, ParseOutcome, parse_darwin};
use qld::diag::Collect;
use qld::input::source::MemoryFiles;

use super::{clang_for, compile, os, scratch, skip, syslibroot};

#[test]
fn in_memory_input_output_and_cancellation() {
    if !clang_for("arm64") {
        skip(
            "in_memory_input_output_and_cancellation",
            "clang cannot target arm64-apple-macos",
        );
        return;
    }
    let object = compile("library", "hello.c", "arm64", &[]);
    let dir = scratch("library");
    // Names that exist only in the provider.
    let memory_object = dir.join("in-memory/hello.o");
    let output = dir.join("never-written");
    let _ = std::fs::remove_file(&output);
    let root = syslibroot();
    let mut argv = os(&["ld64.qld", "-arch", "arm64", "-syslibroot"]);
    argv.push(root.into());
    argv.push(memory_object.clone().into());
    argv.extend(os(&["-lSystem", "-o"]));
    argv.push(output.clone().into());
    let Ok(ParseOutcome::Link(mut options)) = parse_darwin(&argv) else {
        panic!("not a link");
    };
    options.input_provider = Some(Arc::new(
        MemoryFiles::new().with(memory_object, std::fs::read(&object).unwrap()),
    ));
    let buffer = OutputBuffer::new();
    options.output_buffer = Some(buffer.clone());
    qld::macho::link(&options, &Collect::new()).unwrap();
    let image = buffer.take().expect("an image in the buffer");
    assert_eq!(&image[..4], &[0xcf, 0xfa, 0xed, 0xfe]);
    assert!(!output.exists(), "the output file was written");

    let token = CancelToken::new();
    options.cancel = Some(token.clone());
    token.cancel();
    let error = qld::macho::link(&options, &Collect::new()).unwrap_err();
    assert!(CancelToken::is_cancellation(&error), "{error}");
    assert!(buffer.take().is_none());
}
