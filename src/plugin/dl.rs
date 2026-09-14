//! Minimal `dlopen` bindings.
//!
//! The C library already linked into every Rust program on Unix provides
//! `dlopen`, `dlsym` and `dlerror` (glibc 2.34 and later, musl, the BSDs and
//! macOS's libSystem; older glibc keeps them in `libdl`, which Rust's
//! standard library links on those targets). Declaring them here avoids a
//! dependency.
//!
//! Libraries are never closed: see [`Library`].

#![allow(unsafe_code)] // FFI: the only way to load a plugin.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

unsafe extern "C" {
    fn dlopen(filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlerror() -> *mut c_char;
}

/// Resolve every symbol at load time, so a plugin with a missing dependency
/// fails in `dlopen` rather than in the middle of a link.
const RTLD_NOW: c_int = 2;
/// Keep the plugin's symbols out of the global namespace.
#[cfg(any(target_os = "macos", target_os = "ios"))]
const RTLD_LOCAL: c_int = 4;
#[cfg(not(any(target_os = "macos", target_os = "ios")))]
const RTLD_LOCAL: c_int = 0;

/// A loaded shared library.
///
/// Dropping it does not `dlclose` it. Plugins are not written to be unloaded:
/// LLVM's plugin runs `llvm_shutdown` and leaves static destructors and
/// thread-local destructors registered in the process, and GCC's plugin keeps
/// process-level state. Unloading code whose destructors are still
/// registered crashes at exit, so qld keeps plugins mapped until the process
/// ends, as gold does. The handle stays valid for the process's lifetime.
#[derive(Debug)]
pub(crate) struct Library {
    handle: *mut c_void,
}

impl Library {
    /// Loads the shared library at `path`.
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let name = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
        // `dlopen` reports problems through the thread-local `dlerror` state,
        // and the loader lock serializes the call itself.
        // SAFETY: `name` is a valid NUL-terminated string. Loading runs the
        // library's initializers, which is the point of loading a plugin: the
        // user named this library as a linker plugin, and qld trusts it the way
        // every GNU-compatible linker does.
        let handle = unsafe {
            dlerror();
            dlopen(name.as_ptr(), RTLD_NOW | RTLD_LOCAL)
        };
        if handle.is_null() {
            return Err(io::Error::new(io::ErrorKind::InvalidData, last_error()));
        }
        Ok(Self { handle })
    }

    /// An identity for the loaded object: the same library loaded twice
    /// yields the same handle.
    pub(crate) fn id(&self) -> usize {
        self.handle as usize
    }

    /// Looks up the address of `symbol`.
    pub(crate) fn symbol(&self, symbol: &CStr) -> io::Result<*mut c_void> {
        // SAFETY: `handle` came from a successful `dlopen` and is never
        // closed; `symbol` is NUL-terminated.
        let address = unsafe {
            dlerror();
            dlsym(self.handle, symbol.as_ptr())
        };
        if address.is_null() {
            return Err(io::Error::new(io::ErrorKind::NotFound, last_error()));
        }
        Ok(address)
    }
}

/// The loader's description of the last failure on this thread.
fn last_error() -> String {
    // SAFETY: `dlerror` returns null or a NUL-terminated string that stays
    // valid until the next `dl*` call on this thread; it is copied at once.
    unsafe {
        let text = dlerror();
        if text.is_null() {
            "unknown dynamic loader error".to_owned()
        } else {
            CStr::from_ptr(text).to_string_lossy().into_owned()
        }
    }
}
