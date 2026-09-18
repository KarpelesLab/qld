//! Cancels a link from another thread: a link with a deadline.
//!
//! ```sh
//! cargo run --example cancel                       # built-in demo
//! cargo run --example cancel -- 250 -o big <ld arguments>
//! ```
//!
//! With arguments, the first is a time limit in milliseconds and the rest
//! is a GNU ld command line; the link is cancelled if it takes longer. The
//! link checks its [`CancelToken`] between pipeline stages and while it
//! loads inputs and writes the output, then returns an error that
//! [`CancelToken::is_cancellation`] recognizes. A cancelled link leaves any
//! previous output file untouched.

#[path = "support/objects.rs"]
mod objects;

use std::sync::mpsc;
use std::time::{Duration, Instant};

use qld::args::{CancelToken, InputAttrs, InputKind, LinkOptions, OutputBuffer, OutputKind};
use qld::diag::Stderr;

/// Runs `options` with a deadline: a watchdog thread cancels the link if
/// it has not finished in time.
fn link_with_deadline(mut options: LinkOptions, limit: Duration) -> qld::Result<()> {
    let token = CancelToken::new();
    options.cancel = Some(token.clone());
    let (done, finished) = mpsc::channel::<()>();
    let watchdog = std::thread::spawn(move || {
        // Either the link finishes (the sender is dropped) or time runs out.
        if finished.recv_timeout(limit) == Err(mpsc::RecvTimeoutError::Timeout) {
            token.cancel();
        }
    });
    let result = qld::link(&options, &Stderr::new(qld::PROGRAM_NAME));
    drop(done);
    let _ = watchdog.join();
    result
}

fn main() -> qld::Result<()> {
    let mut args = std::env::args().skip(1);
    if let Some(limit) = args.next() {
        let limit = Duration::from_millis(limit.parse().unwrap_or(1000));
        let argv: Vec<String> = std::iter::once("ld".to_string()).chain(args).collect();
        let qld::ParseOutcome::Link(options) = qld::parse_gnu(&argv)? else {
            return Ok(());
        };
        let start = Instant::now();
        let result = link_with_deadline(*options, limit);
        match &result {
            Ok(()) => eprintln!("linked in {:?}", start.elapsed()),
            Err(error) if CancelToken::is_cancellation(error) => {
                eprintln!("{error} after {:?}", start.elapsed());
            }
            Err(_) => {}
        }
        return result;
    }

    // Without arguments: link the in-memory demo program twice, once with
    // plenty of time and once cancelled before it starts.
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
    let buffer = OutputBuffer::new();
    options.output_buffer = Some(buffer.clone());

    link_with_deadline(options.clone(), Duration::from_secs(10))?;
    println!(
        "with a 10 s deadline: linked {} bytes",
        buffer.take().map_or(0, |b| b.len())
    );

    let token = CancelToken::new();
    token.cancel();
    options.cancel = Some(token);
    match qld::link(&options, &Stderr::new(qld::PROGRAM_NAME)) {
        Err(error) if CancelToken::is_cancellation(&error) => {
            println!(
                "cancelled before it started: {error}; output empty: {}",
                !buffer.is_filled()
            );
        }
        other => println!("unexpected: {other:?}"),
    }
    Ok(())
}
