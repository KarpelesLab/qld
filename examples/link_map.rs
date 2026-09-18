//! Captures the link map a link would print, and shows what a library link
//! takes from the process: nothing, unless it is asked to.
//!
//! ```sh
//! cargo run --example link_map
//! ```
//!
//! A link writes to the process's standard output only through
//! [`LinkOptions::map_output`], and reads the environment only through the
//! fields [`LinkOptions::use_process_defaults`] fills. Both are empty in
//! [`LinkOptions::new`], so a library link is hermetic and silent; the
//! `qld` binary — and [`qld::parse_gnu`], which describes the link the
//! binary runs — opts into both.

#[path = "support/objects.rs"]
mod objects;

use std::sync::{Arc, Mutex};

use qld::args::{InputAttrs, InputKind, LinkOptions, OutputKind, TextOutput};
use qld::diag::Collect;

fn main() -> qld::Result<()> {
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.push_input(
        InputKind::bytes("main.o", objects::main_object()),
        InputAttrs::default(),
    );
    options.push_input(
        InputKind::bytes("answer.o", objects::answer_object(42)),
        InputAttrs::default(),
    );

    // `-M` and `--cref`, into a string instead of standard output.
    options.print_map = true;
    options.cref = true;
    let map = Arc::new(Mutex::new(String::new()));
    let collected = Arc::clone(&map);
    options.map_output = Some(TextOutput::new(move |text| {
        collected.lock().unwrap().push_str(text);
    }));

    let image = qld::link_to_memory(&options, &Collect::new())?;
    let map = map.lock().unwrap();
    println!("linked {} bytes, {} of map", image.len(), map.len());
    for line in map.lines().take(8) {
        println!("  {line}");
    }

    // What the same options would take from the process, if asked.
    let mut as_the_binary = options.clone();
    as_the_binary.use_process_defaults();
    println!(
        "\nhermetic: map_output {}, LD_LIBRARY_PATH {} entries, timing {}",
        if options.map_output.is_some() {
            "set by this program"
        } else {
            "unset"
        },
        options.env_library_path.len(),
        if options.timing.is_some() {
            "on"
        } else {
            "off"
        },
    );
    println!(
        "as the binary: LD_LIBRARY_PATH {} entries, timing {}",
        as_the_binary.env_library_path.len(),
        if as_the_binary.timing.is_some() {
            "on"
        } else {
            "off"
        },
    );
    Ok(())
}
