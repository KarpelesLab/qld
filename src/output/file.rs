//! Creating, filling and committing the output file.
//!
//! The replacement strategy, background release of the old output and
//! durability are documented on [`OutputFile`].

#![deny(clippy::arithmetic_side_effects)]

use super::build_id::{self, BlockHasher, build_id_size};
use super::chunks::{self, ChunkRange, LayoutError};
use super::mmap::map_output;
use super::positional;
use super::stats::{Backing, WritePhase, WriteStats};
use super::written::{self, HashPlan, Precomputed};
use crate::args::{BuildId, CancelToken, LinkOptions, OutputBuffer};
use crate::error::{Error, Result};
use memmap2::MmapMut;
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::thread::JoinHandle;
use std::time::Instant;

/// Permissions given to a newly created output file.
///
/// On non-Unix platforms this is ignored.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FileMode {
    /// `0o777` minus the process umask (`0o755` under the usual umask), like
    /// GNU ld for executables and shared objects.
    #[default]
    Executable,
    /// `0o666` minus the process umask, for relocatable (`-r`) and raw
    /// binary outputs.
    Regular,
    /// Exactly these permission bits, regardless of the umask.
    Exact(u32),
}

/// How a new output replaces an existing file. See [`OutputFile`] for the
/// tradeoffs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ReplaceStrategy {
    /// Write a temporary file in the same directory; when finished, remove
    /// the old output and rename the temporary file into place.
    #[default]
    Rename,
    /// Write a temporary file in the same directory and rename it directly
    /// over the old output, so the path always names a complete file.
    Atomic,
    /// Remove the existing file, then create the output at its final path.
    Unlink,
}

/// Which backing [`OutputFile::create`] uses for a regular output file.
/// See [`OutputFile`] ("Backings") for the tradeoffs.
///
/// Pipes, devices and paths under `/dev` and `/proc` are always buffered,
/// and an output that cannot be mapped falls back to a buffer; an in-memory
/// output ([`OutputFile::in_memory`]) is always [`Backing::Memory`].
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum BackingPolicy {
    /// The `QLD_OUTPUT_BACKING` environment variable if it is set to a known
    /// value (`mmap`, `write` or `memory`), otherwise
    /// [`BackingPolicy::DEFAULT`].
    #[default]
    Auto,
    /// Map the file writable ([`Backing::Mapped`]).
    Mapped,
    /// Write chunks with positional writes ([`Backing::Written`]), where the
    /// platform supports them (Unix and Windows); elsewhere, map.
    Written,
    /// Build the image in a heap buffer and write it on commit
    /// ([`Backing::Buffered`]).
    Buffered,
}

impl BackingPolicy {
    /// The environment variable read by [`BackingPolicy::Auto`].
    pub const ENV: &'static str = "QLD_OUTPUT_BACKING";

    /// What [`BackingPolicy::Auto`] means without the environment variable.
    pub const DEFAULT: Self = Self::Written;

    /// Parses a `QLD_OUTPUT_BACKING` value: `mmap`, `write` or `memory`
    /// (also `mapped`, `written`, `pwrite`, `buffered`, `buffer`, `auto`).
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "auto" => Some(Self::Auto),
            "mmap" | "mapped" => Some(Self::Mapped),
            "write" | "written" | "pwrite" => Some(Self::Written),
            "memory" | "buffer" | "buffered" => Some(Self::Buffered),
            _ => None,
        }
    }

    /// Resolves [`BackingPolicy::Auto`] from the environment and the default;
    /// other values are returned as they are.
    #[must_use]
    pub fn resolve(self) -> Self {
        match self {
            Self::Auto => std::env::var(Self::ENV)
                .ok()
                .and_then(|value| Self::from_name(value.trim()))
                .filter(|policy| *policy != Self::Auto)
                .unwrap_or(Self::DEFAULT),
            other => other,
        }
    }
}

/// Options for [`OutputFile::create`].
#[non_exhaustive]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OutputOptions {
    /// Permissions of the new file.
    pub mode: FileMode,
    /// How to replace an existing file.
    pub replace: ReplaceStrategy,
    /// Flush the mapping and `fsync` the file before [`OutputFile::finish`]
    /// returns. Off by default.
    pub sync: bool,
    /// Close the last handle to an old output at least this large on a
    /// background thread (Unix only). `None` disables it.
    pub background_release_threshold: Option<u64>,
    /// How the image is held while it is written.
    pub backing: BackingPolicy,
    /// Keep the image in memory and hand it to this buffer on
    /// [`OutputFile::finish`] instead of creating a file: the path is then
    /// only a name. See [`LinkOptions::output_buffer`].
    pub capture: Option<OutputBuffer>,
    /// Fail [`OutputFile::write_chunks`] and [`OutputFile::finish`] once this
    /// token is cancelled, leaving no output behind. See
    /// [`LinkOptions::cancel`].
    pub cancel: Option<CancelToken>,
}

impl OutputOptions {
    /// Default threshold for [`Self::background_release_threshold`]: 32 MiB,
    /// where freeing starts to cost milliseconds.
    pub const DEFAULT_RELEASE_THRESHOLD: u64 = 32 << 20;

    /// The defaults, with the output buffer and cancellation token of a link
    /// ([`LinkOptions::output_buffer`], [`LinkOptions::cancel`]). Link drivers
    /// create their main output with these.
    #[must_use]
    pub fn for_link(options: &LinkOptions) -> Self {
        Self {
            capture: options.output_buffer.clone(),
            cancel: options.cancel.clone(),
            ..Self::default()
        }
    }

    fn check_cancelled(&self) -> Result<()> {
        self.cancel.as_ref().map_or(Ok(()), CancelToken::check)
    }
}

impl Default for OutputOptions {
    fn default() -> Self {
        Self {
            mode: FileMode::Executable,
            replace: ReplaceStrategy::Rename,
            sync: false,
            background_release_threshold: Some(Self::DEFAULT_RELEASE_THRESHOLD),
            backing: BackingPolicy::Auto,
            capture: None,
            cancel: None,
        }
    }
}

/// Where the bytes live while the image is being written.
enum Storage {
    Mapped(MmapMut),
    Buffer(Vec<u8>),
    /// Nothing in memory: chunks go straight to the destination file.
    Positional(Positional),
}

/// State of the write backing.
struct Positional {
    len: usize,
    /// Whether anything has been written to the file yet.
    written: bool,
    /// The build-id announced by [`OutputFile::reserve_build_id`].
    plan: Option<HashPlan>,
    /// Block digests computed while writing, valid while nothing else is
    /// written.
    precomputed: Option<Precomputed>,
}

impl Positional {
    /// Records a write that the precomputed digests do not account for.
    fn touch(&mut self) {
        self.written = true;
        self.precomputed = None;
    }
}

impl Storage {
    fn len(&self) -> usize {
        match self {
            Self::Mapped(map) => map.len(),
            Self::Buffer(buf) => buf.len(),
            Self::Positional(state) => state.len,
        }
    }
}

/// The file behind a destination that owns one.
fn destination_file(destination: &Destination) -> Option<&File> {
    match destination {
        Destination::Temp { file, .. } | Destination::Direct { file } => Some(file),
        Destination::Stream { .. } | Destination::Memory | Destination::Done => None,
    }
}

/// An I/O error, naming `path` when there is one.
fn io_error(path: Option<&Path>, error: io::Error) -> Error {
    match path {
        Some(path) => Error::io(path, error),
        None => Error::from(error),
    }
}

fn no_file() -> Error {
    Error::Internal("positional output without a file".into())
}

/// Where the bytes go on finish.
enum Destination {
    /// Returned to the caller.
    Memory,
    /// A temporary file renamed over the output path.
    Temp { file: File, temp: PathBuf },
    /// The output path itself, created by us.
    Direct { file: File },
    /// An existing non-regular file (pipe, device).
    Stream { file: File },
    /// Finished or abandoned; nothing to do.
    Done,
}

/// An output image being written.
///
/// Create one with [`OutputFile::create`] (a file) or
/// [`OutputFile::in_memory`] (a byte vector), fill it through
/// [`OutputFile::write_chunks`] and [`OutputFile::write_at`] (or the whole
/// image, [`OutputFile::as_mut_slice`]), optionally apply a build-id, then
/// call [`OutputFile::finish`]. Dropping it without finishing removes the
/// partially written file.
///
/// # Backings
///
/// The final size is known before the first byte is written, so a regular
/// output file is created at full length with `set_len` (sparse, reading as
/// zeros). Its bytes are then held in one of three ways, chosen by
/// [`OutputOptions::backing`] and reported by [`OutputFile::backing`]:
///
/// - [`Backing::Written`] (the default): [`OutputFile::write_chunks`] gives
///   each chunk a zeroed heap buffer, and consecutive chunks are written to
///   the file with one positional write per region of about 1 MiB. No image
///   is kept in memory; memory use peaks at the largest chunks being written
///   at once. A build-id announced with [`OutputFile::reserve_build_id`] is
///   hashed from the buffers as they are written; otherwise the file is read
///   back. Whole-image access ([`OutputFile::as_mut_slice`],
///   [`OutputFile::split_chunks`]) reads the file into a heap buffer that is
///   written back on commit, so a writer that needs the whole image should
///   ask for it before writing chunks, when it costs only the allocation.
/// - [`Backing::Mapped`]: the file is mapped writable and chunks are slices
///   of the mapping. Page faults on a new file contend in the file system as
///   threads are added: on btrfs, 16 threads wrote 1 GiB 2–3 times slower
///   through a mapping than with positional writes.
/// - [`Backing::Buffered`]: a zeroed heap buffer written out on commit. Also
///   the fallback when mapping fails.
///
/// The output bytes, including the build-id, are identical with every
/// backing.
///
/// # Replacement strategy
///
/// How the new file replaces an existing one is a [`ReplaceStrategy`]. Every strategy creates a new
/// inode instead of truncating the old file, so a running copy of the old
/// output does not make the link fail with `ETXTBSY`, readers that have it
/// open or mapped (including qld itself, when an input is also the output)
/// keep seeing the old bytes, and the old page cache is not flushed. Hard
/// links to the old output keep the old contents (`docs/compatibility.md`).
///
/// - [`ReplaceStrategy::Rename`] (the default) writes a temporary file next
///   to the output (`.<name>.qld-<pid>-<n>.tmp`). On
///   [`OutputFile::finish`] it removes the old output and renames the
///   temporary file into place. A link that fails or is interrupted leaves
///   the previous output untouched, instead of a truncated file with a fresh
///   timestamp that `make` would consider up to date. The output path is
///   missing only between two consecutive system calls.
/// - [`ReplaceStrategy::Atomic`] renames the temporary file directly over
///   the old output, so the path always names a complete file. The price
///   is a filesystem heuristic: btrfs (for files over 16 MiB) and ext4
///   (`auto_da_alloc`) start flushing the new file's data when a rename
///   replaces an existing file, because that pattern usually means "replace
///   a config file safely". Measured on btrfs, committing a
///   1 GiB output took 270–550 ms with `Atomic` against 45 ms with `Rename`
///   or `Unlink`. That makes relinks I/O bound, so it is not the default.
/// - [`ReplaceStrategy::Unlink`] removes the existing file first and creates
///   the new one at the final path (GNU ld's approach). It is as fast as
///   `Rename`, but the output path is missing or partial while the link
///   runs. It is also the fallback when no temporary file can be created in
///   the output directory.
///
/// The temporary-file strategies cost a hidden temporary file left behind
/// if the process is killed (`SIGKILL`, or a panic with `panic = "abort"`),
/// and they replace a symlink at the output path rather than writing
/// through it (as `Unlink`, GNU ld and lld also do).
///
/// If the output path exists and is not a regular file (a pipe, a character
/// device such as `/dev/null`, a socket), it is opened for writing as is,
/// the image is built in memory, and the bytes are written out on finish.
/// On Unix, the same applies to any existing path under `/dev` or `/proc`
/// even when it resolves to a regular file: `-o /dev/stdout` with stdout
/// redirected to a file must write into that file, not replace the
/// `/dev/stdout` symlink (which a link running as root could otherwise do).
///
/// # Releasing a large old output
///
/// Deleting a file frees its blocks and page cache, which took about 70 ms
/// per GiB on btrfs on the development machine. On Unix, when the old
/// output is at least [`OutputOptions::background_release_threshold`]
/// bytes, the writer opens it before removing its directory entry and closes
/// that last handle on a background thread, so the freeing happens off the
/// link's main path. With `Unlink` this happens in [`OutputFile::create`],
/// concurrently with the rest of the link; with `Rename` and `Atomic`, at
/// commit. It gains nothing if the process exits right away, since the
/// kernel then does the work during exit; it helps library callers and
/// links that still have work to do (map files, a second output).
///
/// # Durability
///
/// By default nothing is `msync`ed or `fsync`ed: the kernel writes the pages
/// out in its own time, and the link stays CPU bound instead of waiting for
/// the disk. Set [`OutputOptions::sync`] to flush before returning.
///
/// # Example
///
/// ```
/// use qld::output::{ChunkRange, OutputFile};
///
/// let mut out = OutputFile::in_memory(8)?;
/// let layout = [ChunkRange::new(0, 2), ChunkRange::new(4, 4)];
/// let contents: [&[u8]; 2] = [b"hi", b"body"];
/// out.write_chunks(&layout, |i, chunk| {
///     chunk.copy_from_slice(contents[i]);
///     Ok(())
/// })?;
/// let finished = out.finish()?;
/// assert_eq!(finished.bytes(), Some(&b"hi\0\0body"[..]));
/// # Ok::<(), qld::Error>(())
/// ```
pub struct OutputFile {
    path: Option<PathBuf>,
    storage: Storage,
    destination: Destination,
    options: OutputOptions,
    release: Option<JoinHandle<()>>,
    stats: WriteStats,
}

impl std::fmt::Debug for OutputFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputFile")
            .field("path", &self.path)
            .field("len", &self.len())
            .field("backing", &self.stats.backing)
            .finish_non_exhaustive()
    }
}

impl OutputFile {
    /// Creates the output file at `path`, `size` bytes long and zero-filled.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] naming `path` if the file cannot be created,
    /// sized or opened, or if `size` does not fit in memory.
    pub fn create(path: &Path, size: u64, options: &OutputOptions) -> Result<Self> {
        options.check_cancelled()?;
        if options.capture.is_some() {
            let mut out = Self::in_memory(size)?;
            out.options = options.clone();
            return Ok(out);
        }
        let start = Instant::now();
        let len = usize::try_from(size).map_err(|_| Error::io(path, too_large()))?;

        let target = fs::metadata(path).ok();
        let is_special = target.as_ref().is_some_and(|meta| !meta.is_file());
        let mut release = None;
        let destination = if is_special || (target.is_some() && is_system_path(path)) {
            let file = OpenOptions::new()
                .write(true)
                // Truncating a pipe or device is meaningless; a regular file
                // reached through `/dev/stdout` must not keep a stale tail.
                .truncate(!is_special)
                .open(path)
                .map_err(|e| Error::io(path, e))?;
            Destination::Stream { file }
        } else {
            let temp = match options.replace {
                ReplaceStrategy::Rename | ReplaceStrategy::Atomic => {
                    create_temp(path, options.mode).ok()
                }
                ReplaceStrategy::Unlink => None,
            };
            match temp {
                Some((file, temp)) => Destination::Temp { file, temp },
                None => {
                    if let Some(old) = hold_old_output(path, options.background_release_threshold) {
                        release = release_in_background(old);
                    }
                    // Failure to remove is not fatal: the create below either
                    // succeeds (and truncates) or reports the real problem.
                    let _ = fs::remove_file(path);
                    let file = open_options(options.mode)
                        .truncate(true)
                        .open(path)
                        .map_err(|e| Error::io(path, e))?;
                    Destination::Direct { file }
                }
            }
        };

        let mut out = Self {
            path: Some(path.to_path_buf()),
            storage: Storage::Buffer(Vec::new()),
            destination,
            options: options.clone(),
            release,
            stats: WriteStats::new(size, Backing::Buffered),
        };
        out.stats.background_release = out.release.is_some();

        let mut storage = None;
        if let Some(file) = destination_file(&out.destination) {
            file.set_len(size).map_err(|e| Error::io(path, e))?;
            let policy = match options.backing.resolve() {
                BackingPolicy::Written if !positional::SUPPORTED => BackingPolicy::Mapped,
                policy => policy,
            };
            if len != 0 {
                storage = match policy {
                    BackingPolicy::Written => Some((
                        Backing::Written,
                        Storage::Positional(Positional {
                            len,
                            written: false,
                            plan: None,
                            precomputed: None,
                        }),
                    )),
                    BackingPolicy::Mapped | BackingPolicy::Auto => map_output(file, len)
                        .ok()
                        .map(|map| (Backing::Mapped, Storage::Mapped(map))),
                    BackingPolicy::Buffered => None,
                };
            }
        }
        out.storage = match storage {
            Some((backing, storage)) => {
                out.stats.backing = backing;
                storage
            }
            None => Storage::Buffer(zeroed(len).map_err(|e| Error::io(path, e))?),
        };
        out.stats.record(WritePhase::Open, start.elapsed());
        Ok(out)
    }

    /// Creates an output that is kept in memory and returned by
    /// [`Finished::into_bytes`], for library callers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if `size` bytes cannot be allocated.
    pub fn in_memory(size: u64) -> Result<Self> {
        let start = Instant::now();
        let len = usize::try_from(size).map_err(|_| Error::from(too_large()))?;
        let buf = zeroed(len)?;
        let mut stats = WriteStats::new(size, Backing::Memory);
        stats.record(WritePhase::Open, start.elapsed());
        Ok(Self {
            path: None,
            storage: Storage::Buffer(buf),
            destination: Destination::Memory,
            options: OutputOptions::default(),
            release: None,
            stats,
        })
    }

    /// The output path, or `None` for an in-memory output.
    #[must_use]
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Size of the image in bytes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.storage.len()
    }

    /// Whether the image is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// How the bytes are held.
    #[must_use]
    pub fn backing(&self) -> Backing {
        self.stats.backing
    }

    /// The whole image.
    ///
    /// With [`Backing::Written`] this first reads the file into a heap
    /// buffer, which is then used instead of positional writes (see
    /// [`OutputFile`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the buffer cannot be allocated or the file
    /// cannot be read back.
    pub fn as_slice(&mut self) -> Result<&[u8]> {
        self.as_mut_slice().map(|image| &*image)
    }

    /// The whole image, writable.
    ///
    /// With [`Backing::Written`] this first reads the file into a heap
    /// buffer, which is then used instead of positional writes (see
    /// [`OutputFile`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if the buffer cannot be allocated or the file
    /// cannot be read back.
    pub fn as_mut_slice(&mut self) -> Result<&mut [u8]> {
        self.materialize()?;
        match &mut self.storage {
            Storage::Mapped(map) => Ok(map),
            Storage::Buffer(buf) => Ok(buf),
            Storage::Positional(_) => Err(Error::Internal("output image not in memory".into())),
        }
    }

    /// Replaces positional storage with a heap buffer holding the image.
    fn materialize(&mut self) -> Result<()> {
        let Storage::Positional(state) = &self.storage else {
            return Ok(());
        };
        let path = self.path.as_deref();
        let mut buf = zeroed(state.len).map_err(|e| io_error(path, e))?;
        if state.written {
            let file = destination_file(&self.destination).ok_or_else(no_file)?;
            written::read_buffer(file, &mut buf, 0).map_err(|e| io_error(path, e))?;
        }
        self.storage = Storage::Buffer(buf);
        Ok(())
    }

    /// Writes `bytes` at `offset` in the image, with any backing.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Internal`] if the range does not fit in the image,
    /// and [`Error::Io`] if the write fails.
    pub fn write_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        let range = ChunkRange::new(offset, bytes.len() as u64);
        let len = self.len() as u64;
        let path = self.path.as_deref();
        let out_of_bounds = || layout_error(path, LayoutError::FieldOutOfBounds { range, len });
        let end = range
            .end()
            .filter(|end| *end <= len)
            .ok_or_else(out_of_bounds)?;
        match &mut self.storage {
            Storage::Positional(state) => {
                let file = destination_file(&self.destination).ok_or_else(no_file)?;
                state.touch();
                written::write_buffer(file, bytes, offset).map_err(|e| io_error(path, e))?;
            }
            Storage::Mapped(map) => {
                copy_range(map, offset, end, bytes).ok_or_else(out_of_bounds)?
            }
            Storage::Buffer(buf) => {
                copy_range(buf, offset, end, bytes).ok_or_else(out_of_bounds)?
            }
        }
        Ok(())
    }

    /// Statistics recorded so far.
    #[must_use]
    pub fn stats(&self) -> &WriteStats {
        &self.stats
    }

    /// Statistics, for callers recording their own phases (typically
    /// [`WritePhase::Write`]).
    pub fn stats_mut(&mut self) -> &mut WriteStats {
        &mut self.stats
    }

    /// Splits the image into disjoint writable chunks; see
    /// [`chunks::split_chunks`]. This needs the whole image, like
    /// [`OutputFile::as_mut_slice`]; prefer [`OutputFile::write_chunks`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::Internal`] describing the [`LayoutError`] if the
    /// layout is invalid, and [`Error::Io`] if the image cannot be held in
    /// memory.
    pub fn split_chunks(&mut self, ranges: &[ChunkRange]) -> Result<Vec<&mut [u8]>> {
        let path = self.path.clone();
        chunks::validate_layout(ranges, self.len() as u64)
            .map_err(|e| layout_error(path.as_deref(), e))?;
        chunks::split_chunks(self.as_mut_slice()?, ranges)
            .map_err(|e| layout_error(path.as_deref(), e))
    }

    /// Writes every chunk in parallel with `write(index, chunk)`, and adds the
    /// elapsed time to [`WritePhase::Write`]; see [`chunks::write_chunks`].
    ///
    /// Every chunk is zero-filled when `write` receives it, whatever the
    /// backing, and `write` must only touch its own chunk. With
    /// [`Backing::Written`] the chunk is a heap buffer written to the file
    /// after `write` returns. If a chunk fails, other chunks may or may not
    /// have been written.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid layout, the first error (in layout
    /// order) returned by `write`, or [`Error::Io`] if writing the file
    /// fails.
    pub fn write_chunks<F>(&mut self, ranges: &[ChunkRange], write: F) -> Result<()>
    where
        F: Fn(usize, &mut [u8]) -> Result<()> + Sync,
    {
        let start = Instant::now();
        let path = self.path.as_deref();
        let len = self.storage.len() as u64;
        let cancel = self.options.cancel.clone();
        let write = |index: usize, chunk: &mut [u8]| {
            if let Some(token) = &cancel {
                token.check()?;
            }
            write(index, chunk)
        };
        let result = chunks::validate_layout(ranges, len)
            .map_err(|e| layout_error(path, e))
            .and_then(|()| match &mut self.storage {
                Storage::Positional(state) => {
                    let (Some(file), Some(path)) = (destination_file(&self.destination), path)
                    else {
                        return Err(no_file());
                    };
                    // Digests computed while writing are only valid over a
                    // file that holds nothing else.
                    let plan = state.plan.as_ref().filter(|_| !state.written);
                    let outcome = written::write_chunks(file, path, len, ranges, write, plan);
                    state.touch();
                    state.precomputed = outcome?;
                    Ok(())
                }
                Storage::Mapped(map) => run_chunks(map, ranges, path, &write),
                Storage::Buffer(buf) => run_chunks(buf, ranges, path, &write),
            });
        self.stats.record(WritePhase::Write, start.elapsed());
        result
    }

    /// Announces that [`OutputFile::apply_build_id`] will be called with
    /// `kind` and `offset` once the chunks are written.
    ///
    /// This never changes the output. With [`Backing::Written`], the next
    /// [`OutputFile::write_chunks`] then hashes the chunk buffers as it
    /// writes them, so that `apply_build_id` does not have to read the file
    /// back; that only works for a `write_chunks` call before anything else
    /// is written. Other backings ignore it.
    pub fn reserve_build_id(&mut self, kind: &BuildId, offset: u64) {
        if let Storage::Positional(state) = &mut self.storage {
            state.plan = BlockHasher::new(kind).map(|_| HashPlan {
                kind: kind.clone(),
                field: ChunkRange::new(offset, build_id_size(kind).unwrap_or(0) as u64),
            });
        }
    }

    /// Computes the build-id over the finished image with the field at
    /// `offset` zeroed, patches it in, and returns it; see
    /// [`build_id::apply_build_id`]. Returns `Ok(None)` for
    /// [`BuildId::None`].
    ///
    /// # Errors
    ///
    /// Returns an error if the field does not fit in the image.
    pub fn apply_build_id(&mut self, kind: &BuildId, offset: u64) -> Result<Option<Vec<u8>>> {
        let start = Instant::now();
        let path = self.path.as_deref();
        let result = match &mut self.storage {
            Storage::Mapped(map) => {
                build_id::apply_build_id(kind, map, offset).map_err(|e| layout_error(path, e))
            }
            Storage::Buffer(buf) => {
                build_id::apply_build_id(kind, buf, offset).map_err(|e| layout_error(path, e))
            }
            Storage::Positional(_) => self.apply_build_id_positional(kind, offset),
        };
        self.stats.record(WritePhase::BuildId, start.elapsed());
        result
    }

    /// [`OutputFile::apply_build_id`] for [`Backing::Written`]: the same
    /// value, from block digests computed while writing or read back from
    /// the file.
    fn apply_build_id_positional(
        &mut self,
        kind: &BuildId,
        offset: u64,
    ) -> Result<Option<Vec<u8>>> {
        let Some(size) = build_id_size(kind) else {
            return Ok(None);
        };
        let len = self.storage.len() as u64;
        let path = self.path.as_deref();
        let field = ChunkRange::new(offset, size as u64);
        if field.end().is_none_or(|end| end > len) {
            return Err(layout_error(
                path,
                LayoutError::FieldOutOfBounds { range: field, len },
            ));
        }
        let (Storage::Positional(state), Some(file)) =
            (&mut self.storage, destination_file(&self.destination))
        else {
            return Err(no_file());
        };
        let precomputed = state
            .precomputed
            .take()
            .filter(|pre| pre.plan.kind == *kind && pre.plan.field == field);
        let id = match written::finish_build_id(file, len, kind, field, precomputed)
            .map_err(|e| io_error(path, e))?
        {
            Some(id) => id,
            // `uuid` and literal ids do not depend on the contents.
            None => match build_id::compute_build_id(kind, &[]) {
                Some(id) => id,
                None => return Ok(None),
            },
        };
        // As with the other backings, the field is zeroed and then the id
        // written over it.
        let mut patch = vec![0u8; size];
        if let Some(dest) = patch.get_mut(..id.len()) {
            dest.copy_from_slice(&id);
        }
        self.write_at(offset, &patch)?;
        Ok(Some(id))
    }

    /// Commits the output: writes out a buffered image, applies permissions,
    /// renames a temporary file into place, and returns the statistics (and
    /// the bytes, for an in-memory output).
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] naming the output path. A temporary file is
    /// removed on failure, and the previous output is left in place.
    pub fn finish(mut self) -> Result<Finished> {
        // Dropping `self` on this error removes a partial file.
        self.options.check_cancelled()?;
        let start = Instant::now();
        let storage = std::mem::replace(&mut self.storage, Storage::Buffer(Vec::new()));
        let destination = std::mem::replace(&mut self.destination, Destination::Done);
        let mut bytes = None;
        let mut release = self.release.take();

        match (destination, self.path.as_deref()) {
            (Destination::Memory, _) | (_, None) => {
                bytes = Some(match storage {
                    Storage::Buffer(buf) => buf,
                    Storage::Mapped(map) => map.to_vec(),
                    // Only created with a file; nothing to return.
                    Storage::Positional(_) => Vec::new(),
                });
            }
            (Destination::Done, Some(_)) => {}
            (Destination::Stream { mut file }, Some(path)) => {
                let image: &[u8] = match &storage {
                    Storage::Buffer(buf) => buf,
                    Storage::Mapped(map) => map,
                    // Only created for regular files.
                    Storage::Positional(_) => &[],
                };
                file.write_all(image)
                    .and_then(|()| file.flush())
                    .map_err(|e| Error::io(path, e))?;
            }
            (Destination::Direct { file }, Some(path)) => {
                if let Err(e) = commit_file(&file, storage, &self.options) {
                    drop(file);
                    let _ = fs::remove_file(path);
                    return Err(Error::io(path, e));
                }
            }
            (Destination::Temp { file, temp }, Some(path)) => {
                let committed = commit_file(&file, storage, &self.options).and_then(|()| {
                    drop(file);
                    let old = hold_old_output(path, self.options.background_release_threshold);
                    if self.options.replace == ReplaceStrategy::Rename {
                        // Renaming over an existing file makes btrfs and ext4
                        // flush the new file's data synchronously; removing
                        // the old one first avoids that. If removal fails,
                        // the rename reports the problem.
                        let _ = fs::remove_file(path);
                    }
                    fs::rename(&temp, path)?;
                    if let Some(old) = old {
                        release = release_in_background(old);
                    }
                    Ok(())
                });
                if let Err(e) = committed {
                    let _ = fs::remove_file(&temp);
                    return Err(Error::io(path, e));
                }
            }
        }

        if let Some(buffer) = &self.options.capture
            && let Some(image) = bytes.take()
        {
            buffer.store(image);
        }
        let mut stats = std::mem::take(&mut self.stats);
        stats.background_release |= release.is_some();
        stats.record(WritePhase::Commit, start.elapsed());
        Ok(Finished {
            bytes,
            stats,
            release,
        })
    }
}

impl Drop for OutputFile {
    fn drop(&mut self) {
        // Unmap before removing: Windows cannot delete a mapped file.
        drop(std::mem::replace(
            &mut self.storage,
            Storage::Buffer(Vec::new()),
        ));
        match std::mem::replace(&mut self.destination, Destination::Done) {
            Destination::Temp { file, temp } => {
                drop(file);
                let _ = fs::remove_file(temp);
            }
            Destination::Direct { file } => {
                drop(file);
                if let Some(path) = &self.path {
                    let _ = fs::remove_file(path);
                }
            }
            Destination::Stream { .. } | Destination::Memory | Destination::Done => {}
        }
    }
}

/// The result of [`OutputFile::finish`].
#[derive(Debug)]
pub struct Finished {
    bytes: Option<Vec<u8>>,
    stats: WriteStats,
    release: Option<JoinHandle<()>>,
}

impl Finished {
    /// The image, for an in-memory output.
    #[must_use]
    pub fn bytes(&self) -> Option<&[u8]> {
        self.bytes.as_deref()
    }

    /// Takes the image, for an in-memory output.
    #[must_use]
    pub fn into_bytes(self) -> Option<Vec<u8>> {
        self.bytes
    }

    /// Size and timing of the write.
    #[must_use]
    pub fn stats(&self) -> &WriteStats {
        &self.stats
    }

    /// Waits for a background release of the old output to complete, if one
    /// is running. Dropping [`Finished`] instead lets it continue detached.
    pub fn wait_for_release(&mut self) {
        if let Some(handle) = self.release.take() {
            let _ = handle.join();
        }
    }
}

fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        "output is larger than the address space",
    )
}

/// Copies `bytes` into `image[offset..end]`.
fn copy_range(image: &mut [u8], offset: u64, end: u64, bytes: &[u8]) -> Option<()> {
    let dest = image.get_mut(usize::try_from(offset).ok()?..usize::try_from(end).ok()?)?;
    dest.copy_from_slice(bytes);
    Some(())
}

/// Runs `write` over the chunks of an in-memory image, in parallel.
fn run_chunks<F>(
    image: &mut [u8],
    ranges: &[ChunkRange],
    path: Option<&Path>,
    write: &F,
) -> Result<()>
where
    F: Fn(usize, &mut [u8]) -> Result<()> + Sync,
{
    use rayon::prelude::*;
    let slices = chunks::split_chunks(image, ranges).map_err(|e| layout_error(path, e))?;
    let results: Vec<Result<()>> = slices
        .into_par_iter()
        .enumerate()
        .map(|(index, chunk)| write(index, chunk))
        .collect();
    results.into_iter().collect()
}

fn layout_error(path: Option<&Path>, error: LayoutError) -> Error {
    match path {
        Some(path) => Error::Internal(format!(
            "{}: invalid output layout: {error}",
            path.display()
        )),
        None => Error::Internal(format!("invalid output layout: {error}")),
    }
}

/// Allocates a zero-filled buffer, reporting allocation failure as an error
/// instead of aborting.
fn zeroed(len: usize) -> io::Result<Vec<u8>> {
    let mut buf = Vec::new();
    buf.try_reserve_exact(len)
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
    buf.resize(len, 0);
    Ok(buf)
}

/// Open options for a new output: read+write (a shared writable mapping
/// needs both) and the creation mode.
fn open_options(mode: FileMode) -> OpenOptions {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(match mode {
            FileMode::Executable => 0o777,
            FileMode::Regular => 0o666,
            // Applied exactly on commit.
            FileMode::Exact(_) => 0o600,
        });
    }
    #[cfg(not(unix))]
    let _ = mode;
    options
}

/// Whether `path` names a special location whose directory entry must never
/// be replaced, such as `/dev/stdout` or `/proc/self/fd/1`.
fn is_system_path(path: &Path) -> bool {
    cfg!(unix) && (path.starts_with("/dev") || path.starts_with("/proc"))
}

/// Creates a new, uniquely named temporary file next to `path`.
fn create_temp(path: &Path, mode: FileMode) -> io::Result<(File, PathBuf)> {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let name = path.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "output path has no file name")
    })?;
    let dir = match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    };
    let mut last_error = None;
    for _ in 0..16 {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let mut temp_name = OsString::from(".");
        temp_name.push(name);
        temp_name.push(format!(".qld-{}-{n}.tmp", std::process::id()));
        let temp = dir.join(temp_name);
        let mut options = open_options(mode);
        options.create_new(true);
        match options.open(&temp) {
            Ok(file) => return Ok((file, temp)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => last_error = Some(e),
            Err(e) => return Err(e),
        }
    }
    Err(last_error.unwrap_or_else(|| io::Error::from(io::ErrorKind::AlreadyExists)))
}

/// Writes a buffered image into `file` (a mapped one is already there),
/// applies exact permissions, and syncs if asked.
fn commit_file(file: &File, storage: Storage, options: &OutputOptions) -> io::Result<()> {
    match storage {
        Storage::Mapped(map) => {
            if options.sync {
                map.flush()?;
            }
            drop(map);
        }
        Storage::Buffer(buf) => {
            if positional::SUPPORTED {
                // Positional writes move the cursor on Windows, so write at
                // an explicit offset.
                written::write_buffer(file, &buf, 0)?;
            } else {
                let mut writer = file;
                writer.write_all(&buf)?;
            }
        }
        // Everything is already in the file.
        Storage::Positional(_) => {}
    }
    #[cfg(unix)]
    if let FileMode::Exact(mode) = options.mode {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    if options.sync {
        file.sync_all()?;
    }
    Ok(())
}

/// Opens the existing output so that its last reference can be dropped on
/// a background thread after its directory entry is replaced.
#[cfg(unix)]
fn hold_old_output(path: &Path, threshold: Option<u64>) -> Option<File> {
    use std::os::unix::fs::MetadataExt;
    let threshold = threshold?;
    // Not following symlinks: replacing a symlink frees nothing.
    let meta = fs::symlink_metadata(path).ok()?;
    if !meta.is_file() || meta.len() < threshold || meta.nlink() != 1 {
        return None;
    }
    File::open(path).ok()
}

/// On Windows an open handle can prevent the file from being replaced, so
/// the old output is released synchronously.
#[cfg(not(unix))]
fn hold_old_output(_path: &Path, _threshold: Option<u64>) -> Option<File> {
    None
}

/// Closes `file` on a new thread. If the thread cannot be spawned, the file
/// is closed here instead.
fn release_in_background(file: File) -> Option<JoinHandle<()>> {
    std::thread::Builder::new()
        .name("qld-release-output".into())
        .spawn(move || drop(file))
        .ok()
}
