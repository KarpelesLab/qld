//! Integration tests for the output writer (workstream W5).
//!
//! Tests that create output files run once per backing ([`BACKINGS`]).

use qld::args::BuildId;
#[cfg(unix)]
use qld::output::FileMode;
use qld::output::build_id::{BLOCK_SIZE, build_id_size, compute_build_id};
use qld::output::hash::{Md5, Sha1, xxh64};
use qld::output::{
    Backing, BackingPolicy, ChunkRange, OutputFile, OutputOptions, ReplaceStrategy, WritePhase,
};
use qld::{Error, Result};
use std::fs;
use std::path::{Path, PathBuf};

/// Every backing of a regular output file, with what it reports.
const BACKINGS: [(BackingPolicy, Backing); 3] = [
    (BackingPolicy::Mapped, Backing::Mapped),
    (BackingPolicy::Written, Backing::Written),
    (BackingPolicy::Buffered, Backing::Buffered),
];

const STRATEGIES: [ReplaceStrategy; 3] = [
    ReplaceStrategy::Rename,
    ReplaceStrategy::Atomic,
    ReplaceStrategy::Unlink,
];

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

fn options(replace: ReplaceStrategy, backing: BackingPolicy) -> OutputOptions {
    let mut options = OutputOptions::default();
    options.replace = replace;
    options.backing = backing;
    options
}

fn with_backing(backing: BackingPolicy) -> OutputOptions {
    options(ReplaceStrategy::default(), backing)
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

/// Chunks of up to a few MiB that straddle block boundaries in every way:
/// starting or ending exactly on one, crossing one by a few bytes or by
/// more than the write backing keeps in memory, with gaps and empty chunks.
fn straddling_layout(len: u64) -> Vec<ChunkRange> {
    let b = BLOCK_SIZE as u64;
    let sizes = [
        10,
        b - 10,
        0,
        b + 100,
        70_000,
        3 * b,
        5,
        b - 105 - 70_000,
        200_000,
        1,
        b / 2,
    ];
    let gaps = [0, 0, 0, 7, 0, 4096, 0, 0, 1, 1 << 20, 3];
    let mut ranges = Vec::new();
    let mut offset = 0;
    for i in 0.. {
        let size = sizes[i % sizes.len()];
        if offset + size > len {
            break;
        }
        ranges.push(ChunkRange::new(offset, size));
        offset += size + gaps[i % gaps.len()];
    }
    ranges
}

/// Fills every chunk from a function of its offset; gaps are left alone.
fn fill(out: &mut OutputFile, ranges: &[ChunkRange]) -> Result<()> {
    out.write_chunks(ranges, |i, chunk| {
        assert!(chunk.iter().all(|&b| b == 0), "chunks start zeroed");
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
    for (policy, backing) in BACKINGS {
        for replace in STRATEGIES {
            let dir = scratch(&format!("new-{replace:?}-{policy:?}"));
            let path = dir.join("a.out");
            let len = 3 << 20;
            let ranges = gappy_layout(len);
            let mut out = OutputFile::create(&path, len, &options(replace, policy)).unwrap();
            assert_eq!(out.backing(), backing);
            assert_eq!(out.len() as u64, len);
            fill(&mut out, &ranges).unwrap();
            let finished = out.finish().unwrap();
            assert!(finished.bytes().is_none());
            assert_eq!(finished.stats().bytes, len);
            assert_eq!(finished.stats().backing, backing);
            assert_eq!(fs::read(&path).unwrap(), expected_image(len, &ranges));
            assert_eq!(entries(&dir), ["a.out"], "no temporary files left");
        }
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
    for (policy, _) in BACKINGS {
        let dir = scratch(&format!("empty-{policy:?}"));
        let path = dir.join("empty");
        fs::write(&path, b"old contents").unwrap();
        let mut out = OutputFile::create(&path, 0, &with_backing(policy)).unwrap();
        assert!(out.is_empty());
        out.write_chunks(&[ChunkRange::new(0, 0)], |_, chunk| {
            assert!(chunk.is_empty());
            Ok(())
        })
        .unwrap();
        out.finish().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"");
    }
    assert_eq!(
        OutputFile::in_memory(0).unwrap().finish().unwrap().bytes(),
        Some(&[][..])
    );
}

#[test]
fn replaces_an_existing_larger_file() {
    for (policy, _) in BACKINGS {
        for replace in STRATEGIES {
            let dir = scratch(&format!("replace-{replace:?}-{policy:?}"));
            let path = dir.join("out");
            fs::write(&path, pattern(1 << 20, 3)).unwrap();
            let mut out = OutputFile::create(&path, 10, &options(replace, policy)).unwrap();
            out.as_mut_slice().unwrap().copy_from_slice(b"0123456789");
            out.finish().unwrap();
            assert_eq!(fs::read(&path).unwrap(), b"0123456789");
            assert_eq!(entries(&dir), ["out"]);
        }
    }
}

/// Readers holding the old file keep seeing the old contents: the old
/// inode is replaced, never written.
#[cfg(unix)]
#[test]
fn open_handles_to_the_old_output_keep_old_contents() {
    use std::io::{Read, Seek, SeekFrom};
    for (policy, _) in BACKINGS {
        for replace in STRATEGIES {
            let dir = scratch(&format!("held-{replace:?}-{policy:?}"));
            let path = dir.join("out");
            let old = pattern(8192, 5);
            fs::write(&path, &old).unwrap();
            let mut held = fs::File::open(&path).unwrap();

            let mut out = OutputFile::create(&path, 4, &options(replace, policy)).unwrap();
            out.write_at(0, b"new!").unwrap();
            out.finish().unwrap();

            let mut seen = Vec::new();
            held.seek(SeekFrom::Start(0)).unwrap();
            held.read_to_end(&mut seen).unwrap();
            assert_eq!(seen, old);
            assert_eq!(fs::read(&path).unwrap(), b"new!");
        }
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
    for (policy, _) in BACKINGS {
        for replace in STRATEGIES {
            let dir = scratch(&format!("running-{replace:?}-{policy:?}"));
            let path = dir.join("sleeper");
            let options = options(replace, policy);
            {
                let mut out = OutputFile::create(&path, sleep.len() as u64, &options).unwrap();
                out.write_chunks(&[ChunkRange::new(0, sleep.len() as u64)], |_, chunk| {
                    chunk.copy_from_slice(&sleep);
                    Ok(())
                })
                .unwrap();
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
            let mut out = OutputFile::create(&path, new.len() as u64, &options).unwrap();
            out.write_at(0, &new).unwrap();
            out.finish().unwrap();

            assert!(child.try_wait().unwrap().is_none(), "child still running");
            assert_eq!(fs::read(&path).unwrap(), new);
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}

#[cfg(unix)]
#[test]
fn permissions() {
    use std::os::unix::fs::PermissionsExt;
    let mode_of = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o7777;
    for (policy, _) in BACKINGS {
        for replace in STRATEGIES {
            let dir = scratch(&format!("mode-{replace:?}-{policy:?}"));
            for (name, mode) in [
                ("exe", FileMode::Executable),
                ("obj", FileMode::Regular),
                ("exact", FileMode::Exact(0o640)),
                ("exact-exec", FileMode::Exact(0o751)),
            ] {
                let path = dir.join(name);
                // An existing file with other permissions must not leak its
                // mode.
                fs::write(&path, b"old").unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
                let mut options = options(replace, policy);
                options.mode = mode;
                let out = OutputFile::create(&path, 16, &options).unwrap();
                out.finish().unwrap();
                let actual = mode_of(&path);
                match mode {
                    // Subject to the umask, which never removes the owner's
                    // bits in any sane configuration.
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
                assert_eq!(fs::read(&path).unwrap(), [0; 16]);
            }
        }
    }
}

#[test]
fn invalid_layouts_are_errors() {
    for (policy, _) in BACKINGS {
        let dir = scratch(&format!("layouts-{policy:?}"));
        let path = dir.join("out");
        let mut out = OutputFile::create(&path, 100, &with_backing(policy)).unwrap();
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
        for (offset, size) in [(95, 10), (100, 1), (u64::MAX - 1, 4)] {
            assert!(matches!(
                out.write_at(offset, &vec![0; size]),
                Err(Error::Internal(_))
            ));
        }
        // A valid layout still works afterwards.
        assert_eq!(
            out.split_chunks(&[ChunkRange::new(0, 100)]).unwrap().len(),
            1
        );
    }
}

#[test]
fn chunk_write_errors_are_returned() {
    let dir = scratch("chunk-errors");
    let ranges: Vec<ChunkRange> = (0..512).map(|i| ChunkRange::new(i * 8192, 8192)).collect();
    let check = |out: &mut OutputFile| {
        let result = out.write_chunks(&ranges, |i, _| {
            if i >= 5 && i % 3 == 2 {
                Err(Error::Option(format!("chunk {i}")))
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(Error::Option(m)) if m == "chunk 5"));
    };
    check(&mut OutputFile::in_memory(512 * 8192).unwrap());
    for (policy, _) in BACKINGS {
        let path = dir.join(format!("{policy:?}"));
        check(&mut OutputFile::create(&path, 512 * 8192, &with_backing(policy)).unwrap());
    }
}

#[test]
fn abandoned_output_is_removed_and_old_file_kept() {
    for (policy, _) in BACKINGS {
        let dir = scratch(&format!("abandoned-atomic-{policy:?}"));
        let path = dir.join("out");
        fs::write(&path, b"previous").unwrap();
        let mut out = OutputFile::create(&path, 1000, &with_backing(policy)).unwrap();
        fill(&mut out, &[ChunkRange::new(0, 1000)]).unwrap();
        assert_eq!(
            entries(&dir).len(),
            2,
            "temporary file exists while writing"
        );
        drop(out);
        assert_eq!(entries(&dir), ["out"]);
        assert_eq!(fs::read(&path).unwrap(), b"previous");

        // With `Unlink`, the old file is already gone, and so is the partial
        // one.
        let dir = scratch(&format!("abandoned-unlink-{policy:?}"));
        let path = dir.join("out");
        fs::write(&path, b"previous").unwrap();
        let mut out =
            OutputFile::create(&path, 1000, &options(ReplaceStrategy::Unlink, policy)).unwrap();
        out.write_at(10, b"partial").unwrap();
        drop(out);
        assert!(entries(&dir).is_empty());
    }
}

#[test]
fn missing_directory_error_names_the_output() {
    let dir = scratch("missing");
    let path = dir.join("no-such-dir").join("out");
    for (policy, _) in BACKINGS {
        for replace in STRATEGIES {
            match OutputFile::create(&path, 10, &options(replace, policy)) {
                Err(Error::Io { path: Some(p), .. }) => assert_eq!(p, path),
                other => panic!("expected an I/O error, got {other:?}"),
            }
        }
    }
}

/// A non-regular destination is written through, not replaced, and is
/// buffered whatever the backing asked for.
#[cfg(unix)]
#[test]
fn special_file_destination_is_buffered() {
    let path = Path::new("/dev/null");
    if !path.exists() {
        return;
    }
    for (policy, _) in BACKINGS {
        let mut out = OutputFile::create(path, 4096, &with_backing(policy)).unwrap();
        assert_eq!(out.backing(), Backing::Buffered);
        fill(&mut out, &gappy_layout(4096)).unwrap();
        out.apply_build_id(&BuildId::Sha1, 100).unwrap();
        let finished = out.finish().unwrap();
        assert_eq!(finished.stats().backing, Backing::Buffered);
        use std::os::unix::fs::FileTypeExt;
        assert!(fs::metadata(path).unwrap().file_type().is_char_device());
    }
}

/// `-o /dev/stdout` with stdout redirected to a file, or `/proc/self/fd/N`:
/// the bytes go into the open file, and the link is not replaced.
#[cfg(target_os = "linux")]
#[test]
fn writes_through_proc_fd_to_a_regular_file() {
    use std::os::fd::AsRawFd;
    for (policy, _) in BACKINGS {
        let dir = scratch(&format!("proc-fd-{policy:?}"));
        let real = dir.join("redirected");
        fs::write(&real, pattern(10_000, 1)).unwrap();
        let held = fs::OpenOptions::new().write(true).open(&real).unwrap();
        let link = PathBuf::from(format!("/proc/self/fd/{}", held.as_raw_fd()));

        let mut out = OutputFile::create(&link, 6, &with_backing(policy)).unwrap();
        assert_eq!(out.backing(), Backing::Buffered);
        out.write_chunks(&[ChunkRange::new(0, 6)], |_, chunk| {
            chunk.copy_from_slice(b"stdout");
            Ok(())
        })
        .unwrap();
        out.finish().unwrap();
        assert_eq!(fs::read(&real).unwrap(), b"stdout");
        assert_eq!(entries(&dir), ["redirected"]);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn writes_into_a_pipe() {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    for (policy, _) in BACKINGS {
        let (mut reader, writer) = std::io::pipe().unwrap();
        let link = PathBuf::from(format!("/proc/self/fd/{}", writer.as_raw_fd()));
        let data = pattern(300_000, 77);
        let expected = data.clone();
        let consumer = std::thread::spawn(move || {
            let mut received = Vec::new();
            reader.read_to_end(&mut received).unwrap();
            received
        });
        let mut out = OutputFile::create(&link, data.len() as u64, &with_backing(policy)).unwrap();
        assert_eq!(out.backing(), Backing::Buffered);
        out.as_mut_slice().unwrap().copy_from_slice(&data);
        out.finish().unwrap();
        drop(writer);
        assert_eq!(consumer.join().unwrap(), expected);
    }
}

#[cfg(unix)]
#[test]
fn old_output_is_released_in_the_background() {
    for (policy, _) in BACKINGS {
        for replace in STRATEGIES {
            let dir = scratch(&format!("release-{replace:?}-{policy:?}"));
            let path = dir.join("big");
            fs::write(&path, pattern(4096, 9)).unwrap();
            let mut options = options(replace, policy);
            options.background_release_threshold = Some(1024);
            let mut out = OutputFile::create(&path, 8, &options).unwrap();
            out.write_at(0, b"released").unwrap();
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
    for (policy, _) in BACKINGS {
        let dir = scratch(&format!("build-id-{policy:?}"));
        let path = dir.join("out");
        let len = BLOCK_SIZE as u64 + 999;
        let mut out = OutputFile::create(&path, len, &with_backing(policy)).unwrap();
        out.as_mut_slice()
            .unwrap()
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

        let mut out = OutputFile::create(&path, 16, &with_backing(policy)).unwrap();
        assert!(matches!(
            out.apply_build_id(&BuildId::Md5, 1),
            Err(Error::Internal(_))
        ));
    }
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

/// How a test output is filled before its build-id is applied.
#[derive(Clone, Copy, Debug)]
enum Steps {
    /// `reserve_build_id`, then `write_chunks`.
    Reserved,
    /// `write_chunks` only: the write backing reads the file back.
    Unreserved,
    /// `reserve_build_id` for another offset, then `write_chunks`.
    ReservedElsewhere,
    /// `reserve_build_id`, `write_chunks`, then a `write_at` patch, which
    /// invalidates the digests computed while writing.
    ReservedThenPatched,
    /// `reserve_build_id`, `write_chunks`, then whole-image access.
    ReservedThenWholeImage,
}

/// Writes `ranges` to a file with `policy`, applies a `kind` build-id at
/// `offset`, and returns the file's bytes and the id.
fn build(
    path: &Path,
    policy: BackingPolicy,
    len: u64,
    ranges: &[ChunkRange],
    kind: &BuildId,
    offset: u64,
    steps: Steps,
) -> (Vec<u8>, Vec<u8>) {
    let mut out = OutputFile::create(path, len, &with_backing(policy)).unwrap();
    match steps {
        Steps::Unreserved => {}
        Steps::ReservedElsewhere => out.reserve_build_id(kind, offset / 2),
        _ => out.reserve_build_id(kind, offset),
    }
    fill(&mut out, ranges).unwrap();
    match steps {
        Steps::ReservedThenPatched => out.write_at(len / 3, b"patch").unwrap(),
        Steps::ReservedThenWholeImage => out.as_mut_slice().unwrap()[len as usize - 1] ^= 0x80,
        _ => {}
    }
    let id = out.apply_build_id(kind, offset).unwrap().unwrap();
    out.finish().unwrap();
    (fs::read(path).unwrap(), id)
}

/// The write backing hashes chunk buffers while writing them; the result
/// must be identical to the mapped backing's in every layout.
#[test]
fn backings_produce_identical_files_and_build_ids() {
    let dir = scratch("identical");
    let b = BLOCK_SIZE as u64;
    let cases: Vec<(&str, u64, Vec<ChunkRange>)> = vec![
        ("gappy", 5 * b + 3, gappy_layout(5 * b + 3)),
        ("straddling", 9 * b, straddling_layout(9 * b)),
        (
            "sparse",
            6 * b,
            vec![ChunkRange::new(b - 3, 6), ChunkRange::new(5 * b + 1, 17)],
        ),
        ("one-chunk", 3 * b + 1, vec![ChunkRange::new(0, 3 * b + 1)]),
        (
            // Small heads of straddling chunks, kept in memory.
            "heads",
            3 * b,
            vec![
                ChunkRange::new(0, b - 1000),
                ChunkRange::new(b - 990, 5000),
                ChunkRange::new(2 * b - 64 * 1024, 64 * 1024 + 1),
                ChunkRange::new(2 * b + 1, 10),
                ChunkRange::new(3 * b - 20, 20),
            ],
        ),
        ("small", 5000, gappy_layout(5000)),
    ];
    for (name, len, ranges) in &cases {
        let image = expected_image(*len, ranges);
        // The field inside a chunk, across a block boundary, and in a gap
        // (or at the end).
        let offsets = [100, b.min(*len - 20).saturating_sub(7), *len - 20];
        let mut combos: Vec<(BuildId, usize, Steps)> = [BuildId::Fast, BuildId::Md5, BuildId::Sha1]
            .into_iter()
            .map(|kind| (kind, 7, Steps::Reserved))
            .collect();
        combos.push((BuildId::Fast, 1, Steps::Reserved));
        for steps in [
            Steps::Unreserved,
            Steps::ReservedElsewhere,
            Steps::ReservedThenPatched,
            Steps::ReservedThenWholeImage,
        ] {
            combos.push((BuildId::Fast, 3, steps));
        }
        for offset in offsets {
            for (kind, threads, steps) in &combos {
                let size = build_id_size(kind).unwrap();
                let results: Vec<(Vec<u8>, Vec<u8>)> = BACKINGS
                    .iter()
                    .map(|(policy, _)| {
                        let path = dir.join(format!("{name}-{policy:?}"));
                        with_threads(*threads, || {
                            build(&path, *policy, *len, ranges, kind, offset, *steps)
                        })
                    })
                    .collect();
                let context = format!("{name} {kind:?} {offset} {threads} {steps:?}");
                assert!(results[0] == results[1], "{context}: mapped vs written");
                assert!(results[0] == results[2], "{context}: mapped vs buffered");
                if matches!(steps, Steps::Reserved | Steps::Unreserved) {
                    let mut zeroed = image.clone();
                    zeroed[offset as usize..offset as usize + size].fill(0);
                    let (bytes, id) = &results[1];
                    assert_eq!(
                        compute_build_id(kind, &zeroed).as_ref(),
                        Some(id),
                        "{context}"
                    );
                    assert_eq!(&bytes[offset as usize..][..id.len()], &id[..]);
                }
            }
        }
    }
    // Literal and random ids are written the same way.
    for (policy, _) in BACKINGS {
        let path = dir.join(format!("hex-{policy:?}"));
        let ranges = gappy_layout(5000);
        let hex = BuildId::Hex(vec![0xde, 0xad, 0xbe, 0xef]);
        let (bytes, id) = build(&path, policy, 5000, &ranges, &hex, 10, Steps::Reserved);
        assert_eq!(id, [0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(&bytes[10..14], &id[..]);
        let (bytes, id) = build(
            &path,
            policy,
            5000,
            &ranges,
            &BuildId::Uuid,
            4984,
            Steps::Reserved,
        );
        assert_eq!(&bytes[4984..], &id[..]);
    }
}

#[test]
fn whole_image_access_sees_written_chunks() {
    let dir = scratch("whole-image");
    let len = 3 << 20;
    let ranges = straddling_layout(len);
    for (policy, backing) in BACKINGS {
        let path = dir.join(format!("{policy:?}"));
        let mut out = OutputFile::create(&path, len, &with_backing(policy)).unwrap();
        fill(&mut out, &ranges).unwrap();
        out.write_at(5, b"five").unwrap();
        let image = out.as_slice().unwrap().to_vec();
        let mut expected = expected_image(len, &ranges);
        expected[5..9].copy_from_slice(b"five");
        assert!(image == expected, "{policy:?}");
        out.as_mut_slice().unwrap()[len as usize - 1] = 0xaa;
        out.write_at(len - 2, b"\xbb").unwrap();
        assert_eq!(out.backing(), backing);
        out.finish().unwrap();
        expected[len as usize - 2..].copy_from_slice(b"\xbb\xaa");
        assert!(fs::read(&path).unwrap() == expected, "{policy:?}");
    }
}

#[test]
fn backing_policy_names() {
    assert_eq!(
        BackingPolicy::from_name("mmap"),
        Some(BackingPolicy::Mapped)
    );
    assert_eq!(
        BackingPolicy::from_name("write"),
        Some(BackingPolicy::Written)
    );
    assert_eq!(
        BackingPolicy::from_name("memory"),
        Some(BackingPolicy::Buffered)
    );
    assert_eq!(BackingPolicy::from_name("auto"), Some(BackingPolicy::Auto));
    assert_eq!(BackingPolicy::from_name("tape"), None);
    assert_ne!(BackingPolicy::Auto.resolve(), BackingPolicy::Auto);
    assert_eq!(BackingPolicy::Mapped.resolve(), BackingPolicy::Mapped);
}

/// Throughput check, not run by default:
/// `cargo test --release --test output -- --ignored --nocapture throughput`.
/// `QLD_OUTPUT_BENCH_MIB` sets the size (default 1024) and
/// `QLD_OUTPUT_BENCH_DIR` the directory (default: under the target dir).
#[test]
#[ignore = "benchmark; writes a large file"]
fn throughput() {
    let mib: u64 = std::env::var("QLD_OUTPUT_BENCH_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(1024);
    let len = mib << 20;
    let dir = match std::env::var_os("QLD_OUTPUT_BENCH_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => scratch("throughput"),
    };
    let path = dir.join("qld-throughput.out");
    let ranges: Vec<ChunkRange> = (0..len / (4 << 20))
        .map(|i| ChunkRange::new(i * (4 << 20), (4 << 20) - 4096))
        .collect();
    let seed = pattern(4 << 20, 5);
    for (policy, _) in BACKINGS {
        for replace in STRATEGIES {
            let _ = fs::remove_file(&path);
            for round in 0..2 {
                let mut out = OutputFile::create(&path, len, &options(replace, policy)).unwrap();
                out.reserve_build_id(&BuildId::Fast, 0);
                out.write_chunks(&ranges, |_, chunk| {
                    chunk.copy_from_slice(&seed[..chunk.len()]);
                    Ok(())
                })
                .unwrap();
                out.apply_build_id(&BuildId::Fast, 0).unwrap();
                let finished = out.finish().unwrap();
                eprintln!(
                    "{policy:?} {replace:?}, replacing an old output: {}: {}",
                    round > 0,
                    finished.stats()
                );
            }
        }
    }
    let _ = fs::remove_file(&path);
    let mut out = OutputFile::in_memory(len).unwrap();
    out.write_chunks(&ranges, |_, chunk| {
        chunk.copy_from_slice(&seed[..chunk.len()]);
        Ok(())
    })
    .unwrap();
    for kind in [BuildId::Fast, BuildId::Md5, BuildId::Sha1] {
        let start = std::time::Instant::now();
        std::hint::black_box(compute_build_id(&kind, out.as_slice().unwrap()));
        eprintln!("  build-id {kind:?}: {:?}", start.elapsed());
    }
    eprintln!("in memory: {}", out.finish().unwrap().stats());
}

#[test]
fn written_files_do_not_depend_on_thread_count() {
    let dir = scratch("threads");
    let len = 5 << 20;
    let ranges = gappy_layout(len);
    let mut outputs = Vec::new();
    for (policy, _) in BACKINGS {
        for threads in [1, 2, 8] {
            let path = dir.join(format!("out-{threads}-{policy:?}"));
            with_threads(threads, || {
                let mut out = OutputFile::create(&path, len, &with_backing(policy)).unwrap();
                out.reserve_build_id(&BuildId::Fast, 64);
                fill(&mut out, &ranges).unwrap();
                out.apply_build_id(&BuildId::Fast, 64).unwrap();
                out.finish().unwrap();
            });
            outputs.push(fs::read(&path).unwrap());
        }
    }
    for output in &outputs[1..] {
        assert!(output == &outputs[0]);
    }
}
