// A Rust program that exercises std on aarch64-apple-darwin: threads,
// panic=unwind with catch_unwind, thread-locals, formatting and HashMap.
// Linked by qld in tests/macho_link.rs (`rust_binary`).

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

thread_local! {
    static COUNTER: Cell<u32> = const { Cell::new(0) };
    static NAME: String = String::from("tls");
}

fn bump(by: u32) -> u32 {
    COUNTER.with(|c| {
        c.set(c.get() + by);
        c.get()
    })
}

#[inline(never)]
fn might_panic(value: u32) -> u32 {
    if value % 3 == 0 {
        panic!("value {value} is a multiple of three");
    }
    value * 2
}

fn main() {
    // Threads with their own thread-locals, results over a channel.
    let (tx, rx) = mpsc::channel();
    let handles: Vec<_> = (1..=4u32)
        .map(|i| {
            let tx = tx.clone();
            thread::spawn(move || {
                let mut last = 0;
                for _ in 0..i {
                    last = bump(i);
                }
                tx.send((i, last)).unwrap();
                NAME.with(|n| n.len())
            })
        })
        .collect();
    drop(tx);
    let lens: usize = handles.into_iter().map(|h| h.join().unwrap()).sum();
    let mut results: Vec<(u32, u32)> = rx.iter().collect();
    results.sort_unstable();
    println!("threads: {results:?} tls-len {lens} main {}", bump(0));

    // panic=unwind across frames, caught.
    std::panic::set_hook(Box::new(|_| {}));
    let mut caught = 0;
    let mut sum = 0;
    for value in 1..=9 {
        match std::panic::catch_unwind(|| might_panic(value)) {
            Ok(v) => sum += v,
            Err(payload) => {
                let message = payload
                    .downcast_ref::<String>()
                    .map(String::as_str)
                    .unwrap_or("?");
                assert!(message.contains("multiple of three"));
                caught += 1;
            }
        }
    }
    println!("panics: caught {caught} sum {sum}");

    // A panicking thread is reported through join.
    let joined = thread::spawn(|| might_panic(6)).join();
    let _ = std::panic::take_hook();
    println!("thread panic: {}", joined.is_err());

    // HashMap and formatting.
    let mut words: HashMap<&str, usize> = HashMap::new();
    for word in "the quick brown fox jumps over the lazy dog the end".split(' ') {
        *words.entry(word).or_default() += 1;
    }
    let mut counts: Vec<_> = words.into_iter().collect();
    counts.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    println!(
        "words: {:?} pi {:>8.3} hex {:#x} float {:e}",
        &counts[..3],
        std::f64::consts::PI,
        255,
        1234.5f64
    );
}
