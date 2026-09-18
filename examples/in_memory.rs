//! Links objects a program produced in memory, and gets the executable back
//! as bytes: no temporary files.
//!
//! ```sh
//! cargo run --example in_memory            # prints what it linked
//! cargo run --example in_memory -- answer  # also writes ./answer
//! ```
//!
//! Two ways of passing inputs are shown:
//!
//! 1. anonymous buffers ([`InputKind::bytes`]), built into [`LinkOptions`]
//!    directly;
//! 2. files by path ([`MemoryFiles`]), for running an ordinary command
//!    line, `-l` search included, against files that only exist in memory.

#[path = "support/objects.rs"]
mod objects;

use std::sync::Arc;

use qld::args::{InputAttrs, InputKind, LinkOptions, OutputBuffer, OutputKind};
use qld::diag::Collect;
use qld::input::MemoryFiles;

fn main() -> qld::Result<()> {
    // Object code from a compiler or JIT in the same process.
    let main_o = objects::main_object();
    let answer_o = objects::answer_object(42);

    // 1. Buffers as inputs, the output into a buffer.
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.push_input(
        InputKind::bytes("main.o", main_o.clone()),
        InputAttrs::default(),
    );
    options.push_input(
        InputKind::bytes("answer.o", answer_o.clone()),
        InputAttrs::default(),
    );
    let buffer = OutputBuffer::new();
    options.output_buffer = Some(buffer.clone());
    let diagnostics = Collect::new();
    qld::link(&options, &diagnostics)?;
    let first = buffer.take().expect("a successful link fills the buffer");
    report("buffers", &first);

    // 2. A command line whose files live in memory: `main.o`, and
    //    `libanswer.a` in a search directory that does not exist on disk.
    let library = objects::archive(&[("answer.o", &answer_o, &["answer"])]);
    let files = MemoryFiles::new()
        .with("main.o", main_o)
        .with("/virtual/lib/libanswer.a", library);
    let argv = ["ld", "-static", "-L/virtual/lib", "main.o", "-lanswer"];
    let qld::ParseOutcome::Link(options) = qld::parse_gnu(&argv)? else {
        unreachable!("a link command line");
    };
    let mut options = *options;
    options.input_provider = Some(Arc::new(files));
    let buffer = OutputBuffer::new();
    options.output_buffer = Some(buffer.clone());
    qld::link(&options, &diagnostics)?;
    let second = buffer.take().expect("a successful link fills the buffer");
    report("memory files", &second);
    assert_eq!(first, second, "same program either way");

    for diagnostic in diagnostics.take_sorted() {
        eprintln!("{}: {}", diagnostic.severity, diagnostic.message);
    }
    if let Some(path) = std::env::args_os().nth(1) {
        std::fs::write(&path, &first).map_err(|error| qld::Error::io(&path, error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .map_err(|error| qld::Error::io(&path, error))?;
        }
        println!("wrote {} (it exits with status 42)", path.to_string_lossy());
    }
    Ok(())
}

fn report(how: &str, image: &[u8]) {
    let entry = objects::entry_point(image).unwrap_or(0);
    println!(
        "{how}: {} byte x86-64 executable, entry point {entry:#x}",
        image.len()
    );
}
