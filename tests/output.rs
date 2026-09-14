//! Integration tests for the output writer (workstream W5).

use qld::args::BuildId;
use qld::output::build_id::{BLOCK_SIZE, compute_build_id};
use qld::output::hash::{Md5, Sha1, xxh64};
use qld::output::{
    Backing, ChunkRange, FileMode, OutputFile, OutputOptions, ReplaceStrategy, WritePhase,
};
use qld::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// A fresh, empty directory for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join("output-tests")
        .join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Names of the files in `dir`, sorted.
fn entries(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

fn options(replace: ReplaceStrategy) -> OutputOptions {
    let mut options = OutputOptions::default();
    options.replace = replace;
    options
}

/// Deterministic pseudo-random bytes (xorshift), so tests need no RNG.
fn pattern(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// A layout with gaps, zero-sized chunks and a chunk at the very end.
fn gappy_layout(len: u64) -> Vec<ChunkRange> {
    let mut ranges = Vec::new();
    let mut offset = 0;
    let mut i = 0u64;
    while offset < len {
        let size = (i * 7919 % 5000).min(len - offset);
        ranges.push(ChunkRange::new(offset, size));
        offset += size + i % 3 * 17;
        i += 1;
    }
    ranges
}

/// Fills every chunk from a function of its offset; gaps are left alone.
fn fill(out: &mut OutputFile, ranges: &[ChunkRange]) -> Result<()> {
    out.write_chunks(ranges, |i, chunk| {
        let offset = ranges[i].offset;
        for (j, byte) in chunk.iter_mut().enumerate() {
            *byte = ((offset + j as u64) % 251) as u8 | 1;
        }
        Ok(())
    })
}

fn expected_image(len: u64, ranges: &[ChunkRange]) -> Vec<u8> {
    let mut image = vec![0u8; len as usize];
    for range in ranges {
        for pos in range.offset..range.offset + range.size {
            image[pos as usize] = (pos % 251) as u8 | 1;
        }
    }
    image
}

#[test]
fn writes_a_new_file_through_parallel_chunks() {
    for replace in [
        ReplaceStrategy::Rename,
        ReplaceStrategy::Atomic,
        ReplaceStrategy::Unlink,
    ] {
        let dir = scratch(&format!("new-{replace:?}"));
        let path = dir.join("a.out");
        let len = 3 << 20;
        let ranges = gappy_layout(len);
        let mut out = OutputFile::create(&path, len, &options(replace)).unwrap();
        assert_eq!(out.backing(), Backing::Mapped);
        assert_eq!(out.len() as u64, len);
        fill(&mut out, &ranges).unwrap();
        let finished = out.finish().unwrap();
        assert!(finished.bytes().is_none());
        assert_eq!(finished.stats().bytes, len);
        assert_eq!(finished.stats().backing, Backing::Mapped);
        assert_eq!(fs::read(&path).unwrap(), expected_image(len, &ranges));
        assert_eq!(entries(&dir), ["a.out"], "no temporary files left");
    }
}

#[test]
fn in_memory_output_returns_zero_filled_bytes() {
    let len = 100_000;
    let ranges = gappy_layout(len);
    let mut out = OutputFile::in_memory(len).unwrap();
    assert_eq!(out.backing(), Backing::Memory);
    assert!(out.path().is_none());
    fill(&mut out, &ranges).unwrap();
    let finished = out.finish().unwrap();
    assert_eq!(finished.stats().backing, Backing::Memory);
    assert_eq!(finished.into_bytes().unwrap(), expected_image(len, &ranges));
}

#[test]
fn empty_output() {
    let dir = scratch("empty");
    let path = dir.join("empty");
    fs::write(&path, b"old contents").unwrap();
    let out = OutputFile::create(&path, 0, &OutputOptions::default()).unwrap();
    assert!(out.is_empty());
    out.finish().unwrap();
    assert_eq!(fs::read(&path).unwrap(), b"");
    assert_eq!(
        OutputFile::in_memory(0).unwrap().finish().unwrap().bytes(),
        Some(&[][..])
    );
}

#[test]
fn replaces_an_existing_larger_file() {
    for replace in [
        ReplaceStrategy::Rename,
        ReplaceStrategy::Atomic,
        ReplaceStrategy::Unlink,
    ] {
        let dir = scratch(&format!("replace-{replace:?}"));
        let path = dir.join("out");
        fs::write(&path, pattern(1 << 20, 3)).unwrap();
        let mut out = OutputFile::create(&path, 10, &options(replace)).unwrap();
        out.as_mut_slice().copy_from_slice(b"0123456789");
        out.finish().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"0123456789");
        assert_eq!(entries(&dir), ["out"]);
    }
}

/// Readers holding the old file keep seeing the old contents: the old
/// inode is replaced, never written.
#[cfg(unix)]
#[test]
fn open_handles_to_the_old_output_keep_old_contents() {
    use std::io::{Read, Seek, SeekFrom};
    for replace in [
        ReplaceStrategy::Rename,
        ReplaceStrategy::Atomic,
        ReplaceStrategy::Unlink,
    ] {
        let dir = scratch(&format!("held-{replace:?}"));
        let path = dir.join("out");
        let old = pattern(8192, 5);
        fs::write(&path, &old).unwrap();
        let mut held = fs::File::open(&path).unwrap();

        let mut out = OutputFile::create(&path, 4, &options(replace)).unwrap();
        out.as_mut_slice().copy_from_slice(b"new!");
        out.finish().unwrap();

        let mut seen = Vec::new();
        held.seek(SeekFrom::Start(0)).unwrap();
        held.read_to_end(&mut seen).unwrap();
        assert_eq!(seen, old);
        assert_eq!(fs::read(&path).unwrap(), b"new!");
    }
}

/// Replacing an executable while it runs must not fail with `ETXTBSY` and
/// must not disturb the running process.
#[cfg(target_os = "linux")]
#[test]
fn replaces_a_running_executable() {
    let Ok(sleep) = fs::read("/bin/sleep") else {
        return; // No /bin/sleep: nothing to run.
    };
    for replace in [
        ReplaceStrategy::Rename,
        ReplaceStrategy::Atomic,
        ReplaceStrategy::Unlink,
    ] {
        let dir = scratch(&format!("running-{replace:?}"));
        let path = dir.join("sleeper");
        {
            let mut out = OutputFile::create(&path, sleep.len() as u64, &options(replace)).unwrap();
            out.as_mut_slice().copy_from_slice(&sleep);
            out.finish().unwrap();
        }
        let mut child = match std::process::Command::new(&path).arg("30").spawn() {
            Ok(child) => child,
            // A noexec scratch directory: skip.
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => return,
            Err(e) => panic!("cannot run the copied /bin/sleep: {e}"),
        };
        // Give exec a moment to map the binary.
        std::thread::sleep(std::time::Duration::from_millis(100));

        let new = pattern(64 * 1024, 11);
        let mut out = OutputFile::create(&path, new.len() as u64, &options(replace)).unwrap();
        out.as_mut_slice().copy_from_slice(&new);
        out.finish().unwrap();

        assert!(child.try_wait().unwrap().is_none(), "child still running");
        assert_eq!(fs::read(&path).unwrap(), new);
        child.kill().unwrap();
        child.wait().unwrap();
    }
}

#[cfg(unix)]
#[test]
fn permissions() {
    use std::os::unix::fs::PermissionsExt;
    let mode_of = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    for replace in [
        ReplaceStrategy::Rename,
        ReplaceStrategy::Atomic,
        ReplaceStrategy::Unlink,
    ] {
        let dir = scratch(&format!("mode-{replace:?}"));
        for (name, mode) in [
            ("exe", FileMode::Executable),
            ("obj", FileMode::Regular),
            ("exact", FileMode::Exact(0o640)),
            ("exact-exec", FileMode::Exact(0o751)),
        ] {
            let path = dir.join(name);
            // An existing file with other permissions must not leak its mode.
            fs::write(&path, b"old").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
            let mut options = options(replace);
            options.mode = mode;
            let out = OutputFile::create(&path, 16, &options).unwrap();
            out.finish().unwrap();
            let actual = mode_of(&path);
            match mode {
                // Subject to the umask, which never removes the owner's bits
                // in any sane configuration.
                FileMode::Executable => {
                    assert_eq!(actual & 0o700, 0o700, "{name}: {actual:o}");
                    assert_eq!(actual & !0o777, 0);
                }
                FileMode::Regular => {
                    assert_eq!(actual & 0o700, 0o600, "{name}: {actual:o}");
                    assert_eq!(actual & 0o111, 0);
                }
                FileMode::Exact(exact) => assert_eq!(actual, exact, "{name}"),
            }
        }
    }
}

#[test]
fn invalid_layouts_are_errors() {
    let dir = scratch("layouts");
    let path = dir.join("out");
    let mut out = OutputFile::create(&path, 100, &OutputOptions::default()).unwrap();
    let bad: [&[(u64, u64)]; 5] = [
        &[(0, 101)],
        &[(90, 20)],
        &[(10, 10), (15, 10)],
        &[(50, 10), (10, 10)],
        &[(u64::MAX - 1, 4)],
    ];
    for layout in bad {
        let ranges: Vec<ChunkRange> = layout.iter().copied().map(ChunkRange::from).collect();
        match out.split_chunks(&ranges) {
            Err(Error::Internal(message)) => {
                assert!(message.contains(&path.display().to_string()), "{message}");
            }
            other => panic!("{layout:?}: expected a layout error, got {other:?}"),
        }
        let called = std::sync::atomic::AtomicBool::new(false);
        let result = out.write_chunks(&ranges, |_, _| {
            called.store(true, std::sync::atomic::Ordering::Relaxed);
            Ok(())
        });
        assert!(result.is_err());
        assert!(!called.into_inner(), "no chunk written for a bad layout");
    }
    // A valid layout still works afterwards.
    assert_eq!(
        out.split_chunks(&[ChunkRange::new(0, 100)]).unwrap().len(),
        1
    );
}

#[test]
fn chunk_write_errors_are_returned() {
    let mut out = OutputFile::in_memory(64).unwrap();
    let ranges: Vec<ChunkRange> = (0..8).map(|i| ChunkRange::new(i * 8, 8)).collect();
    let result = out.write_chunks(&ranges, |i, _| {
        if i >= 5 {
            Err(Error::Option(format!("chunk {i}")))
        } else {
            Ok(())
        }
    });
    assert!(matches!(result, Err(Error::Option(m)) if m == "chunk 5"));
}

#[test]
fn abandoned_output_is_removed_and_old_file_kept() {
    let dir = scratch("abandoned-atomic");
    let path = dir.join("out");
    fs::write(&path, b"previous").unwrap();
    let mut out = OutputFile::create(&path, 1000, &OutputOptions::default()).unwrap();
    out.as_mut_slice().fill(0xff);
    assert_eq!(
        entries(&dir).len(),
        2,
        "temporary file exists while writing"
    );
    drop(out);
    assert_eq!(entries(&dir), ["out"]);
    assert_eq!(fs::read(&path).unwrap(), b"previous");

    // With `Unlink`, the old file is already gone, and so is the partial one.
    let dir = scratch("abandoned-unlink");
    let path = dir.join("out");
    fs::write(&path, b"previous").unwrap();
    let out = OutputFile::create(&path, 1000, &options(ReplaceStrategy::Unlink)).unwrap();
    drop(out);
    assert!(entries(&dir).is_empty());
}

#[test]
fn missing_directory_error_names_the_output() {
    let dir = scratch("missing");
    let path = dir.join("no-such-dir").join("out");
    for replace in [
        ReplaceStrategy::Rename,
        ReplaceStrategy::Atomic,
        ReplaceStrategy::Unlink,
    ] {
        match OutputFile::create(&path, 10, &options(replace)) {
            Err(Error::Io { path: Some(p), .. }) => assert_eq!(p, path),
            other => panic!("expected an I/O error, got {other:?}"),
        }
    }
}

/// A non-regular destination is written through, not replaced.
#[cfg(unix)]
#[test]
fn special_file_destination_is_buffered() {
    let path = Path::new("/dev/null");
    if !path.exists() {
        return;
    }
    let mut out = OutputFile::create(path, 4096, &OutputOptions::default()).unwrap();
    assert_eq!(out.backing(), Backing::Buffered);
    out.as_mut_slice().fill(0x5a);
    let finished = out.finish().unwrap();
    assert_eq!(finished.stats().backing, Backing::Buffered);
    use std::os::unix::fs::FileTypeExt;
    assert!(fs::metadata(path).unwrap().file_type().is_char_device());
}

/// `-o /dev/stdout` with stdout redirected to a file, or `/proc/self/fd/N`:
/// the bytes go into the open file, and the link is not replaced.
#[cfg(target_os = "linux")]
#[test]
fn writes_through_proc_fd_to_a_regular_file() {
    use std::os::fd::AsRawFd;
    let dir = scratch("proc-fd");
    let real = dir.join("redirected");
    fs::write(&real, pattern(10_000, 1)).unwrap();
    let held = fs::OpenOptions::new().write(true).open(&real).unwrap();
    let link = PathBuf::from(format!("/proc/self/fd/{}", held.as_raw_fd()));

    let mut out = OutputFile::create(&link, 6, &OutputOptions::default()).unwrap();
    assert_eq!(out.backing(), Backing::Buffered);
    out.as_mut_slice().copy_from_slice(b"stdout");
    out.finish().unwrap();
    assert_eq!(fs::read(&real).unwrap(), b"stdout");
    assert_eq!(entries(&dir), ["redirected"]);
}

#[cfg(target_os = "linux")]
#[test]
fn writes_into_a_pipe() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    let (mut reader, writer) = std::io::pipe().unwrap();
    let link = PathBuf::from(format!("/proc/self/fd/{}", writer.as_raw_fd()));
    let data = pattern(300_000, 77);
    let expected = data.clone();
    let consumer = std::thread::spawn(move || {
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        received
    });
    let mut out = OutputFile::create(&link, data.len() as u64, &OutputOptions::default()).unwrap();
    assert_eq!(out.backing(), Backing::Buffered);
    out.as_mut_slice().copy_from_slice(&data);
    out.finish().unwrap();
    drop(writer);
    assert_eq!(consumer.join().unwrap(), expected);
}

#[cfg(unix)]
#[test]
fn old_output_is_released_in_the_background() {
    for replace in [
        ReplaceStrategy::Rename,
        ReplaceStrategy::Atomic,
        ReplaceStrategy::Unlink,
    ] {
        let dir = scratch(&format!("release-{replace:?}"));
        let path = dir.join("big");
        fs::write(&path, pattern(4096, 9)).unwrap();
        let mut options = options(replace);
        options.background_release_threshold = Some(1024);
        let mut out = OutputFile::create(&path, 8, &options).unwrap();
        out.as_mut_slice().copy_from_slice(b"released");
        let mut finished = out.finish().unwrap();
        assert!(finished.stats().background_release);
        finished.wait_for_release();
        assert_eq!(fs::read(&path).unwrap(), b"released");
        assert_eq!(entries(&dir), ["big"]);

        // Below the threshold, nothing is offloaded.
        options.background_release_threshold = Some(1 << 30);
        let out = OutputFile::create(&path, 8, &options).unwrap();
        assert!(!out.finish().unwrap().stats().background_release);
    }
}

#[test]
fn stats_record_phases() {
    let mut out = OutputFile::in_memory(1 << 20).unwrap();
    out.write_chunks(&[ChunkRange::new(0, 1 << 20)], |_, chunk| {
        chunk.fill(1);
        Ok(())
    })
    .unwrap();
    out.stats_mut().time(WritePhase::Write, || {
        std::thread::sleep(std::time::Duration::from_millis(2))
    });
    out.apply_build_id(&BuildId::Fast, 0).unwrap();
    let finished = out.finish().unwrap();
    let stats = finished.stats();
    assert_eq!(stats.bytes, 1 << 20);
    assert!(stats.elapsed(WritePhase::Write) >= std::time::Duration::from_millis(2));
    assert!(stats.total() >= stats.elapsed(WritePhase::Write));
}

#[test]
fn build_id_is_patched_into_the_file() {
    let dir = scratch("build-id");
    let path = dir.join("out");
    let len = BLOCK_SIZE as u64 + 999;
    let mut out = OutputFile::create(&path, len, &OutputOptions::default()).unwrap();
    out.as_mut_slice()
        .copy_from_slice(&pattern(len as usize, 21));
    let offset = 4000;
    let id = out.apply_build_id(&BuildId::Sha1, offset).unwrap().unwrap();
    out.finish().unwrap();

    let bytes = fs::read(&path).unwrap();
    assert_eq!(&bytes[4000..4020], &id[..]);
    let mut zeroed = bytes.clone();
    zeroed[4000..4020].fill(0);
    assert_eq!(compute_build_id(&BuildId::Sha1, &zeroed).unwrap(), id);

    // Tree layout: two blocks, then a hash of the two digests.
    let mut concat = Vec::new();
    concat.extend_from_slice(&Sha1::digest(&zeroed[..BLOCK_SIZE]));
    concat.extend_from_slice(&Sha1::digest(&zeroed[BLOCK_SIZE..]));
    assert_eq!(id, Sha1::digest(&concat));

    let mut out = OutputFile::in_memory(16).unwrap();
    assert!(matches!(
        out.apply_build_id(&BuildId::Md5, 1),
        Err(Error::Internal(_))
    ));
}

#[test]
fn hash_vectors() {
    let hex = |bytes: &[u8]| -> String { bytes.iter().map(|b| format!("{b:02x}")).collect() };
    assert_eq!(
        hex(&Md5::digest(b"message digest")),
        "f96b697d7cb7938d525a2f31aaf161d0"
    );
    assert_eq!(
        hex(&Sha1::digest(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    assert_eq!(xxh64(b"", 0), 0xef46_db37_51d8_e999);
    assert_eq!(xxh64(b"abc", 0), 0x44bc_2cf5_ad77_0999);
}

fn with_threads<T: Send>(threads: usize, f: impl FnOnce() -> T + Send) -> T {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .unwrap()
        .install(f)
}

#[test]
fn build_ids_do_not_depend_on_thread_count() {
    let image = pattern(7 * BLOCK_SIZE + 12_345, 42);
    for kind in [BuildId::Fast, BuildId::Md5, BuildId::Sha1] {
        let ids: Vec<Vec<u8>> = [1, 2, 8]
            .into_iter()
            .map(|threads| with_threads(threads, || compute_build_id(&kind, &image).unwrap()))
            .collect();
        assert_eq!(ids[0], ids[1], "{kind:?}");
        assert_eq!(ids[0], ids[2], "{kind:?}");
    }
    // `uuid` is intentionally random.
    assert_ne!(
        compute_build_id(&BuildId::Uuid, &image),
        compute_build_id(&BuildId::Uuid, &image)
    );
}

/// Throughput check, not run by default:
/// `cargo test --release --test output -- --ignored --nocapture throughput`.
/// `QLD_OUTPUT_BENCH_MIB` sets the size (default 1024).
#[test]
#[ignore = "benchmark; writes a large file"]
fn throughput() {
    let mib: u64 = std::env::var("QLD_OUTPUT_BENCH_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    let len = mib << 20;
    let dir = scratch("throughput");
    let path = dir.join("big");
    let ranges: Vec<ChunkRange> = (0..len / (4 << 20))
        .map(|i| ChunkRange::new(i * (4 << 20), (4 << 20) - 4096))
        .collect();
    let seed = pattern(4 << 20, 5);
    for replace in [
        ReplaceStrategy::Rename,
        ReplaceStrategy::Atomic,
        ReplaceStrategy::Unlink,
    ] {
        let _ = fs::remove_file(&path);
        for round in 0..2 {
            let mut out = OutputFile::create(&path, len, &options(replace)).unwrap();
            out.write_chunks(&ranges, |_, chunk| {
                chunk.copy_from_slice(&seed[..chunk.len()]);
                Ok(())
            })
            .unwrap();
            if round == 0 {
                for kind in [BuildId::Fast, BuildId::Md5, BuildId::Sha1] {
                    let start = std::time::Instant::now();
                    std::hint::black_box(compute_build_id(&kind, out.as_slice()));
                    eprintln!("  build-id {kind:?}: {:?}", start.elapsed());
                }
            }
            out.apply_build_id(&BuildId::Fast, 0).unwrap();
            let finished = out.finish().unwrap();
            eprintln!(
                "{replace:?}, replacing an old output: {}: {}",
                round > 0,
                finished.stats()
            );
        }
    }
    let mut out = OutputFile::in_memory(len).unwrap();
    out.write_chunks(&ranges, |_, chunk| {
        chunk.copy_from_slice(&seed[..chunk.len()]);
        Ok(())
    })
    .unwrap();
    eprintln!("in memory: {}", out.finish().unwrap().stats());
}

#[test]
fn written_files_do_not_depend_on_thread_count() {
    let dir = scratch("threads");
    let len = 5 << 20;
    let ranges = gappy_layout(len);
    let mut outputs = Vec::new();
    for threads in [1, 2, 8] {
        let path = dir.join(format!("out-{threads}"));
        with_threads(threads, || {
            let mut out = OutputFile::create(&path, len, &OutputOptions::default()).unwrap();
            fill(&mut out, &ranges).unwrap();
            out.apply_build_id(&BuildId::Fast, 64).unwrap();
            out.finish().unwrap();
        });
        outputs.push(fs::read(&path).unwrap());
    }
    assert_eq!(outputs[0], outputs[1]);
    assert_eq!(outputs[0], outputs[2]);
}
