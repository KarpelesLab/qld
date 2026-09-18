//! Runs links inside a rayon pool the caller owns, several at once.
//!
//! ```sh
//! cargo run --example rayon_pool
//! ```
//!
//! A link called inside [`rayon::ThreadPool::install`] runs its parallel
//! stages on that pool's threads instead of creating its own (unless
//! [`LinkOptions::threads`] asks for a specific count). qld keeps no global
//! state, so links can also run concurrently, here one per task of a
//! parallel iterator; the output does not depend on the thread count.

#[path = "support/objects.rs"]
mod objects;

use rayon::prelude::*;

use qld::args::{InputAttrs, InputKind, LinkOptions, OutputBuffer, OutputKind};
use qld::diag::Collect;

/// Links a program that exits with `value`, into memory.
fn link(value: u8) -> qld::Result<Vec<u8>> {
    let mut options = LinkOptions::new();
    options.kind = OutputKind::StaticExecutable;
    options.push_input(
        InputKind::bytes("main.o", objects::main_object()),
        InputAttrs::default(),
    );
    options.push_input(
        InputKind::bytes("answer.o", objects::answer_object(value)),
        InputAttrs::default(),
    );
    let buffer = OutputBuffer::new();
    options.output_buffer = Some(buffer.clone());
    qld::link(&options, &Collect::new())?;
    buffer
        .take()
        .ok_or_else(|| qld::Error::Internal("no output".into()))
}

fn main() -> qld::Result<()> {
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(4)
        .thread_name(|index| format!("build-worker-{index}"))
        .build()
        .map_err(|error| qld::Error::Internal(error.to_string()))?;

    // One link, on the pool's threads.
    let single = pool.install(|| link(1))?;
    println!("one link in the pool: {} bytes", single.len());

    // Sixteen links sharing the pool with each other.
    let images = pool.install(|| {
        (0..16u8)
            .into_par_iter()
            .map(link)
            .collect::<qld::Result<Vec<_>>>()
    })?;
    println!("{} concurrent links in the pool", images.len());
    assert_eq!(images[1], single, "the same inputs give the same output");

    // Outside any pool, the same link gives the same bytes.
    assert_eq!(link(1)?, single);
    Ok(())
}
