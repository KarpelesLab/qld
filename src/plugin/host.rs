//! The linker side of the plugin interface: process-global state and the C
//! callbacks plugins call.
//!
//! # Why global state
//!
//! Most callbacks carry no context pointer: a plugin calls `message`,
//! `add_input_file` or `register_cleanup` with only its own arguments. The
//! state they act on therefore lives in one process-wide [`STATE`], and only
//! one [`Session`](super::Session) can exist at a time.
//!
//! # Locking
//!
//! [`STATE`] is locked by every callback and by the session's own
//! operations, but never while calling into a plugin: a plugin handler calls
//! back into the host on the same thread, so holding the lock across that
//! call would deadlock. Callbacks may also come from plugin threads (LLVM's
//! ThinLTO backends report diagnostics from worker threads), which the mutex
//! serializes.
//!
//! # Trust
//!
//! Loading a plugin runs its code in-process, so a plugin is trusted not to
//! corrupt memory, as with every linker that hosts plugins. Within that
//! trust the host is defensive: it checks every pointer it can (null
//! pointers, negative counts, unknown handles, out-of-range enumerators) and
//! answers with an error status instead of crashing. It cannot check that a
//! non-null pointer points where the interface says, or that a string is
//! NUL-terminated. No callback lets a panic unwind into plugin code.

#![allow(unsafe_code)] // FFI with the plugin; each block states its invariant.

use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::ffi::{CStr, CString, OsString, c_char, c_int, c_uint, c_void};
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::ptr::{self, NonNull};
use std::sync::{Mutex, MutexGuard, PoisonError};

use memmap2::Mmap;

use super::abi::{
    self, ClaimFileFn, ClaimFileV2Fn, NewInputFn, OnloadFn, RawInputFile, RawSection, RawSymbol,
    STATUS_BAD_HANDLE, STATUS_ERR, STATUS_NO_SYMS, STATUS_OK, Status, Tv, TvValue, VoidHandlerFn,
    tag,
};
use super::dl::Library;
use super::format::{Arguments, format_message};
use super::session::{PluginInfo, SessionOptions};
use super::types::{
    ClaimedFile, ClaimedSymbol, FileResolution, InputFile, LtoOutput, MessageLevel, PluginMessage,
    SectionKind, SectionRef, SymbolKind, SymbolResolution, SymbolType, UniqueSegment, Visibility,
};
use crate::diag::{Diagnostic, DiagnosticSink, Severity};
use crate::elf::read::{Elf32Be, Elf32Le, Elf64Be, Elf64Le, ElfFile, ElfFormat, ElfKind, Source};
use crate::error::{Error, Result};

/// The active session's state, if any.
static STATE: Mutex<Option<Host>> = Mutex::new(None);

/// `dlopen` identities of plugins whose `onload` has run in this process.
/// Plugins keep process-level state that does not survive a second link (see
/// [`load`]), so each may be used once.
static USED_PLUGINS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Identifier and version qld gives plugins that negotiate an API level.
const LINKER_IDENTIFIER: &CStr = c"qld";
const LINKER_VERSION: &CStr =
    match CStr::from_bytes_with_nul(concat!(env!("CARGO_PKG_VERSION"), "\0").as_bytes()) {
        Ok(version) => version,
        Err(_) => c"0",
    };

fn lock() -> MutexGuard<'static, Option<Host>> {
    STATE.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Where the session is in the plugin protocol.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    /// Plugins may be loaded.
    Loading,
    /// Files are being claimed; no more plugins may be loaded.
    Claiming,
    /// The all-symbols-read handlers ran (or are running).
    AllSymbolsRead,
    /// Cleanup handlers ran.
    Finished,
}

/// One loaded plugin and the handlers it registered.
#[derive(Debug)]
struct Plugin {
    path: PathBuf,
    claim: Option<ClaimFileFn>,
    claim_v2: Option<ClaimFileV2Fn>,
    all_symbols_read: Option<VoidHandlerFn>,
    cleanup: Option<VoidHandlerFn>,
    new_input: Option<NewInputFn>,
    identifier: Option<String>,
    version: Option<String>,
    api_level: Option<c_int>,
}

/// A claim handler, of either version.
#[derive(Clone, Copy, Debug)]
enum ClaimHandler {
    V1(ClaimFileFn),
    V2(ClaimFileV2Fn),
}

/// How far the linker resolved a claimed file.
#[derive(Debug)]
enum Resolved {
    Pending,
    Included(Vec<SymbolResolution>),
    NotIncluded,
}

/// A file the plugins were shown, claimed or not. Its index in
/// [`Host::files`] is the plugin handle (plus one, so it is never null).
#[derive(Debug)]
struct FileRecord {
    handle: u64,
    path: PathBuf,
    name: CString,
    offset: u64,
    size: u64,
    /// A claim handler is running for this file: `add_symbols` is allowed.
    in_claim: bool,
    claimed_by: Option<usize>,
    /// Symbols added during the claim, moved to the session afterwards.
    pending: Vec<ClaimedSymbol>,
    symbol_count: usize,
    resolution: Resolved,
}

/// The bytes of an open file, kept at a stable address for the session.
#[derive(Debug)]
enum Bytes {
    Mapped(Mmap),
    Owned(Box<[u8]>),
}

/// An input file kept open for the plugins.
#[derive(Debug)]
struct OpenFile {
    file: File,
    bytes: Option<Bytes>,
}

impl OpenFile {
    fn bytes(&mut self) -> io::Result<&[u8]> {
        if self.bytes.is_none() {
            // SAFETY: the mapping is read-only and private. Another process
            // modifying the file while it is mapped is the hazard every
            // mmap-based linker accepts for its inputs (docs/development.md,
            // "`unsafe` policy"). The `Mmap` lives in `self.bytes` until the
            // session ends, which outlives every view handed to a plugin.
            let bytes = match unsafe { Mmap::map(&self.file) } {
                Ok(map) => Bytes::Mapped(map),
                Err(_) => {
                    let mut buffer = Vec::new();
                    (&self.file).read_to_end(&mut buffer)?;
                    Bytes::Owned(buffer.into_boxed_slice())
                }
            };
            self.bytes = Some(bytes);
        }
        Ok(match &self.bytes {
            Some(Bytes::Mapped(map)) => map,
            Some(Bytes::Owned(bytes)) => bytes,
            None => &[],
        })
    }
}

/// C strings handed out by `get_wrap_symbols`.
#[derive(Debug, Default)]
struct WrapList {
    /// Owns the strings `pointers` points into.
    #[allow(dead_code)]
    names: Vec<CString>,
    pointers: Box<[*const c_char]>,
}

// SAFETY: the pointers point into `names`, which the list owns and never
// changes after construction; nothing mutates through them.
unsafe impl Send for WrapList {}

/// Everything a session knows.
#[derive(Debug)]
struct Host {
    options: SessionOptions,
    output_name: Option<&'static CStr>,
    wrap: WrapList,
    plugins: Vec<Plugin>,
    /// The plugin the host is calling right now, for registrations.
    current: Option<usize>,
    files: Vec<FileRecord>,
    open: BTreeMap<PathBuf, OpenFile>,
    phase: Phase,
    added_files: Vec<PathBuf>,
    added_libraries: Vec<OsString>,
    library_paths: Vec<PathBuf>,
    messages: Vec<PluginMessage>,
    /// A plugin reported a fatal error; the session can only be finished.
    failed: bool,
    section_ordering_allowed: bool,
    section_order: Option<Vec<SectionRef>>,
    unique_segment_allowed: bool,
    unique_segments: Vec<UniqueSegment>,
}

impl Host {
    fn file(&self, handle: *const c_void) -> Option<(usize, &FileRecord)> {
        let index = handle.addr().checked_sub(1)?;
        self.files.get(index).map(|record| (index, record))
    }

    fn open(&mut self, path: &Path) -> io::Result<&mut OpenFile> {
        match self.open.entry(path.to_path_buf()) {
            Entry::Occupied(entry) => Ok(entry.into_mut()),
            Entry::Vacant(entry) => Ok(entry.insert(OpenFile {
                file: File::open(path)?,
                bytes: None,
            })),
        }
    }

    /// The bytes of file record `index`, or `None` if they cannot be read or
    /// the record's range lies outside the file.
    fn view(&mut self, index: usize) -> Option<&[u8]> {
        let record = self.files.get(index)?;
        let (path, offset, size) = (record.path.clone(), record.offset, record.size);
        let bytes = self.open(&path).ok()?.bytes().ok()?;
        let start = usize::try_from(offset).ok()?;
        let end = start.checked_add(usize::try_from(size).ok()?)?;
        bytes.get(start..end)
    }

    /// Closes `path` unless a claimed file still needs it.
    fn release_if_unclaimed(&mut self, path: &Path) {
        let needed = self
            .files
            .iter()
            .any(|record| record.claimed_by.is_some() && record.path == path);
        if !needed {
            self.open.remove(path);
        }
    }

    fn raw_file(&self, index: usize, fd: c_int) -> Option<RawInputFile> {
        let record = self.files.get(index)?;
        Some(RawInputFile {
            name: record.name.as_ptr(),
            fd,
            offset: i64::try_from(record.offset).ok()?,
            filesize: i64::try_from(record.size).ok()?,
            handle: handle_for(index),
        })
    }

    fn section_ref(&self, section: &RawSection) -> Option<SectionRef> {
        let (_, record) = self.file(section.handle)?;
        Some(SectionRef {
            file: record.handle,
            index: section.shndx,
        })
    }
}

/// The plugin handle for file record `index`.
fn handle_for(index: usize) -> *mut c_void {
    ptr::without_provenance_mut(index.wrapping_add(1))
}

fn active(guard: &mut Option<Host>) -> Result<&mut Host> {
    guard
        .as_mut()
        .ok_or_else(|| Error::Internal("no LTO plugin session is active".to_owned()))
}

fn usable(guard: &mut Option<Host>) -> Result<&mut Host> {
    let host = active(guard)?;
    if host.failed {
        return Err(Error::Internal(
            "LTO plugin session used after a fatal plugin error".to_owned(),
        ));
    }
    Ok(host)
}

// ---------------------------------------------------------------------------
// Session operations
// ---------------------------------------------------------------------------

/// Starts the process's session.
pub(super) fn begin(options: SessionOptions) -> Result<()> {
    let mut guard = lock();
    if guard.is_some() {
        return Err(Error::Limit(
            "only one LTO plugin session can be active in a process".to_owned(),
        ));
    }
    let output_name = match &options.output_name {
        Some(path) => Some(leak_c_string(
            path.as_os_str().as_bytes(),
            "output file name",
        )?),
        None => None,
    };
    let mut names = Vec::with_capacity(options.wrap_symbols.len());
    for symbol in &options.wrap_symbols {
        names.push(
            CString::new(symbol.as_slice())
                .map_err(|_| Error::Option("--wrap symbol contains a NUL byte".to_owned()))?,
        );
    }
    let pointers = names.iter().map(|name| name.as_ptr()).collect();
    *guard = Some(Host {
        options,
        output_name,
        wrap: WrapList { names, pointers },
        plugins: Vec::new(),
        current: None,
        files: Vec::new(),
        open: BTreeMap::new(),
        phase: Phase::Loading,
        added_files: Vec::new(),
        added_libraries: Vec::new(),
        library_paths: Vec::new(),
        messages: Vec::new(),
        failed: false,
        section_ordering_allowed: false,
        section_order: None,
        unique_segment_allowed: false,
        unique_segments: Vec::new(),
    });
    Ok(())
}

/// Copies `bytes` into a C string that lives for the rest of the process.
///
/// Plugins keep the pointers they get in the transfer vector (LLVM's plugin
/// stores option strings and reads them when it compiles), and a plugin is
/// never unloaded, so these strings must never be freed. They are small and
/// allocated once per plugin load.
fn leak_c_string(bytes: &[u8], what: &str) -> Result<&'static CStr> {
    let string =
        CString::new(bytes).map_err(|_| Error::Option(format!("{what} contains a NUL byte")))?;
    Ok(Box::leak(string.into_boxed_c_str()))
}

/// Loads one plugin and runs its `onload`.
pub(super) fn load(
    path: &Path,
    options: &[String],
    diagnostics: &dyn DiagnosticSink,
) -> Result<()> {
    {
        let mut guard = lock();
        let host = usable(&mut guard)?;
        if host.phase != Phase::Loading {
            return Err(Error::Internal(
                "LTO plugins must be loaded before any file is claimed".to_owned(),
            ));
        }
    }
    let option_strings = options
        .iter()
        .map(|option| leak_c_string(option.as_bytes(), "-plugin-opt value"))
        .collect::<Result<Vec<_>>>()?;

    if let Err(error) = std::fs::metadata(path) {
        return Err(Error::io(path, error));
    }
    let library = Library::open(path).map_err(|error| Error::io(path, error))?;
    let entry = library
        .symbol(c"onload")
        .or_else(|_| library.symbol(c"_onload"))
        .map_err(|error| {
            Error::io(
                path,
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("not a linker plugin (no `onload` entry point): {error}"),
                ),
            )
        })?;
    {
        let mut used = USED_PLUGINS.lock().unwrap_or_else(PoisonError::into_inner);
        if used.contains(&library.id()) {
            return Err(Error::Limit(format!(
                "{}: this plugin was already used in this process, and plugins support \
                 one link per process",
                path.display()
            )));
        }
        used.push(library.id());
    }
    // SAFETY: `onload` is the plugin's entry point, whose signature the plugin
    // interface fixes. Function and data pointers have the same size and
    // representation on every Unix host.
    let onload = unsafe { std::mem::transmute::<*mut c_void, OnloadFn>(entry) };

    let vector = {
        let mut guard = lock();
        let host = usable(&mut guard)?;
        let index = host.plugins.len();
        host.plugins.push(Plugin {
            path: path.to_path_buf(),
            claim: None,
            claim_v2: None,
            all_symbols_read: None,
            cleanup: None,
            new_input: None,
            identifier: None,
            version: None,
            api_level: None,
        });
        host.current = Some(index);
        transfer_vector(host, &option_strings)
    };
    // The vector itself is only read during `onload`, but GNU ld keeps its
    // copy alive for the whole run and a plugin could rely on that.
    let vector: &'static mut [Tv] = Box::leak(vector.into_boxed_slice());

    // SAFETY: `vector` is a NULL-terminated transfer vector whose strings and
    // callbacks stay valid for the rest of the process. The plugin is trusted
    // to follow the interface (see the module documentation).
    let status = unsafe { onload(vector.as_mut_ptr()) };
    set_current(None);
    // `library` is not closed when it goes out of scope: the plugin stays
    // loaded for the process lifetime (see `Library`).

    let outcome = report(diagnostics);
    if outcome.fatal {
        return Err(Error::Reported {
            errors: outcome.errors.max(1),
        });
    }
    if status != STATUS_OK {
        diagnostics.emit(Diagnostic::error(format!(
            "{}: plugin failed to load (status {status})",
            path.display()
        )));
        fail();
        return Err(Error::Reported {
            errors: outcome.errors + 1,
        });
    }
    Ok(())
}

fn set_current(plugin: Option<usize>) {
    if let Some(host) = lock().as_mut() {
        host.current = plugin;
    }
}

fn fail() {
    if let Some(host) = lock().as_mut() {
        host.failed = true;
    }
}

/// Builds the transfer vector for the next plugin, in the order GNU ld uses
/// (the message callback first, so a plugin can report errors in options).
fn transfer_vector(host: &Host, options: &[&'static CStr]) -> Vec<Tv> {
    fn val(tag: c_int, val: c_int) -> Tv {
        Tv {
            tag,
            value: TvValue { val },
        }
    }
    fn function(tag: c_int, function: *const ()) -> Tv {
        Tv {
            tag,
            value: TvValue {
                function: function.cast(),
            },
        }
    }
    fn string(tag: c_int, string: &'static CStr) -> Tv {
        Tv {
            tag,
            value: TvValue {
                string: string.as_ptr(),
            },
        }
    }

    let mut tv = vec![
        function(tag::MESSAGE, message as *const ()),
        val(tag::API_VERSION, abi::API_VERSION),
        val(tag::GNU_LD_VERSION, abi::GNU_LD_VERSION),
        val(tag::LINKER_OUTPUT, host.options.output_kind.raw()),
    ];
    if let Some(name) = host.output_name {
        tv.push(string(tag::OUTPUT_NAME, name));
    }
    tv.extend([
        function(
            tag::REGISTER_CLAIM_FILE_HOOK,
            register_claim_file as *const (),
        ),
        function(
            tag::REGISTER_CLAIM_FILE_HOOK_V2,
            register_claim_file_v2 as *const (),
        ),
        function(
            tag::REGISTER_ALL_SYMBOLS_READ_HOOK,
            register_all_symbols_read as *const (),
        ),
        function(tag::REGISTER_CLEANUP_HOOK, register_cleanup as *const ()),
        function(
            tag::REGISTER_NEW_INPUT_HOOK,
            register_new_input as *const (),
        ),
        function(tag::ADD_SYMBOLS, add_symbols as *const ()),
        function(tag::ADD_SYMBOLS_V2, add_symbols_v2 as *const ()),
        function(tag::GET_INPUT_FILE, get_input_file as *const ()),
        function(tag::GET_VIEW, get_view as *const ()),
        function(tag::RELEASE_INPUT_FILE, release_input_file as *const ()),
        function(tag::GET_SYMBOLS, get_symbols as *const ()),
        function(tag::GET_SYMBOLS_V2, get_symbols_v2 as *const ()),
        function(tag::GET_SYMBOLS_V3, get_symbols_v3 as *const ()),
        function(tag::ADD_INPUT_FILE, add_input_file as *const ()),
        function(tag::ADD_INPUT_LIBRARY, add_input_library as *const ()),
        function(
            tag::SET_EXTRA_LIBRARY_PATH,
            set_extra_library_path as *const (),
        ),
        function(
            tag::GET_INPUT_SECTION_COUNT,
            get_input_section_count as *const (),
        ),
        function(
            tag::GET_INPUT_SECTION_TYPE,
            get_input_section_type as *const (),
        ),
        function(
            tag::GET_INPUT_SECTION_NAME,
            get_input_section_name as *const (),
        ),
        function(
            tag::GET_INPUT_SECTION_CONTENTS,
            get_input_section_contents as *const (),
        ),
        function(
            tag::GET_INPUT_SECTION_ALIGNMENT,
            get_input_section_alignment as *const (),
        ),
        function(
            tag::GET_INPUT_SECTION_SIZE,
            get_input_section_size as *const (),
        ),
        function(tag::UPDATE_SECTION_ORDER, update_section_order as *const ()),
        function(
            tag::ALLOW_SECTION_ORDERING,
            allow_section_ordering as *const (),
        ),
        function(
            tag::ALLOW_UNIQUE_SEGMENT_FOR_SECTIONS,
            allow_unique_segment_for_sections as *const (),
        ),
        function(
            tag::UNIQUE_SEGMENT_FOR_SECTIONS,
            unique_segment_for_sections as *const (),
        ),
        function(tag::GET_WRAP_SYMBOLS, get_wrap_symbols as *const ()),
        function(tag::GET_API_VERSION, get_api_version as *const ()),
    ]);
    tv.extend(options.iter().map(|&option| string(tag::OPTION, option)));
    tv.push(val(tag::NULL, 0));
    tv
}

/// What [`report`] found among the queued messages.
struct Outcome {
    errors: usize,
    fatal: bool,
}

/// Emits the queued plugin messages to `diagnostics`.
fn report(diagnostics: &dyn DiagnosticSink) -> Outcome {
    let (messages, failed) = match lock().as_mut() {
        Some(host) => (std::mem::take(&mut host.messages), host.failed),
        None => (Vec::new(), false),
    };
    let mut outcome = Outcome {
        errors: 0,
        fatal: false,
    };
    for message in messages {
        let severity = match message.level {
            MessageLevel::Info => Severity::Note,
            MessageLevel::Warning => Severity::Warning,
            MessageLevel::Error | MessageLevel::Fatal => {
                outcome.errors += 1;
                Severity::Error
            }
        };
        outcome.fatal |= message.level == MessageLevel::Fatal;
        diagnostics.emit(Diagnostic::new(severity, message.text));
    }
    outcome.fatal |= failed;
    outcome
}

/// Runs the claim handlers on `file`.
pub(super) fn claim(
    file: &InputFile,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Option<(usize, ClaimedFile)>> {
    let name = CString::new(file.path.as_os_str().as_bytes()).map_err(|_| {
        Error::io(
            &file.path,
            io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"),
        )
    })?;
    let (offset, size) = match (i64::try_from(file.offset), i64::try_from(file.size)) {
        (Ok(offset), Ok(size)) if offset.checked_add(size).is_some() => (offset, size),
        _ => {
            return Err(Error::Limit(format!(
                "{}: offset or size too large for the plugin interface",
                file.path.display()
            )));
        }
    };

    let (raw, handlers, index) = {
        let mut guard = lock();
        let host = usable(&mut guard)?;
        match host.phase {
            Phase::Loading | Phase::Claiming => host.phase = Phase::Claiming,
            Phase::AllSymbolsRead | Phase::Finished => {
                return Err(Error::Internal(
                    "files cannot be claimed after all symbols were read".to_owned(),
                ));
            }
        }
        let fd = host
            .open(&file.path)
            .map_err(|error| Error::io(&file.path, error))?
            .file
            .as_raw_fd();
        let index = host.files.len();
        host.files.push(FileRecord {
            handle: file.handle,
            path: file.path.clone(),
            name,
            offset: file.offset,
            size: file.size,
            in_claim: true,
            claimed_by: None,
            pending: Vec::new(),
            symbol_count: 0,
            resolution: Resolved::Pending,
        });
        let raw = RawInputFile {
            name: host.files[index].name.as_ptr(),
            fd,
            offset,
            filesize: size,
            handle: handle_for(index),
        };
        let handlers: Vec<(usize, ClaimHandler)> = host
            .plugins
            .iter()
            .enumerate()
            .filter_map(|(plugin, slot)| match (slot.claim_v2, slot.claim) {
                (Some(handler), _) => Some((plugin, ClaimHandler::V2(handler))),
                (None, Some(handler)) => Some((plugin, ClaimHandler::V1(handler))),
                (None, None) => None,
            })
            .collect();
        (raw, handlers, index)
    };

    let mut claimed_by = None;
    let mut failure = None;
    for (plugin, handler) in handlers {
        set_current(Some(plugin));
        let mut claimed: c_int = 0;
        // SAFETY: `raw` describes an open file whose name string lives in the
        // file record for the whole session; `claimed` is a valid out
        // pointer. The handler was registered by the plugin for exactly this
        // call.
        let status = unsafe {
            match handler {
                ClaimHandler::V1(handler) => handler(&raw, &mut claimed),
                ClaimHandler::V2(handler) => {
                    handler(&raw, &mut claimed, c_int::from(file.known_used))
                }
            }
        };
        set_current(None);
        let mut guard = lock();
        let Some(host) = guard.as_mut() else { break };
        if host.failed {
            break;
        }
        if status != STATUS_OK {
            failure = Some((host.plugins[plugin].path.clone(), status));
            break;
        }
        if claimed != 0 {
            claimed_by = Some(plugin);
            break;
        }
        // Symbols added by a plugin that then declined the file are dropped.
        if let Some(record) = host.files.get_mut(index) {
            record.pending.clear();
        }
    }

    let result = {
        let mut guard = lock();
        let host = active(&mut guard)?;
        let record = &mut host.files[index];
        record.in_claim = false;
        record.claimed_by = claimed_by;
        let symbols = std::mem::take(&mut record.pending);
        record.symbol_count = symbols.len();
        let result = claimed_by.map(|plugin| {
            (
                index,
                ClaimedFile {
                    handle: file.handle,
                    path: file.path.clone(),
                    offset: file.offset,
                    size: file.size,
                    plugin,
                    symbols,
                },
            )
        });
        if claimed_by.is_none() {
            host.release_if_unclaimed(&file.path);
        }
        result
    };

    let outcome = report(diagnostics);
    if outcome.fatal {
        fail();
        return Err(Error::Reported {
            errors: outcome.errors.max(1),
        });
    }
    if let Some((plugin, status)) = failure {
        diagnostics.emit(Diagnostic::error(format!(
            "{}: plugin reported an error claiming {} (status {status})",
            plugin.display(),
            file.path.display()
        )));
        fail();
        return Err(Error::Reported {
            errors: outcome.errors + 1,
        });
    }
    Ok(result)
}

/// Stores resolutions and runs the all-symbols-read handlers.
pub(super) fn all_symbols_read(
    resolutions: Vec<(usize, FileResolution)>,
    diagnostics: &dyn DiagnosticSink,
) -> Result<LtoOutput> {
    let handlers: Vec<(usize, VoidHandlerFn)> = {
        let mut guard = lock();
        let host = usable(&mut guard)?;
        if matches!(host.phase, Phase::AllSymbolsRead | Phase::Finished) {
            return Err(Error::Internal(
                "all_symbols_read may only run once per session".to_owned(),
            ));
        }
        host.phase = Phase::AllSymbolsRead;
        for (index, resolution) in resolutions {
            let Some(record) = host.files.get_mut(index) else {
                return Err(Error::Internal("unknown claimed file".to_owned()));
            };
            record.resolution = match resolution {
                FileResolution::Included(values) => Resolved::Included(values),
                FileResolution::NotIncluded => Resolved::NotIncluded,
            };
        }
        host.plugins
            .iter()
            .enumerate()
            .filter_map(|(plugin, slot)| slot.all_symbols_read.map(|handler| (plugin, handler)))
            .collect()
    };

    let mut failure = None;
    for (plugin, handler) in handlers {
        set_current(Some(plugin));
        // SAFETY: a handler the plugin registered, called once with no
        // arguments as the interface specifies.
        let status = unsafe { handler() };
        set_current(None);
        let mut guard = lock();
        let Some(host) = guard.as_mut() else { break };
        if host.failed {
            break;
        }
        if status != STATUS_OK {
            failure = Some((host.plugins[plugin].path.clone(), status));
            break;
        }
    }

    let mut output = {
        let mut guard = lock();
        let host = active(&mut guard)?;
        LtoOutput {
            files: std::mem::take(&mut host.added_files),
            libraries: std::mem::take(&mut host.added_libraries),
            library_paths: std::mem::take(&mut host.library_paths),
            section_order: host.section_order.take(),
            unique_segments: std::mem::take(&mut host.unique_segments),
            errors: 0,
        }
    };
    let outcome = report(diagnostics);
    if outcome.fatal {
        fail();
        return Err(Error::Reported {
            errors: outcome.errors.max(1),
        });
    }
    if let Some((plugin, status)) = failure {
        diagnostics.emit(Diagnostic::error(format!(
            "{}: plugin reported an error after all symbols were read (status {status})",
            plugin.display()
        )));
        fail();
        return Err(Error::Reported {
            errors: outcome.errors + 1,
        });
    }
    output.errors = outcome.errors;
    Ok(output)
}

/// Runs the new-input handlers for a file added after all symbols were read.
pub(super) fn new_input(
    file: &InputFile,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Vec<UniqueSegment>> {
    let name = CString::new(file.path.as_os_str().as_bytes()).map_err(|_| {
        Error::io(
            &file.path,
            io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"),
        )
    })?;
    let (raw, handlers) = {
        let mut guard = lock();
        let host = usable(&mut guard)?;
        let handlers: Vec<(usize, NewInputFn)> = host
            .plugins
            .iter()
            .enumerate()
            .filter_map(|(plugin, slot)| slot.new_input.map(|handler| (plugin, handler)))
            .collect();
        if handlers.is_empty() {
            return Ok(Vec::new());
        }
        let fd = host
            .open(&file.path)
            .map_err(|error| Error::io(&file.path, error))?
            .file
            .as_raw_fd();
        let index = host.files.len();
        host.files.push(FileRecord {
            handle: file.handle,
            path: file.path.clone(),
            name,
            offset: file.offset,
            size: file.size,
            in_claim: false,
            claimed_by: None,
            pending: Vec::new(),
            symbol_count: 0,
            resolution: Resolved::NotIncluded,
        });
        let raw = host.raw_file(index, fd).ok_or_else(|| {
            Error::Limit(format!(
                "{}: offset or size too large for the plugin interface",
                file.path.display()
            ))
        })?;
        (raw, handlers)
    };

    let mut failure = None;
    for (plugin, handler) in handlers {
        set_current(Some(plugin));
        // SAFETY: as for claim handlers: `raw` names an open file whose
        // strings outlive the call.
        let status = unsafe { handler(&raw) };
        set_current(None);
        let mut guard = lock();
        let Some(host) = guard.as_mut() else { break };
        if host.failed {
            break;
        }
        if status != STATUS_OK {
            failure = Some((host.plugins[plugin].path.clone(), status));
            break;
        }
    }

    let segments = match lock().as_mut() {
        Some(host) => std::mem::take(&mut host.unique_segments),
        None => Vec::new(),
    };
    let outcome = report(diagnostics);
    if outcome.fatal {
        fail();
        return Err(Error::Reported {
            errors: outcome.errors.max(1),
        });
    }
    if let Some((plugin, status)) = failure {
        diagnostics.emit(Diagnostic::error(format!(
            "{}: plugin reported an error for new input {} (status {status})",
            plugin.display(),
            file.path.display()
        )));
        fail();
        return Err(Error::Reported {
            errors: outcome.errors + 1,
        });
    }
    Ok(segments)
}

/// Information about the loaded plugins.
pub(super) fn plugins() -> Vec<PluginInfo> {
    match lock().as_ref() {
        Some(host) => host
            .plugins
            .iter()
            .map(|plugin| PluginInfo {
                path: plugin.path.clone(),
                identifier: plugin.identifier.clone(),
                version: plugin.version.clone(),
                api_level: plugin.api_level,
            })
            .collect(),
        None => Vec::new(),
    }
}

/// Runs the cleanup handlers (unless the session keeps temporary files) and
/// ends the session.
pub(super) fn end(diagnostics: Option<&dyn DiagnosticSink>) -> Result<()> {
    let handlers: Vec<VoidHandlerFn> = {
        let mut guard = lock();
        let Some(host) = guard.as_mut() else {
            return Ok(());
        };
        if host.phase == Phase::Finished {
            Vec::new()
        } else {
            host.phase = Phase::Finished;
            if host.options.save_temps {
                Vec::new()
            } else {
                host.plugins
                    .iter()
                    .filter_map(|slot| slot.cleanup)
                    .collect()
            }
        }
    };
    let mut failures = 0;
    for handler in handlers {
        // SAFETY: a handler the plugin registered, called once with no
        // arguments as the interface specifies.
        let status = unsafe { handler() };
        if status != STATUS_OK {
            failures += 1;
        }
    }
    let outcome = diagnostics.map(|sink| {
        let outcome = report(sink);
        if failures > 0 {
            // GNU ld notes cleanup failures and carries on.
            sink.emit(Diagnostic::warning(format!(
                "{failures} plugin cleanup handler(s) reported an error"
            )));
        }
        outcome
    });
    // Dropping the state closes and unmaps the files the plugins used.
    let host = lock().take();
    drop(host);
    match outcome {
        Some(outcome) if outcome.errors > 0 => Err(Error::Reported {
            errors: outcome.errors,
        }),
        _ => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Callbacks
// ---------------------------------------------------------------------------

/// Runs a callback body, turning a panic into an error status so that no
/// unwinding crosses into plugin code.
fn guard(body: impl FnOnce() -> Status) -> Status {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(STATUS_ERR)
}

/// Runs `body` on the active session's state, or returns an error status if
/// no session is active.
fn with_host(body: impl FnOnce(&mut Host) -> Status) -> Status {
    guard(|| match lock().as_mut() {
        Some(host) => body(host),
        None => STATUS_ERR,
    })
}

/// Copies a NUL-terminated string from the plugin. `None` for null.
///
/// # Safety
///
/// `pointer` must be null or point to a NUL-terminated string.
unsafe fn plugin_string(pointer: *const c_char) -> Option<Vec<u8>> {
    if pointer.is_null() {
        return None;
    }
    // SAFETY: non-null, and NUL-terminated by the caller's contract.
    Some(unsafe { CStr::from_ptr(pointer) }.to_bytes().to_vec())
}

/// Stores a registered handler for the plugin being called.
fn register(apply: impl FnOnce(&mut Plugin)) -> Status {
    with_host(
        |host| match host.current.and_then(|index| host.plugins.get_mut(index)) {
            Some(plugin) => {
                apply(plugin);
                STATUS_OK
            }
            None => STATUS_ERR,
        },
    )
}

extern "C" fn register_claim_file(handler: Option<ClaimFileFn>) -> Status {
    match handler {
        Some(handler) => register(|plugin| plugin.claim = Some(handler)),
        None => STATUS_ERR,
    }
}

extern "C" fn register_claim_file_v2(handler: Option<ClaimFileV2Fn>) -> Status {
    match handler {
        Some(handler) => register(|plugin| plugin.claim_v2 = Some(handler)),
        None => STATUS_ERR,
    }
}

extern "C" fn register_all_symbols_read(handler: Option<VoidHandlerFn>) -> Status {
    match handler {
        Some(handler) => register(|plugin| plugin.all_symbols_read = Some(handler)),
        None => STATUS_ERR,
    }
}

extern "C" fn register_cleanup(handler: Option<VoidHandlerFn>) -> Status {
    match handler {
        Some(handler) => register(|plugin| plugin.cleanup = Some(handler)),
        None => STATUS_ERR,
    }
}

extern "C" fn register_new_input(handler: Option<NewInputFn>) -> Status {
    match handler {
        Some(handler) => register(|plugin| plugin.new_input = Some(handler)),
        None => STATUS_ERR,
    }
}

/// Decodes one symbol record. `None` if a field is invalid.
///
/// # Safety
///
/// The record's string pointers must be null or NUL-terminated strings.
unsafe fn decode_symbol(raw: &RawSymbol, second_version: bool) -> Option<ClaimedSymbol> {
    // SAFETY: forwarded from the caller.
    let name = unsafe { plugin_string(raw.name) }?;
    // SAFETY: as above.
    let version = unsafe { plugin_string(raw.version) };
    // SAFETY: as above.
    let comdat_key = unsafe { plugin_string(raw.comdat_key) };
    let (symbol_type, section_kind) = if second_version {
        let symbol_type = match raw.symbol_type {
            0 => SymbolType::Unknown,
            1 => SymbolType::Function,
            2 => SymbolType::Variable,
            _ => return None,
        };
        let section_kind = match raw.section_kind {
            0 => SectionKind::Default,
            1 => SectionKind::Bss,
            _ => return None,
        };
        (symbol_type, section_kind)
    } else {
        // The first version has no such fields: those bytes were the upper
        // part of an `int` and carry nothing.
        (SymbolType::Unknown, SectionKind::Default)
    };
    Some(ClaimedSymbol {
        name,
        version,
        kind: SymbolKind::from_raw(raw.def)?,
        visibility: Visibility::from_raw(raw.visibility)?,
        size: raw.size,
        comdat_key,
        symbol_type,
        section_kind,
    })
}

fn add_symbols_impl(
    handle: *mut c_void,
    count: c_int,
    symbols: *const RawSymbol,
    second_version: bool,
) -> Status {
    guard(|| {
        let Ok(count) = usize::try_from(count) else {
            return STATUS_ERR;
        };
        if count > 0 && symbols.is_null() {
            return STATUS_ERR;
        }
        let records: &[RawSymbol] = if count == 0 {
            &[]
        } else {
            // SAFETY: the plugin passes an array of `count` initialized symbol
            // records (checked non-null above); plugin arrays of this C type
            // are properly aligned.
            unsafe { std::slice::from_raw_parts(symbols, count) }
        };
        let mut decoded = Vec::with_capacity(count);
        for record in records {
            // SAFETY: the interface requires the strings to be null or
            // NUL-terminated.
            match unsafe { decode_symbol(record, second_version) } {
                Some(symbol) => decoded.push(symbol),
                None => return STATUS_ERR,
            }
        }
        let mut state = lock();
        let Some(host) = state.as_mut() else {
            return STATUS_ERR;
        };
        let Some(index) = handle.addr().checked_sub(1) else {
            return STATUS_BAD_HANDLE;
        };
        match host.files.get_mut(index) {
            Some(record) if record.in_claim => {
                record.pending.append(&mut decoded);
                STATUS_OK
            }
            // Symbols may only be added while the file is being claimed.
            Some(_) => STATUS_ERR,
            None => STATUS_BAD_HANDLE,
        }
    })
}

extern "C" fn add_symbols(handle: *mut c_void, count: c_int, symbols: *const RawSymbol) -> Status {
    add_symbols_impl(handle, count, symbols, false)
}

extern "C" fn add_symbols_v2(
    handle: *mut c_void,
    count: c_int,
    symbols: *const RawSymbol,
) -> Status {
    add_symbols_impl(handle, count, symbols, true)
}

extern "C" fn get_input_file(handle: *const c_void, file: *mut RawInputFile) -> Status {
    if file.is_null() {
        return STATUS_ERR;
    }
    with_host(|host| {
        let Some((index, record)) = host.file(handle) else {
            return STATUS_BAD_HANDLE;
        };
        let path = record.path.clone();
        let fd = match host.open(&path) {
            Ok(open) => open.file.as_raw_fd(),
            Err(_) => return STATUS_ERR,
        };
        let Some(raw) = host.raw_file(index, fd) else {
            return STATUS_ERR;
        };
        // SAFETY: `file` is the plugin's non-null out pointer to a record of
        // this type.
        unsafe { file.write(raw) };
        STATUS_OK
    })
}

extern "C" fn release_input_file(handle: *const c_void) -> Status {
    // Descriptors stay open until the session ends, so there is nothing to
    // release; the handle is still checked.
    with_host(|host| {
        if host.file(handle).is_some() {
            STATUS_OK
        } else {
            STATUS_BAD_HANDLE
        }
    })
}

extern "C" fn get_view(handle: *const c_void, view: *mut *const c_void) -> Status {
    if view.is_null() {
        return STATUS_ERR;
    }
    with_host(|host| {
        let Some((index, _)) = host.file(handle) else {
            return STATUS_BAD_HANDLE;
        };
        let Some(bytes) = host.view(index) else {
            return STATUS_ERR;
        };
        let address: *const c_void = if bytes.is_empty() {
            NonNull::<c_void>::dangling().as_ptr()
        } else {
            bytes.as_ptr().cast()
        };
        // SAFETY: `view` is the plugin's non-null out pointer. The bytes stay
        // mapped until the session ends.
        unsafe { view.write(address) };
        STATUS_OK
    })
}

fn get_symbols_impl(
    handle: *const c_void,
    count: c_int,
    symbols: *mut RawSymbol,
    version: u8,
) -> Status {
    guard(|| {
        let Ok(count) = usize::try_from(count) else {
            return STATUS_ERR;
        };
        if count > 0 && symbols.is_null() {
            return STATUS_ERR;
        }
        let (values, status) = {
            let state = lock();
            let Some(host) = state.as_ref() else {
                return STATUS_ERR;
            };
            let Some((_, record)) = host.file(handle) else {
                return STATUS_BAD_HANDLE;
            };
            if record.claimed_by.is_none() {
                return STATUS_BAD_HANDLE;
            }
            match &record.resolution {
                // Resolution is not known before all symbols are read.
                Resolved::Pending => return STATUS_ERR,
                Resolved::NotIncluded => {
                    let preempted = SymbolResolution::PreemptedRegular.raw();
                    let values = vec![preempted; count.min(record.symbol_count)];
                    let status = if version >= 3 {
                        STATUS_NO_SYMS
                    } else {
                        STATUS_OK
                    };
                    (values, status)
                }
                Resolved::Included(resolutions) => {
                    if count > resolutions.len() {
                        return STATUS_NO_SYMS;
                    }
                    let values = resolutions[..count]
                        .iter()
                        .map(|&resolution| match resolution {
                            SymbolResolution::PrevailingDefIronlyExp if version == 1 => {
                                SymbolResolution::PrevailingDef.raw()
                            }
                            resolution => resolution.raw(),
                        })
                        .collect::<Vec<_>>();
                    (values, STATUS_OK)
                }
            }
        };
        for (i, value) in values.into_iter().enumerate() {
            // SAFETY: `symbols` is the plugin's array of at least `count`
            // records (checked non-null), and `i < count`. Only the
            // resolution field is written, through a raw pointer, so no
            // reference to plugin memory is created.
            unsafe { ptr::addr_of_mut!((*symbols.add(i)).resolution).write(value) };
        }
        status
    })
}

extern "C" fn get_symbols(handle: *const c_void, count: c_int, symbols: *mut RawSymbol) -> Status {
    get_symbols_impl(handle, count, symbols, 1)
}

extern "C" fn get_symbols_v2(
    handle: *const c_void,
    count: c_int,
    symbols: *mut RawSymbol,
) -> Status {
    get_symbols_impl(handle, count, symbols, 2)
}

extern "C" fn get_symbols_v3(
    handle: *const c_void,
    count: c_int,
    symbols: *mut RawSymbol,
) -> Status {
    get_symbols_impl(handle, count, symbols, 3)
}

/// Common body of the three callbacks that take one path-like string.
fn add_string(pointer: *const c_char, store: impl FnOnce(&mut Host, OsString)) -> Status {
    // SAFETY: the interface passes null or a NUL-terminated string.
    let Some(bytes) = (unsafe { plugin_string(pointer) }) else {
        return STATUS_ERR;
    };
    with_host(|host| {
        if host.phase == Phase::Finished {
            return STATUS_ERR;
        }
        store(host, OsString::from_vec(bytes));
        STATUS_OK
    })
}

extern "C" fn add_input_file(path: *const c_char) -> Status {
    add_string(path, |host, path| host.added_files.push(path.into()))
}

extern "C" fn add_input_library(name: *const c_char) -> Status {
    add_string(name, |host, name| host.added_libraries.push(name))
}

extern "C" fn set_extra_library_path(path: *const c_char) -> Status {
    add_string(path, |host, path| host.library_paths.push(path.into()))
}

/// The `message` callback.
///
/// The interface declares it C-variadic, which stable Rust cannot define. On
/// the calling conventions below, a variadic caller passes the leading
/// integer-class arguments in the same registers as for a fixed-parameter
/// function, so declaring those registers as extra `usize` parameters reads
/// exactly what the caller passed. Registers the caller did not set hold
/// unspecified (but initialized, from Rust's view: any bit pattern is a valid
/// `usize`) values, and the format string decides which are read, exactly as
/// for `printf`. Only register-passed arguments are declared, so no stack
/// slot the caller did not write is ever read. Floating-point arguments,
/// which travel in other registers, are not supported (see `format.rs`).
///
/// - x86-64 System V: the format is in the second register, leaving four
///   (`rdx`, `rcx`, `r8`, `r9`).
/// - AArch64 (except Apple, which passes variadic arguments on the stack) and
///   RISC-V: six (`x2`..`x7`, `a2`..`a7`).
///
/// Elsewhere the format string is reported verbatim.
#[cfg(target_arch = "x86_64")]
extern "C" fn message(
    level: c_int,
    format: *const c_char,
    a0: usize,
    a1: usize,
    a2: usize,
    a3: usize,
) -> Status {
    record_message(level, format, &[a0, a1, a2, a3])
}

/// The `message` callback; see the x86-64 version.
#[cfg(all(
    any(target_arch = "aarch64", target_arch = "riscv64"),
    not(target_vendor = "apple")
))]
extern "C" fn message(
    level: c_int,
    format: *const c_char,
    a0: usize,
    a1: usize,
    a2: usize,
    a3: usize,
    a4: usize,
    a5: usize,
) -> Status {
    record_message(level, format, &[a0, a1, a2, a3, a4, a5])
}

/// The `message` callback; see the x86-64 version.
#[cfg(not(any(
    target_arch = "x86_64",
    all(
        any(target_arch = "aarch64", target_arch = "riscv64"),
        not(target_vendor = "apple")
    )
)))]
extern "C" fn message(level: c_int, format: *const c_char) -> Status {
    record_message(level, format, &[])
}

/// `printf` arguments taken from register values.
struct Words<'a> {
    words: std::slice::Iter<'a, usize>,
}

impl Arguments for Words<'_> {
    fn next_word(&mut self) -> Option<usize> {
        self.words.next().copied()
    }

    fn string(&mut self, address: usize, limit: usize) -> Vec<u8> {
        let start = ptr::with_exposed_provenance::<u8>(address);
        let mut out = Vec::new();
        for i in 0..limit {
            // SAFETY: the plugin's format string says this argument is a
            // NUL-terminated string (it is non-null, checked by the
            // formatter); bytes are read one at a time up to the terminator
            // or `limit`, as `printf` would read them.
            let byte = unsafe { start.add(i).read() };
            if byte == 0 {
                break;
            }
            out.push(byte);
        }
        out
    }
}

fn record_message(level: c_int, format: *const c_char, words: &[usize]) -> Status {
    guard(|| {
        // SAFETY: the interface passes null or a NUL-terminated format.
        let Some(format) = (unsafe { plugin_string(format) }) else {
            return STATUS_ERR;
        };
        let text = format_message(
            &format,
            &mut Words {
                words: words.iter(),
            },
        );
        let message = PluginMessage {
            level: MessageLevel::from_raw(level),
            text,
        };
        let hook = {
            let mut state = lock();
            match state.as_mut() {
                Some(host) => {
                    let fatal = message.level == MessageLevel::Fatal;
                    host.failed |= fatal;
                    let hook = host.options.fatal_hook.filter(|_| fatal);
                    host.messages.push(message.clone());
                    hook
                }
                None => {
                    // A plugin thread outliving its session: there is no sink.
                    eprintln!("{}: plugin: {message}", crate::PROGRAM_NAME);
                    None
                }
            }
        };
        if let Some(hook) = hook {
            hook(&message);
        }
        STATUS_OK
    })
}

/// Section information for the `get_input_section_*` callbacks.
struct SectionInfo<'a> {
    count: u32,
    header: Option<(crate::elf::read::SectionHeader, &'a [u8], &'a [u8])>,
}

/// Parses `bytes` as ELF and looks up section `index` (if given): its header,
/// name and contents.
fn elf_section(bytes: &[u8], index: Option<u32>) -> Option<SectionInfo<'_>> {
    fn inner<F: ElfFormat>(bytes: &[u8], index: Option<u32>) -> Option<SectionInfo<'_>> {
        let elf = ElfFile::<F>::parse(bytes, Source::new(Path::new(""))).ok()?;
        let count = u32::try_from(elf.section_count()).ok()?;
        let header = match index {
            Some(index) => {
                let header = elf.section_header(index).ok()?;
                let name = elf.section_name(&header).ok()?;
                let data = elf.section_data(&header).ok()?;
                Some((header, name, data))
            }
            None => None,
        };
        Some(SectionInfo { count, header })
    }
    match ElfKind::identify(bytes)? {
        ElfKind::Elf64Le => inner::<Elf64Le>(bytes, index),
        ElfKind::Elf64Be => inner::<Elf64Be>(bytes, index),
        ElfKind::Elf32Le => inner::<Elf32Le>(bytes, index),
        ElfKind::Elf32Be => inner::<Elf32Be>(bytes, index),
    }
}

/// Runs `body` with section `section.shndx` of the file `section.handle`.
fn with_section(
    section: RawSection,
    body: impl FnOnce(crate::elf::read::SectionHeader, &[u8], &[u8]) -> Status,
) -> Status {
    with_host(|host| {
        let Some((index, _)) = host.file(section.handle) else {
            return STATUS_BAD_HANDLE;
        };
        let Some(bytes) = host.view(index) else {
            return STATUS_ERR;
        };
        match elf_section(bytes, Some(section.shndx)).and_then(|info| info.header) {
            Some((header, name, data)) => body(header, name, data),
            None => STATUS_ERR,
        }
    })
}

extern "C" fn get_input_section_count(handle: *const c_void, count: *mut c_uint) -> Status {
    if count.is_null() {
        return STATUS_ERR;
    }
    with_host(|host| {
        let Some((index, _)) = host.file(handle) else {
            return STATUS_BAD_HANDLE;
        };
        let Some(info) = host.view(index).and_then(|bytes| elf_section(bytes, None)) else {
            return STATUS_ERR;
        };
        // SAFETY: non-null out pointer from the plugin.
        unsafe { count.write(info.count) };
        STATUS_OK
    })
}

extern "C" fn get_input_section_type(section: RawSection, kind: *mut c_uint) -> Status {
    if kind.is_null() {
        return STATUS_ERR;
    }
    with_section(section, |header, _, _| {
        // SAFETY: non-null out pointer from the plugin.
        unsafe { kind.write(header.sh_type) };
        STATUS_OK
    })
}

unsafe extern "C" {
    fn malloc(size: usize) -> *mut c_void;
}

extern "C" fn get_input_section_name(section: RawSection, name: *mut *mut c_char) -> Status {
    if name.is_null() {
        return STATUS_ERR;
    }
    with_section(section, |_, section_name, _| {
        let Some(size) = section_name.len().checked_add(1) else {
            return STATUS_ERR;
        };
        // SAFETY: plain allocation; the plugin frees it with `free`, as the
        // interface specifies.
        let buffer = unsafe { malloc(size) }.cast::<u8>();
        if buffer.is_null() {
            return STATUS_ERR;
        }
        // SAFETY: `buffer` holds `size` bytes, and `section_name` (from the
        // mapped file) cannot overlap a fresh allocation.
        unsafe {
            ptr::copy_nonoverlapping(section_name.as_ptr(), buffer, section_name.len());
            buffer.add(section_name.len()).write(0);
            name.write(buffer.cast());
        }
        STATUS_OK
    })
}

extern "C" fn get_input_section_contents(
    section: RawSection,
    contents: *mut *const u8,
    len: *mut usize,
) -> Status {
    if contents.is_null() || len.is_null() {
        return STATUS_ERR;
    }
    with_section(section, |_, _, data| {
        // SAFETY: non-null out pointers from the plugin; `data` stays mapped
        // until the session ends.
        unsafe {
            contents.write(data.as_ptr());
            len.write(data.len());
        }
        STATUS_OK
    })
}

extern "C" fn get_input_section_alignment(section: RawSection, alignment: *mut c_uint) -> Status {
    if alignment.is_null() {
        return STATUS_ERR;
    }
    with_section(section, |header, _, _| {
        let Ok(value) = c_uint::try_from(header.sh_addralign) else {
            return STATUS_ERR;
        };
        // SAFETY: non-null out pointer from the plugin.
        unsafe { alignment.write(value) };
        STATUS_OK
    })
}

extern "C" fn get_input_section_size(section: RawSection, size: *mut u64) -> Status {
    if size.is_null() {
        return STATUS_ERR;
    }
    with_section(section, |header, _, _| {
        // SAFETY: non-null out pointer from the plugin.
        unsafe { size.write(header.sh_size) };
        STATUS_OK
    })
}

/// Converts a plugin's section array.
///
/// # Safety
///
/// `list` must be null or point to `count` section records.
unsafe fn section_list(
    host: &Host,
    list: *const RawSection,
    count: c_uint,
) -> std::result::Result<Vec<SectionRef>, Status> {
    let Ok(count) = usize::try_from(count) else {
        return Err(STATUS_ERR);
    };
    if count == 0 {
        return Ok(Vec::new());
    }
    if list.is_null() {
        return Err(STATUS_ERR);
    }
    // SAFETY: forwarded from the caller; non-null checked above.
    let raw = unsafe { std::slice::from_raw_parts(list, count) };
    raw.iter()
        .map(|section| host.section_ref(section).ok_or(STATUS_BAD_HANDLE))
        .collect()
}

extern "C" fn allow_section_ordering() -> Status {
    with_host(|host| {
        host.section_ordering_allowed = true;
        STATUS_OK
    })
}

extern "C" fn update_section_order(list: *const RawSection, count: c_uint) -> Status {
    with_host(|host| {
        if !host.section_ordering_allowed {
            return STATUS_ERR;
        }
        // SAFETY: the plugin passes `count` records at `list`.
        match unsafe { section_list(host, list, count) } {
            Ok(order) => {
                host.section_order = Some(order);
                STATUS_OK
            }
            Err(status) => status,
        }
    })
}

extern "C" fn allow_unique_segment_for_sections() -> Status {
    with_host(|host| {
        host.unique_segment_allowed = true;
        STATUS_OK
    })
}

extern "C" fn unique_segment_for_sections(
    name: *const c_char,
    flags: u64,
    alignment: u64,
    list: *const RawSection,
    count: c_uint,
) -> Status {
    // SAFETY: the interface passes null or a NUL-terminated string.
    let Some(name) = (unsafe { plugin_string(name) }) else {
        return STATUS_ERR;
    };
    with_host(|host| {
        if !host.unique_segment_allowed {
            return STATUS_ERR;
        }
        // SAFETY: the plugin passes `count` records at `list`.
        match unsafe { section_list(host, list, count) } {
            Ok(sections) => {
                host.unique_segments.push(UniqueSegment {
                    name,
                    flags,
                    alignment,
                    sections,
                });
                STATUS_OK
            }
            Err(status) => status,
        }
    })
}

extern "C" fn get_wrap_symbols(count: *mut u64, list: *mut *const *const c_char) -> Status {
    if count.is_null() || list.is_null() {
        return STATUS_ERR;
    }
    with_host(|host| {
        let pointers = &host.wrap.pointers;
        // SAFETY: non-null out pointers from the plugin. The array and its
        // strings live in the session state until the session ends.
        unsafe {
            count.write(pointers.len() as u64);
            list.write(pointers.as_ptr());
        }
        STATUS_OK
    })
}

extern "C" fn get_api_version(
    plugin_identifier: *const c_char,
    plugin_version: *const c_char,
    minimal: c_int,
    maximal: c_int,
    linker_identifier: *mut *const c_char,
    linker_version: *mut *const c_char,
) -> c_int {
    // SAFETY: the interface passes null or NUL-terminated strings.
    let identifier = unsafe { plugin_string(plugin_identifier) };
    // SAFETY: as above.
    let version = unsafe { plugin_string(plugin_version) };
    let selected = if minimal <= maximal && minimal <= abi::LINKER_API_V1 && maximal >= 0 {
        maximal.min(abi::LINKER_API_V1)
    } else {
        -1
    };
    let recorded = with_host(|host| {
        if selected < 0 {
            host.failed = true;
            host.messages.push(PluginMessage {
                level: MessageLevel::Fatal,
                text: format!(
                    "plugin requires linker API level {minimal} to {maximal}; qld supports 0 to 1"
                ),
            });
        }
        if let Some(plugin) = host.current.and_then(|index| host.plugins.get_mut(index)) {
            plugin.identifier = identifier.map(|s| String::from_utf8_lossy(&s).into_owned());
            plugin.version = version.map(|s| String::from_utf8_lossy(&s).into_owned());
            plugin.api_level = (selected >= 0).then_some(selected);
        }
        STATUS_OK
    });
    if recorded != STATUS_OK {
        return -1;
    }
    if !linker_identifier.is_null() {
        // SAFETY: non-null out pointer; the string is static.
        unsafe { linker_identifier.write(LINKER_IDENTIFIER.as_ptr()) };
    }
    if !linker_version.is_null() {
        // SAFETY: non-null out pointer; the string is static.
        unsafe { linker_version.write(LINKER_VERSION.as_ptr()) };
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linker_version_is_the_package_version() {
        assert_eq!(
            LINKER_VERSION.to_str().ok(),
            Some(env!("CARGO_PKG_VERSION"))
        );
    }

    #[test]
    fn handles_are_never_null() {
        assert!(!handle_for(0).is_null());
        assert_eq!(handle_for(41).addr(), 42);
    }
}
