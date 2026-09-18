//! The libLTO C API (`llvm-c/lto.h`): the interface Apple's ld64 uses for
//! LTO, and which [`crate::macho::lto`] uses for Mach-O links.
//!
//! libLTO ships with Xcode (`libLTO.dylib`) and with every LLVM build
//! (`libLTO.so` elsewhere). This module finds it ([`find`]), loads it with
//! `dlopen` through the private `dl` module, declares the functions it uses
//! by hand (as the GNU plugin host does, so there is no `-sys` crate), and
//! wraps them in a safe interface:
//!
//! - [`LibLto::load`] loads a library once per process and resolves its
//!   functions; the ThinLTO functions are optional.
//! - [`LibLto::lock`] returns a [`Session`], which holds the process-wide
//!   libLTO lock: reading modules ([`Session::read_module`]) and code
//!   generation ([`Session::compile_full`], [`Session::compile_thin`]).
//!
//! # Safety model
//!
//! - Function pointers come from `dlsym` on a library that is never closed
//!   (see `dl`), so they stay valid for the life of the process. Each is
//!   declared with the C signature of `llvm-c/lto.h` (API version 30);
//!   functions added after API version 11 are looked up as optional.
//! - libLTO keeps process-wide state (the last error message, LLVM's
//!   command-line options, the global context), so every call is made with
//!   one lock held: the [`Session`] guard.
//! - Module and code generator handles are owned by guards that dispose of
//!   them on every path. Buffers handed to libLTO are borrowed for at least
//!   as long as the handles that reference them.
//! - Strings libLTO returns are copied at once; object buffers returned by
//!   code generation are copied before the next call on the same handle.
//! - The diagnostic callback only locks a mutex and pushes a message; it
//!   cannot unwind into C (a panic in an `extern "C"` function aborts).

#![allow(unsafe_code)] // FFI: libLTO is a C library loaded at run time.

use std::ffi::{CStr, CString, c_char, c_int, c_uint, c_void};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::dl::Library;
use crate::diag::Severity;
use crate::error::{Error, Result};

type ModuleHandle = *mut c_void;
type CodeGenHandle = *mut c_void;
type ThinHandle = *mut c_void;
type DiagnosticHandler = unsafe extern "C" fn(c_int, *const c_char, *mut c_void);

/// `LTOObjectBuffer`.
#[repr(C)]
#[derive(Clone, Copy)]
struct ObjectBuffer {
    buffer: *const c_char,
    size: usize,
}

/// `LTO_CODEGEN_PIC_MODEL_DYNAMIC`: position-independent code, which every
/// Mach-O output qld writes uses.
const PIC_MODEL_DYNAMIC: c_int = 1;
/// `LTO_DEBUG_MODEL_DWARF`.
const DEBUG_MODEL_DWARF: c_int = 1;
/// `LTO_DS_ERROR`, `LTO_DS_WARNING`, `LTO_DS_NOTE`, `LTO_DS_REMARK`.
const DS_ERROR: c_int = 0;
const DS_WARNING: c_int = 1;
const DS_NOTE: c_int = 2;

/// The functions of the full-LTO interface qld uses.
struct Api {
    get_version: unsafe extern "C" fn() -> *const c_char,
    get_error_message: unsafe extern "C" fn() -> *const c_char,
    module_create_in_local_context:
        unsafe extern "C" fn(*const c_void, usize, *const c_char) -> ModuleHandle,
    module_create_in_codegen_context:
        unsafe extern "C" fn(*const c_void, usize, *const c_char, CodeGenHandle) -> ModuleHandle,
    module_dispose: unsafe extern "C" fn(ModuleHandle),
    module_get_target_triple: unsafe extern "C" fn(ModuleHandle) -> *const c_char,
    module_get_num_symbols: unsafe extern "C" fn(ModuleHandle) -> c_uint,
    module_get_symbol_name: unsafe extern "C" fn(ModuleHandle, c_uint) -> *const c_char,
    module_get_symbol_attribute: unsafe extern "C" fn(ModuleHandle, c_uint) -> c_uint,
    module_get_num_asm_undef_symbols: Option<unsafe extern "C" fn(ModuleHandle) -> c_uint>,
    module_get_asm_undef_symbol_name:
        Option<unsafe extern "C" fn(ModuleHandle, c_uint) -> *const c_char>,
    module_get_linkeropts: Option<unsafe extern "C" fn(ModuleHandle) -> *const c_char>,
    module_get_macho_cputype:
        Option<unsafe extern "C" fn(ModuleHandle, *mut c_uint, *mut c_uint) -> bool>,
    module_is_thinlto: Option<unsafe extern "C" fn(ModuleHandle) -> bool>,
    module_has_objc_category: Option<unsafe extern "C" fn(*const c_void, usize) -> bool>,
    codegen_create_in_local_context: unsafe extern "C" fn() -> CodeGenHandle,
    codegen_dispose: unsafe extern "C" fn(CodeGenHandle),
    codegen_set_diagnostic_handler:
        unsafe extern "C" fn(CodeGenHandle, DiagnosticHandler, *mut c_void),
    codegen_add_module: unsafe extern "C" fn(CodeGenHandle, ModuleHandle) -> bool,
    codegen_set_debug_model: unsafe extern "C" fn(CodeGenHandle, c_int) -> bool,
    codegen_set_pic_model: unsafe extern "C" fn(CodeGenHandle, c_int) -> bool,
    codegen_set_cpu: unsafe extern "C" fn(CodeGenHandle, *const c_char),
    codegen_add_must_preserve_symbol: unsafe extern "C" fn(CodeGenHandle, *const c_char),
    codegen_compile: unsafe extern "C" fn(CodeGenHandle, *mut usize) -> *const c_void,
    codegen_compile_optimized:
        Option<unsafe extern "C" fn(CodeGenHandle, *mut usize) -> *const c_void>,
    codegen_debug_options: unsafe extern "C" fn(CodeGenHandle, *const c_char),
    codegen_set_should_internalize: Option<unsafe extern "C" fn(CodeGenHandle, bool)>,
    thin: Option<ThinApi>,
}

/// The ThinLTO functions (`thinlto_*`, API version 18 and later).
struct ThinApi {
    create_codegen: unsafe extern "C" fn() -> ThinHandle,
    codegen_dispose: unsafe extern "C" fn(ThinHandle),
    codegen_add_module: unsafe extern "C" fn(ThinHandle, *const c_char, *const c_char, c_int),
    codegen_process: unsafe extern "C" fn(ThinHandle),
    module_get_num_objects: unsafe extern "C" fn(ThinHandle) -> c_uint,
    module_get_object: unsafe extern "C" fn(ThinHandle, c_uint) -> ObjectBuffer,
    module_get_num_object_files: Option<unsafe extern "C" fn(ThinHandle) -> c_uint>,
    module_get_object_file: Option<unsafe extern "C" fn(ThinHandle, c_uint) -> *const c_char>,
    set_generated_objects_dir: Option<unsafe extern "C" fn(ThinHandle, *const c_char)>,
    codegen_set_pic_model: unsafe extern "C" fn(ThinHandle, c_int) -> bool,
    codegen_set_cpu: unsafe extern "C" fn(ThinHandle, *const c_char),
    codegen_set_codegen_only: Option<unsafe extern "C" fn(ThinHandle, bool)>,
    debug_options: unsafe extern "C" fn(*const *const c_char, c_int),
    add_must_preserve_symbol: unsafe extern "C" fn(ThinHandle, *const c_char, c_int),
    add_cross_referenced_symbol: unsafe extern "C" fn(ThinHandle, *const c_char, c_int),
    set_cache_dir: unsafe extern "C" fn(ThinHandle, *const c_char),
    set_cache_pruning_interval: unsafe extern "C" fn(ThinHandle, c_int),
    set_cache_entry_expiration: unsafe extern "C" fn(ThinHandle, c_uint),
    set_final_cache_size_relative_to_available_space: unsafe extern "C" fn(ThinHandle, c_uint),
}

/// Looks up `name` in `library` as a function of type `F`.
///
/// # Safety
///
/// `F` must be an `unsafe extern "C" fn` type matching the C declaration of
/// `name`.
unsafe fn function<F: Copy>(library: &Library, name: &CStr) -> Option<F> {
    let address = library.symbol(name).ok()?;
    if size_of::<F>() != size_of::<*mut c_void>() {
        return None;
    }
    // SAFETY: `address` is the non-null address of `name`, and the caller
    // guarantees `F` is a function pointer type with its signature. Function
    // and data pointers have the same size and representation on every Unix
    // host (checked above).
    Some(unsafe { std::mem::transmute_copy::<*mut c_void, F>(&address) })
}

/// A required function: a missing one means the library is not libLTO.
///
/// # Safety
///
/// As for [`function`].
unsafe fn required<F: Copy>(library: &Library, path: &Path, name: &CStr) -> Result<F> {
    // SAFETY: forwarded from the caller.
    unsafe { function(library, name) }.ok_or_else(|| {
        Error::Plugin(format!(
            "{}: not a usable libLTO (no `{}`)",
            path.display(),
            name.to_string_lossy()
        ))
    })
}

impl Api {
    fn load(library: &Library, path: &Path) -> Result<Self> {
        // Every declaration below repeats the C signature of `llvm-c/lto.h`.
        // SAFETY (for each lookup): the type matches the header.
        unsafe {
            let thin = match function(library, c"thinlto_create_codegen") {
                Some(create_codegen) => Some(ThinApi {
                    create_codegen,
                    codegen_dispose: required(library, path, c"thinlto_codegen_dispose")?,
                    codegen_add_module: required(library, path, c"thinlto_codegen_add_module")?,
                    codegen_process: required(library, path, c"thinlto_codegen_process")?,
                    module_get_num_objects: required(
                        library,
                        path,
                        c"thinlto_module_get_num_objects",
                    )?,
                    module_get_object: required(library, path, c"thinlto_module_get_object")?,
                    module_get_num_object_files: function(
                        library,
                        c"thinlto_module_get_num_object_files",
                    ),
                    module_get_object_file: function(library, c"thinlto_module_get_object_file"),
                    set_generated_objects_dir: function(
                        library,
                        c"thinlto_set_generated_objects_dir",
                    ),
                    codegen_set_pic_model: required(
                        library,
                        path,
                        c"thinlto_codegen_set_pic_model",
                    )?,
                    codegen_set_cpu: required(library, path, c"thinlto_codegen_set_cpu")?,
                    codegen_set_codegen_only: function(
                        library,
                        c"thinlto_codegen_set_codegen_only",
                    ),
                    debug_options: required(library, path, c"thinlto_debug_options")?,
                    add_must_preserve_symbol: required(
                        library,
                        path,
                        c"thinlto_codegen_add_must_preserve_symbol",
                    )?,
                    add_cross_referenced_symbol: required(
                        library,
                        path,
                        c"thinlto_codegen_add_cross_referenced_symbol",
                    )?,
                    set_cache_dir: required(library, path, c"thinlto_codegen_set_cache_dir")?,
                    set_cache_pruning_interval: required(
                        library,
                        path,
                        c"thinlto_codegen_set_cache_pruning_interval",
                    )?,
                    set_cache_entry_expiration: required(
                        library,
                        path,
                        c"thinlto_codegen_set_cache_entry_expiration",
                    )?,
                    set_final_cache_size_relative_to_available_space: required(
                        library,
                        path,
                        c"thinlto_codegen_set_final_cache_size_relative_to_available_space",
                    )?,
                }),
                None => None,
            };
            Ok(Self {
                get_version: required(library, path, c"lto_get_version")?,
                get_error_message: required(library, path, c"lto_get_error_message")?,
                module_create_in_local_context: required(
                    library,
                    path,
                    c"lto_module_create_in_local_context",
                )?,
                module_create_in_codegen_context: required(
                    library,
                    path,
                    c"lto_module_create_in_codegen_context",
                )?,
                module_dispose: required(library, path, c"lto_module_dispose")?,
                module_get_target_triple: required(library, path, c"lto_module_get_target_triple")?,
                module_get_num_symbols: required(library, path, c"lto_module_get_num_symbols")?,
                module_get_symbol_name: required(library, path, c"lto_module_get_symbol_name")?,
                module_get_symbol_attribute: required(
                    library,
                    path,
                    c"lto_module_get_symbol_attribute",
                )?,
                module_get_num_asm_undef_symbols: function(
                    library,
                    c"lto_module_get_num_asm_undef_symbols",
                ),
                module_get_asm_undef_symbol_name: function(
                    library,
                    c"lto_module_get_asm_undef_symbol_name",
                ),
                module_get_linkeropts: function(library, c"lto_module_get_linkeropts"),
                module_get_macho_cputype: function(library, c"lto_module_get_macho_cputype"),
                module_is_thinlto: function(library, c"lto_module_is_thinlto"),
                module_has_objc_category: function(library, c"lto_module_has_objc_category"),
                codegen_create_in_local_context: required(
                    library,
                    path,
                    c"lto_codegen_create_in_local_context",
                )?,
                codegen_dispose: required(library, path, c"lto_codegen_dispose")?,
                codegen_set_diagnostic_handler: required(
                    library,
                    path,
                    c"lto_codegen_set_diagnostic_handler",
                )?,
                codegen_add_module: required(library, path, c"lto_codegen_add_module")?,
                codegen_set_debug_model: required(library, path, c"lto_codegen_set_debug_model")?,
                codegen_set_pic_model: required(library, path, c"lto_codegen_set_pic_model")?,
                codegen_set_cpu: required(library, path, c"lto_codegen_set_cpu")?,
                codegen_add_must_preserve_symbol: required(
                    library,
                    path,
                    c"lto_codegen_add_must_preserve_symbol",
                )?,
                codegen_compile: required(library, path, c"lto_codegen_compile")?,
                codegen_compile_optimized: function(library, c"lto_codegen_compile_optimized"),
                codegen_debug_options: required(library, path, c"lto_codegen_debug_options")?,
                codegen_set_should_internalize: function(
                    library,
                    c"lto_codegen_set_should_internalize",
                ),
                thin,
            })
        }
    }
}

/// A loaded libLTO.
pub(crate) struct LibLto {
    path: PathBuf,
    api: Api,
}

impl std::fmt::Debug for LibLto {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LibLto")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

/// Libraries loaded so far, by path. They are never unloaded (see `dl`).
static LOADED: Mutex<Vec<&'static LibLto>> = Mutex::new(Vec::new());

/// Process-wide libLTO state, guarded by the lock every call holds.
struct State {
    /// The `-mllvm` options handed to each library's copy of LLVM's
    /// command-line parser, which accepts them once per process.
    parsed_options: Vec<(PathBuf, Vec<String>)>,
}

static STATE: Mutex<State> = Mutex::new(State {
    parsed_options: Vec::new(),
});

impl LibLto {
    /// Loads the libLTO at `path`, or returns the one already loaded from
    /// there.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when `dlopen` fails, [`Error::Plugin`] when the library
    /// lacks a required function.
    pub(crate) fn load(path: &Path) -> Result<&'static Self> {
        let mut loaded = LOADED.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(found) = loaded.iter().find(|l| l.path == path) {
            return Ok(found);
        }
        let library = Library::open(path).map_err(|error| Error::io(path, error))?;
        let api = Api::load(&library, path)?;
        let lib: &'static Self = Box::leak(Box::new(Self {
            path: path.to_path_buf(),
            api,
        }));
        loaded.push(lib);
        Ok(lib)
    }

    /// Takes the process-wide libLTO lock.
    pub(crate) fn lock(&'static self) -> Session {
        Session {
            lib: self,
            state: STATE.lock().unwrap_or_else(PoisonError::into_inner),
        }
    }
}

/// A defined or referenced symbol of a bitcode module.
#[derive(Clone, Debug)]
pub(crate) struct ModuleSymbol {
    /// The name, as the object file will spell it (with the Mach-O `_`).
    pub(crate) name: Vec<u8>,
    /// `lto_symbol_attributes`.
    pub(crate) attributes: u32,
}

/// What [`Session::read_module`] learns about a bitcode module.
#[derive(Clone, Debug, Default)]
pub(crate) struct ModuleInfo {
    /// The target triple.
    pub(crate) triple: String,
    /// The Mach-O CPU type and subtype, when libLTO reports them.
    pub(crate) cpu: Option<(u32, u32)>,
    /// The symbols, in module order.
    pub(crate) symbols: Vec<ModuleSymbol>,
    /// Symbols that module-level inline assembly references.
    pub(crate) asm_undefined: Vec<Vec<u8>>,
    /// The linker options (`-lfoo`, `-framework Foo`), separated by spaces.
    pub(crate) linker_options: String,
    /// The module carries a ThinLTO summary.
    pub(crate) thin: bool,
    /// The module defines an Objective-C category.
    pub(crate) objc_category: bool,
}

/// A module handed to code generation.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CodegenInput<'d> {
    /// A name for diagnostics; for ThinLTO also the module identifier, which
    /// must be unique in one code generation.
    pub(crate) name: &'d str,
    /// The bitcode.
    pub(crate) data: &'d [u8],
}

/// Code generation settings.
#[derive(Clone, Debug, Default)]
pub(crate) struct CodegenSettings {
    /// `-mcpu`.
    pub(crate) cpu: Option<String>,
    /// `-mllvm` options.
    pub(crate) llvm_options: Vec<String>,
    /// Let the optimizer internalize symbols that are not preserved (off for
    /// `-r`).
    pub(crate) internalize: bool,
    /// Skip optimization: code generation only.
    pub(crate) codegen_only: bool,
    /// ThinLTO: the cache directory (`-cache_path_lto`).
    pub(crate) cache_dir: Option<PathBuf>,
    /// ThinLTO: cache pruning interval in seconds (`-prune_interval_lto`).
    pub(crate) prune_interval: Option<i32>,
    /// ThinLTO: cache entry expiration in seconds (`-prune_after_lto`).
    pub(crate) prune_after: Option<u32>,
    /// ThinLTO: cache size limit, in percent of the available space
    /// (`-max_relative_cache_size_lto`).
    pub(crate) max_relative_cache_size: Option<u32>,
    /// ThinLTO: write the objects to this directory (`-object_path_lto`).
    pub(crate) objects_dir: Option<PathBuf>,
}

/// An object file produced by code generation.
#[derive(Clone, Debug)]
pub(crate) struct CodegenObject {
    /// Where libLTO wrote it, if it did.
    pub(crate) path: Option<PathBuf>,
    /// The contents.
    pub(crate) data: Vec<u8>,
}

/// The result of code generation.
#[derive(Clone, Debug, Default)]
pub(crate) struct CodegenOutput {
    /// The objects, in module order for ThinLTO.
    pub(crate) objects: Vec<CodegenObject>,
    /// Diagnostics from LLVM and about the options.
    pub(crate) messages: Vec<(Severity, String)>,
}

/// The libLTO lock, held for a sequence of calls.
pub(crate) struct Session {
    lib: &'static LibLto,
    state: MutexGuard<'static, State>,
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("lib", &self.lib)
            .finish_non_exhaustive()
    }
}

/// Copies a string libLTO returned.
///
/// # Safety
///
/// `text` is null or a NUL-terminated string valid for the call.
unsafe fn copy_string(text: *const c_char) -> Vec<u8> {
    if text.is_null() {
        return Vec::new();
    }
    // SAFETY: non-null and NUL-terminated, per the caller.
    unsafe { CStr::from_ptr(text) }.to_bytes().to_vec()
}

/// `text` as a C string, with any NUL byte cut off.
fn c_string(text: &[u8]) -> CString {
    let end = text.iter().position(|&b| b == 0).unwrap_or(text.len());
    CString::new(text.get(..end).unwrap_or_default()).unwrap_or_default()
}

fn c_path(path: &Path) -> CString {
    c_string(path.as_os_str().as_encoded_bytes())
}

/// Disposes of a module on drop.
struct ModuleGuard<'s> {
    api: &'s Api,
    handle: ModuleHandle,
}

impl Drop for ModuleGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: a live handle from `lto_module_create_*`, disposed once.
        unsafe { (self.api.module_dispose)(self.handle) };
    }
}

/// Disposes of a code generator on drop.
struct CodeGenGuard<'s> {
    api: &'s Api,
    handle: CodeGenHandle,
}

impl Drop for CodeGenGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: a live handle from `lto_codegen_create_*`, disposed once.
        unsafe { (self.api.codegen_dispose)(self.handle) };
    }
}

/// Disposes of a ThinLTO code generator on drop.
struct ThinGuard<'s> {
    api: &'s ThinApi,
    handle: ThinHandle,
}

impl Drop for ThinGuard<'_> {
    fn drop(&mut self) {
        // SAFETY: a live handle from `thinlto_create_codegen`, disposed once.
        unsafe { (self.api.codegen_dispose)(self.handle) };
    }
}

/// Collects the messages of the diagnostic handler.
type Messages = Mutex<Vec<(c_int, String)>>;

/// `lto_diagnostic_handler_t`.
///
/// # Safety
///
/// `context` is the `Messages` registered with the handler, alive for as long
/// as the code generator; `text` is null or NUL-terminated.
unsafe extern "C" fn diagnostic_handler(
    severity: c_int,
    text: *const c_char,
    context: *mut c_void,
) {
    if context.is_null() {
        return;
    }
    // SAFETY: registered as a pointer to a live `Messages` by
    // `compile_full`, which outlives the code generator.
    let messages = unsafe { &*context.cast::<Messages>() };
    // SAFETY: forwarded from the caller.
    let text = unsafe { copy_string(text) };
    messages
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push((severity, String::from_utf8_lossy(&text).into_owned()));
}

impl Session {
    /// `lto_get_version`.
    fn version(&self) -> String {
        // SAFETY: the function takes no arguments and returns a static string.
        let text = unsafe { copy_string((self.lib.api.get_version)()) };
        String::from_utf8_lossy(&text).into_owned()
    }

    /// The last error libLTO recorded.
    fn last_error(&self) -> String {
        // SAFETY: returns null or a string valid until the next call.
        let text = unsafe { copy_string((self.lib.api.get_error_message)()) };
        if text.is_empty() {
            "unknown libLTO error".to_owned()
        } else {
            String::from_utf8_lossy(&text).trim_end().to_owned()
        }
    }

    /// Reads the symbols and properties of the bitcode module `data`, in a
    /// context of its own.
    ///
    /// # Errors
    ///
    /// [`Error::Plugin`] with libLTO's message when the module does not load
    /// (not bitcode, or bitcode from a newer LLVM).
    pub(crate) fn read_module(&self, data: &[u8], name: &str) -> Result<ModuleInfo> {
        let api = &self.lib.api;
        let path = c_string(name.as_bytes());
        // SAFETY: `data` is valid for reads of its length for the whole
        // call, and the module (which may reference it) is disposed of
        // before this function returns.
        let handle = unsafe {
            (api.module_create_in_local_context)(data.as_ptr().cast(), data.len(), path.as_ptr())
        };
        if handle.is_null() {
            return Err(Error::Plugin(format!(
                "{name}: libLTO ({}, {}) cannot read the bitcode: {}",
                self.lib.path.display(),
                self.version(),
                self.last_error()
            )));
        }
        let module = ModuleGuard { api, handle };
        let mut info = ModuleInfo::default();
        // SAFETY (below): `module.handle` is live; indexes are below the
        // counts libLTO reported; returned strings are copied at once.
        unsafe {
            info.triple = String::from_utf8_lossy(&copy_string((api.module_get_target_triple)(
                module.handle,
            )))
            .into_owned();
            let count = (api.module_get_num_symbols)(module.handle);
            info.symbols.reserve(usize::try_from(count).unwrap_or(0));
            for index in 0..count {
                let name = copy_string((api.module_get_symbol_name)(module.handle, index));
                let attributes = (api.module_get_symbol_attribute)(module.handle, index);
                info.symbols.push(ModuleSymbol { name, attributes });
            }
            if let (Some(count), Some(name)) = (
                api.module_get_num_asm_undef_symbols,
                api.module_get_asm_undef_symbol_name,
            ) {
                for index in 0..count(module.handle) {
                    info.asm_undefined
                        .push(copy_string(name(module.handle, index)));
                }
            }
            if let Some(options) = api.module_get_linkeropts {
                info.linker_options =
                    String::from_utf8_lossy(&copy_string(options(module.handle))).into_owned();
            }
            if let Some(cputype) = api.module_get_macho_cputype {
                let (mut cpu, mut subtype) = (0, 0);
                // `lto_bool_t` true means failure here.
                if !cputype(module.handle, &raw mut cpu, &raw mut subtype) {
                    info.cpu = Some((cpu, subtype));
                }
            }
            if let Some(is_thin) = api.module_is_thinlto {
                info.thin = is_thin(module.handle);
            }
            if let Some(has_category) = api.module_has_objc_category {
                info.objc_category = has_category(data.as_ptr().cast(), data.len());
            }
        }
        drop(module);
        Ok(info)
    }

    /// Records the `-mllvm` options for this process. LLVM parses its
    /// command line once, so options that differ from an earlier link's
    /// are ignored with a warning. Returns whether `options` still need to
    /// be handed to libLTO.
    fn claim_options(
        &mut self,
        options: &[String],
        messages: &mut Vec<(Severity, String)>,
    ) -> bool {
        let parsed = &mut self.state.parsed_options;
        match parsed.iter().find(|(path, _)| *path == self.lib.path) {
            None => {
                parsed.push((self.lib.path.clone(), options.to_vec()));
                true
            }
            Some((_, earlier)) => {
                if earlier.as_slice() != options {
                    messages.push((
                        Severity::Warning,
                        "libLTO accepts -mllvm options once per process; ignoring options that \
                         differ from an earlier link's"
                            .to_owned(),
                    ));
                }
                false
            }
        }
    }

    /// Full LTO: merges `inputs` into one module, optimizes it and returns
    /// one object. Symbols not in `preserve` may be internalized and
    /// removed.
    ///
    /// # Errors
    ///
    /// [`Error::Plugin`] when a module cannot be loaded or merged, or code
    /// generation fails, with LLVM's messages.
    pub(crate) fn compile_full(
        &mut self,
        inputs: &[CodegenInput<'_>],
        preserve: &[&[u8]],
        settings: &CodegenSettings,
    ) -> Result<CodegenOutput> {
        let lib = self.lib;
        let api = &lib.api;
        let mut output = CodegenOutput::default();
        let pass_options = self.claim_options(&settings.llvm_options, &mut output.messages);
        // Declared before the code generator so that it outlives it.
        let messages: Messages = Mutex::new(Vec::new());
        // SAFETY: no arguments.
        let handle = unsafe { (api.codegen_create_in_local_context)() };
        if handle.is_null() {
            return Err(Error::Plugin(format!(
                "libLTO: cannot create a code generator: {}",
                self.last_error()
            )));
        }
        let codegen = CodeGenGuard { api, handle };
        let fail = |what: &str, session: &Self, messages: &Messages| {
            let collected =
                std::mem::take(&mut *messages.lock().unwrap_or_else(PoisonError::into_inner));
            let detail = collected
                .into_iter()
                .filter(|(severity, _)| *severity == DS_ERROR)
                .map(|(_, text)| text)
                .collect::<Vec<_>>();
            let detail = if detail.is_empty() {
                session.last_error()
            } else {
                detail.join("; ")
            };
            Error::Plugin(format!("LTO: {what}: {detail}"))
        };
        // SAFETY (below): `codegen.handle` is live; every string argument is
        // a NUL-terminated `CString` alive for the call; `messages` outlives
        // the code generator, which is the only user of the pointer.
        unsafe {
            (api.codegen_set_diagnostic_handler)(
                codegen.handle,
                diagnostic_handler,
                (&raw const messages).cast_mut().cast(),
            );
            if (api.codegen_set_debug_model)(codegen.handle, DEBUG_MODEL_DWARF)
                || (api.codegen_set_pic_model)(codegen.handle, PIC_MODEL_DYNAMIC)
            {
                return Err(fail("cannot configure the code generator", self, &messages));
            }
            if let Some(cpu) = &settings.cpu {
                let cpu = c_string(cpu.as_bytes());
                (api.codegen_set_cpu)(codegen.handle, cpu.as_ptr());
            }
            if pass_options {
                for option in &settings.llvm_options {
                    let option = c_string(option.as_bytes());
                    (api.codegen_debug_options)(codegen.handle, option.as_ptr());
                }
            }
            if (!settings.internalize || settings.codegen_only)
                && let Some(set) = api.codegen_set_should_internalize
            {
                set(codegen.handle, false);
            }
            for input in inputs {
                let path = c_string(input.name.as_bytes());
                // The module references `input.data`, which outlives the
                // code generator (it is borrowed for this whole call).
                let module = (api.module_create_in_codegen_context)(
                    input.data.as_ptr().cast(),
                    input.data.len(),
                    path.as_ptr(),
                    codegen.handle,
                );
                if module.is_null() {
                    return Err(fail(
                        &format!("cannot load {}", input.name),
                        self,
                        &messages,
                    ));
                }
                let module = ModuleGuard {
                    api,
                    handle: module,
                };
                if (api.codegen_add_module)(codegen.handle, module.handle) {
                    return Err(fail(
                        &format!("cannot merge {}", input.name),
                        self,
                        &messages,
                    ));
                }
                // The code generator took the module's contents.
                drop(module);
            }
            for name in preserve {
                let name = c_string(name);
                (api.codegen_add_must_preserve_symbol)(codegen.handle, name.as_ptr());
            }
            let mut length = 0usize;
            let compile = match api.codegen_compile_optimized {
                Some(compile) if settings.codegen_only => compile,
                _ => api.codegen_compile,
            };
            let buffer = compile(codegen.handle, &raw mut length);
            if buffer.is_null() {
                return Err(fail("code generation failed", self, &messages));
            }
            // The buffer belongs to the code generator and stays valid until
            // its next call; copy it now.
            let data = std::slice::from_raw_parts(buffer.cast::<u8>(), length).to_vec();
            output.objects.push(CodegenObject { path: None, data });
        }
        drop(codegen);
        for (severity, text) in messages
            .into_inner()
            .unwrap_or_else(PoisonError::into_inner)
        {
            let severity = match severity {
                DS_ERROR => Severity::Error,
                DS_WARNING => Severity::Warning,
                DS_NOTE => Severity::Note,
                _ => continue,
            };
            output.messages.push((severity, text));
        }
        Ok(output)
    }

    /// ThinLTO: optimizes and compiles `inputs` separately, with
    /// cross-module importing, and returns one object per module (some may
    /// be empty). `preserve` are the symbols used outside the bitcode;
    /// `cross_referenced` those one module references in another.
    ///
    /// # Errors
    ///
    /// [`Error::Plugin`] when the library has no ThinLTO interface, a module
    /// is too large, or the objects cannot be read back. LLVM reports errors
    /// during ThinLTO code generation as fatal errors, which end the
    /// process: the C interface has no way to return them.
    pub(crate) fn compile_thin(
        &mut self,
        inputs: &[CodegenInput<'_>],
        preserve: &[&[u8]],
        cross_referenced: &[&[u8]],
        settings: &CodegenSettings,
    ) -> Result<CodegenOutput> {
        let lib = self.lib;
        let Some(api) = &lib.api.thin else {
            return Err(Error::Plugin(format!(
                "{}: this libLTO has no ThinLTO interface",
                lib.path.display()
            )));
        };
        let mut output = CodegenOutput::default();
        let pass_options = self.claim_options(&settings.llvm_options, &mut output.messages);
        let names: Vec<CString> = inputs.iter().map(|i| c_string(i.name.as_bytes())).collect();
        let lengths = inputs
            .iter()
            .map(|input| {
                c_int::try_from(input.data.len()).map_err(|_| {
                    Error::Plugin(format!("{}: bitcode too large for ThinLTO", input.name))
                })
            })
            .collect::<Result<Vec<c_int>>>()?;
        let symbol_arguments = |names: &[&[u8]]| -> Result<Vec<(CString, c_int)>> {
            names
                .iter()
                .map(|name| {
                    let name = c_string(name);
                    let length = c_int::try_from(name.as_bytes().len())
                        .map_err(|_| Error::Limit("symbol name too long for libLTO".into()))?;
                    Ok((name, length))
                })
                .collect()
        };
        let preserve = symbol_arguments(preserve)?;
        let cross_referenced = symbol_arguments(cross_referenced)?;
        let options: Vec<CString> = settings
            .llvm_options
            .iter()
            .map(|o| c_string(o.as_bytes()))
            .collect();
        let option_pointers: Vec<*const c_char> = options.iter().map(|o| o.as_ptr()).collect();
        let option_count = c_int::try_from(option_pointers.len())
            .map_err(|_| Error::Limit("too many -mllvm options".into()))?;
        // libLTO writes into both directories without creating them.
        for dir in [&settings.objects_dir, &settings.cache_dir]
            .into_iter()
            .flatten()
        {
            std::fs::create_dir_all(dir).map_err(|error| Error::io(dir, error))?;
        }

        // SAFETY: no arguments.
        let handle = unsafe { (api.create_codegen)() };
        if handle.is_null() {
            return Err(Error::Plugin(format!(
                "libLTO: cannot create a ThinLTO code generator: {}",
                self.last_error()
            )));
        }
        let thin = ThinGuard { api, handle };
        // SAFETY (below): `thin.handle` is live until the guard drops; every
        // string is a `CString` alive for the whole function; the module
        // buffers are borrowed for the whole function, which outlives the
        // code generator, as `thinlto_codegen_add_module` requires; object
        // buffers are copied before the code generator is disposed of.
        unsafe {
            if pass_options && option_count > 0 {
                (api.debug_options)(option_pointers.as_ptr(), option_count);
            }
            if (api.codegen_set_pic_model)(thin.handle, PIC_MODEL_DYNAMIC) {
                return Err(Error::Plugin(format!(
                    "ThinLTO: cannot set the PIC model: {}",
                    self.last_error()
                )));
            }
            if let Some(cpu) = &settings.cpu {
                let cpu = c_string(cpu.as_bytes());
                (api.codegen_set_cpu)(thin.handle, cpu.as_ptr());
            }
            if settings.codegen_only
                && let Some(set) = api.codegen_set_codegen_only
            {
                set(thin.handle, true);
            }
            if let Some(dir) = &settings.cache_dir {
                let dir = c_path(dir);
                (api.set_cache_dir)(thin.handle, dir.as_ptr());
                if let Some(interval) = settings.prune_interval {
                    (api.set_cache_pruning_interval)(thin.handle, interval);
                }
                if let Some(expiration) = settings.prune_after {
                    (api.set_cache_entry_expiration)(thin.handle, expiration);
                }
                if let Some(percent) = settings.max_relative_cache_size {
                    (api.set_final_cache_size_relative_to_available_space)(thin.handle, percent);
                }
            }
            let objects_dir = settings.objects_dir.as_deref().map(c_path);
            if let (Some(dir), Some(set)) = (&objects_dir, api.set_generated_objects_dir) {
                set(thin.handle, dir.as_ptr());
            }
            for ((input, name), length) in inputs.iter().zip(&names).zip(&lengths) {
                (api.codegen_add_module)(
                    thin.handle,
                    name.as_ptr(),
                    input.data.as_ptr().cast(),
                    *length,
                );
            }
            for (name, length) in &preserve {
                (api.add_must_preserve_symbol)(thin.handle, name.as_ptr(), *length);
            }
            for (name, length) in &cross_referenced {
                (api.add_cross_referenced_symbol)(thin.handle, name.as_ptr(), *length);
            }
            (api.codegen_process)(thin.handle);

            let files = match (
                &objects_dir,
                api.module_get_num_object_files,
                api.module_get_object_file,
            ) {
                (Some(_), Some(count), Some(file)) => {
                    let mut paths = Vec::new();
                    for index in 0..count(thin.handle) {
                        let path = copy_string(file(thin.handle, index));
                        paths.push(PathBuf::from(String::from_utf8_lossy(&path).into_owned()));
                    }
                    paths
                }
                _ => Vec::new(),
            };
            if files.is_empty() {
                for index in 0..(api.module_get_num_objects)(thin.handle) {
                    let buffer = (api.module_get_object)(thin.handle, index);
                    let data = if buffer.buffer.is_null() || buffer.size == 0 {
                        Vec::new()
                    } else {
                        // Owned by the code generator until it is disposed.
                        std::slice::from_raw_parts(buffer.buffer.cast::<u8>(), buffer.size).to_vec()
                    };
                    output.objects.push(CodegenObject { path: None, data });
                }
            } else {
                for path in files {
                    let data = std::fs::read(&path).map_err(|error| Error::io(&path, error))?;
                    output.objects.push(CodegenObject {
                        path: Some(path),
                        data,
                    });
                }
            }
        }
        drop(thin);
        Ok(output)
    }
}

/// The file name of libLTO on this host.
const LIBRARY_NAME: &str = if cfg!(any(target_os = "macos", target_os = "ios")) {
    "libLTO.dylib"
} else {
    "libLTO.so"
};

/// Where Xcode keeps libLTO.
const XCODE_LIBRARIES: [&str; 2] = [
    "/Applications/Xcode.app/Contents/Developer/Toolchains/XcodeDefault.xctoolchain/usr/lib/libLTO.dylib",
    "/Library/Developer/CommandLineTools/usr/lib/libLTO.dylib",
];

/// The libLTO of the toolchain whose `clang` is `clang`:
/// `<bin>/../lib/libLTO.*`, following a symlinked `clang` too.
fn next_to_clang(clang: &Path, out: &mut Vec<PathBuf>) {
    let mut binaries = vec![clang.to_path_buf()];
    if let Ok(real) = std::fs::canonicalize(clang)
        && real != clang
    {
        binaries.push(real);
    }
    for binary in binaries {
        let Some(root) = binary.parent().and_then(Path::parent) else {
            continue;
        };
        out.push(root.join("lib").join(LIBRARY_NAME));
        if !cfg!(any(target_os = "macos", target_os = "ios")) {
            out.push(root.join("lib64").join(LIBRARY_NAME));
        }
    }
}

/// The candidates [`find`] tries, in order, when there is no
/// `-lto_library`.
fn candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if cfg!(any(target_os = "macos", target_os = "ios"))
        && let Ok(output) = std::process::Command::new("xcrun")
            .args(["--find", "clang"])
            .stderr(std::process::Stdio::null())
            .output()
        && output.status.success()
    {
        let text = String::from_utf8_lossy(&output.stdout);
        let clang = text.trim();
        if !clang.is_empty() {
            next_to_clang(Path::new(clang), &mut out);
        }
    }
    if let Some(path) = std::env::var_os("PATH")
        && let Some(clang) = std::env::split_paths(&path)
            .map(|dir| dir.join("clang"))
            .find(|clang| clang.is_file())
    {
        next_to_clang(&clang, &mut out);
    }
    out.extend(XCODE_LIBRARIES.iter().map(PathBuf::from));
    out
}

/// Finds and loads libLTO: `explicit` (`-lto_library`) when given; else
/// the first that loads of: the one next to the `clang` that
/// `xcrun --find clang` names (on Apple hosts), the one next to the first
/// `clang` in `PATH` (`<bin>/../lib/libLTO.dylib`, or `libLTO.so` in `lib`
/// or `lib64` elsewhere), and Xcode's or the Command Line Tools'.
///
/// # Errors
///
/// [`Error::NotFound`] naming the places tried, and the errors of
/// [`LibLto::load`] for an explicit library.
pub(crate) fn find(explicit: Option<&Path>) -> Result<&'static LibLto> {
    if let Some(path) = explicit {
        if !path.is_file() {
            return Err(Error::NotFound(format!(
                "-lto_library {}: no such file",
                path.display()
            )));
        }
        return LibLto::load(path);
    }
    let tried = candidates();
    let mut failures = Vec::new();
    for path in tried.iter().filter(|p| p.is_file()) {
        // A library for another architecture (a 32-bit `lib` on a multilib
        // system) fails to load: try the next.
        match LibLto::load(path) {
            Ok(lib) => return Ok(lib),
            Err(error) => failures.push(error.to_string()),
        }
    }
    let list: Vec<String> = tried.iter().map(|p| p.display().to_string()).collect();
    let mut message = format!(
        "bitcode inputs need libLTO, which was not found (tried {}); pass -lto_library",
        list.join(", ")
    );
    if !failures.is_empty() {
        message.push_str(" (");
        message.push_str(&failures.join("; "));
        message.push(')');
    }
    Err(Error::NotFound(message))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_string_stops_at_nul() {
        assert_eq!(c_string(b"_main\0junk").as_bytes(), b"_main");
        assert_eq!(c_string(b"").as_bytes(), b"");
    }

    #[test]
    fn explicit_library_must_exist() {
        let Err(error) = find(Some(Path::new("/nonexistent/libLTO.dylib"))) else {
            panic!("found a library that does not exist");
        };
        assert!(error.to_string().contains("-lto_library"), "{error}");
    }

    #[test]
    fn candidates_follow_clang() {
        let mut out = Vec::new();
        next_to_clang(Path::new("/opt/llvm/bin/clang"), &mut out);
        assert_eq!(
            out.first().map(PathBuf::as_path),
            Some(Path::new("/opt/llvm/lib").join(LIBRARY_NAME).as_path())
        );
    }

    #[test]
    fn loading_a_non_library_fails() {
        let error = LibLto::load(Path::new("/nonexistent/libLTO.so")).unwrap_err();
        assert!(error.to_string().contains("libLTO"), "{error}");
    }
}
