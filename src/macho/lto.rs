//! LTO for Mach-O links: bitcode inputs compiled through libLTO, the C API
//! Apple's ld64 uses (`lto_module_*`, `lto_codegen_*`, `thinlto_*`).
//!
//! [`prepare`] runs before the rest of the link of one architecture. When
//! no input is LLVM bitcode it returns the options unchanged. Otherwise:
//!
//! 1. It finds the bitcode: files (raw or in a bitcode wrapper), slices of
//!    universal files, and members of archives (and of universal archives).
//!    It loads libLTO — `-lto_library`, else the one next to the `clang`
//!    that `xcrun --find clang` (on macOS) or `PATH` names
//!    (`<bin>/../lib/libLTO.dylib`, `libLTO.so` elsewhere), else Xcode's —
//!    and reads each module's symbols.
//! 2. It resolves symbols as the link would, with a stand-in object for
//!    each module (see `stub`): a file per bitcode file, and the archive
//!    rebuilt with stand-ins for its bitcode members, so archive members
//!    are extracted as ld64 extracts them.
//! 3. It tells libLTO what to preserve, following ld64: a definition in
//!    the bitcode is kept when a native object or the linker (entry point,
//!    `-u`, `-alias`, `-init`) uses the name; in a dylib or bundle, and in
//!    an executable with `-export_dynamic` or an export list, also when it
//!    has default visibility, is not `linkonce_odr` with an unnamed address
//!    (ld64's auto-hidden), and the export lists export it. `-r` keeps
//!    every global and turns internalization off. For ThinLTO, names one
//!    module defines and another references are cross-referenced.
//! 4. Code generation: full LTO produces one object (written to
//!    `-object_path_lto` when given, so the debug map can refer to it);
//!    when every module has a ThinLTO summary, ThinLTO produces one object
//!    per module, cached in `-cache_path_lto` and written to the
//!    `-object_path_lto` directory when given. `-mllvm`, `-mcpu`,
//!    `-prune_*_lto`, `-max_relative_cache_size_lto` and
//!    `-flto-codegen-only` are passed on.
//! 5. It returns the options without the bitcode inputs, and the objects
//!    the link adds after the command line: the LTO objects, then archives
//!    with bitcode members rebuilt with only their native members (loose
//!    objects for `-force_load` and `-all_load`).
//!
//! libLTO is only used with the `plugin` feature on Unix hosts; elsewhere
//! bitcode inputs are reported as unsupported by input collection.

#[cfg(all(feature = "plugin", unix))]
mod driver;
#[cfg(all(feature = "plugin", unix))]
mod stub;

use std::borrow::Cow;
use std::path::PathBuf;
use std::sync::Arc;

use crate::args::LinkOptions;
use crate::diag::DiagnosticSink;
use crate::error::Result;
use crate::macho::read::Arch;

/// What [`prepare`] hands to the rest of the link.
#[derive(Debug)]
pub struct Prepared<'o> {
    /// The options, without the inputs LTO replaced.
    pub options: Cow<'o, LinkOptions>,
    /// Inputs to link after the command line's: the objects LTO produced
    /// and the native members of archives that also held bitcode, with the
    /// names diagnostics and the debug map use.
    pub inputs: Vec<(PathBuf, Arc<[u8]>)>,
}

/// Compiles the bitcode inputs of the `arch` link described by `options`,
/// if there are any. See the [module documentation](self).
///
/// # Errors
///
/// [`Error::NotFound`](crate::Error::NotFound) when libLTO cannot be
/// found, [`Error::Plugin`](crate::Error::Plugin) when libLTO fails, and
/// the errors of input collection and symbol resolution.
pub fn prepare<'o>(
    options: &'o LinkOptions,
    arch: Arch,
    diagnostics: &dyn DiagnosticSink,
) -> Result<Prepared<'o>> {
    #[cfg(all(feature = "plugin", unix))]
    {
        driver::prepare(options, arch, diagnostics)
    }
    #[cfg(not(all(feature = "plugin", unix)))]
    {
        let _ = (arch, diagnostics);
        Ok(Prepared {
            options: Cow::Borrowed(options),
            inputs: Vec::new(),
        })
    }
}
