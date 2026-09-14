//! The safe session API.

use std::path::{Path, PathBuf};

use super::types::{
    ClaimedFile, FileResolution, InputFile, LtoOutput, OutputKind, PluginMessage, UniqueSegment,
};
use crate::args::LinkOptions;
use crate::diag::DiagnosticSink;
use crate::error::{Error, Result};

#[cfg(unix)]
use super::host as backend;
#[cfg(not(unix))]
use unsupported as backend;

/// How the linker describes the link to its plugins.
#[derive(Clone, Debug, Default)]
pub struct SessionOptions {
    /// The kind of output (`LDPT_LINKER_OUTPUT`).
    pub output_kind: OutputKind,
    /// The output path (`LDPT_OUTPUT_NAME`). GCC's plugin derives temporary
    /// file names from it.
    pub output_name: Option<PathBuf>,
    /// Symbols named by `--wrap`, returned by `get_wrap_symbols` so the
    /// plugin keeps the wrapped and wrapper symbols visible.
    pub wrap_symbols: Vec<Vec<u8>>,
    /// Do not run the plugins' cleanup handlers, so their temporary files
    /// (including the native objects they added) survive the link, like GNU
    /// ld's `-plugin-save-temps`.
    pub save_temps: bool,
    /// Called on the reporting thread as soon as a plugin sends a fatal
    /// message, before the plugin continues.
    ///
    /// Plugins write fatal errors expecting the linker to exit inside the
    /// `message` call, as GNU ld and gold do; some then continue into code
    /// that is only safe if it had (GCC's plugin dereferences a failed
    /// `fopen` result, for example). A command-line driver should report the
    /// message and exit here. A library caller that leaves it `None` gets an
    /// error from the current session call instead, and accepts that risk.
    pub fatal_hook: Option<fn(&PluginMessage)>,
}

impl SessionOptions {
    /// Takes the output kind, output path and `--wrap` symbols from a link's
    /// options.
    #[must_use]
    pub fn from_link_options(options: &LinkOptions) -> Self {
        Self {
            output_kind: options.kind.into(),
            output_name: Some(
                options
                    .output
                    .clone()
                    .unwrap_or_else(|| PathBuf::from("a.out")),
            ),
            wrap_symbols: options
                .wrap
                .iter()
                .map(|symbol| symbol.as_bytes().to_vec())
                .collect(),
            save_temps: options.plugin_save_temps,
            fatal_hook: None,
        }
    }
}

/// A loaded plugin, as reported by [`Session::plugins`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PluginInfo {
    /// The path it was loaded from.
    pub path: PathBuf,
    /// The name the plugin gave when negotiating the API level (`"GCC"` for
    /// GCC's plugin), if it negotiated.
    pub identifier: Option<String>,
    /// The version it gave with its name.
    pub version: Option<String>,
    /// The negotiated API level, if any.
    pub api_level: Option<i32>,
}

/// One link's use of LTO plugins.
///
/// # Protocol
///
/// 1. [`new`](Self::new), then [`load_plugin`](Self::load_plugin) for each
///    `-plugin`, in command-line order.
/// 2. [`claim`](Self::claim) each input that may hold IR, in input order,
///    including archive members the resolution loop looks at.
/// 3. Once resolution has settled, [`all_symbols_read`](Self::all_symbols_read)
///    with each claimed file's symbol resolutions. The plugins compile and
///    return native objects.
/// 4. Optionally [`new_input`](Self::new_input) for each object added.
/// 5. [`finish`](Self::finish) after the output is written (or drop the
///    session): the plugins delete their temporary files.
///
/// # One session per process
///
/// The plugin interface keeps its state in globals on both sides, so only
/// one session can exist at a time; [`new`](Self::new) fails while another is
/// alive. Plugins also keep process-level state of their own that a second
/// link would trip over, so qld loads each plugin library at most once per
/// process and never unloads it: loading the same plugin in a later session
/// fails with [`Error::Limit`].
///
/// # Hosts
///
/// Plugins are loaded on Unix hosts. Elsewhere,
/// [`load_plugin`](Self::load_plugin) returns [`Error::Unimplemented`].
#[derive(Debug)]
pub struct Session {
    claimed: Vec<ClaimedFile>,
    records: Vec<usize>,
    finished: bool,
}

impl Session {
    /// Starts the process's plugin session.
    ///
    /// # Errors
    ///
    /// [`Error::Limit`] if another session is active, and [`Error::Option`]
    /// if a name contains a NUL byte.
    pub fn new(options: SessionOptions) -> Result<Self> {
        backend::begin(options)?;
        Ok(Self {
            claimed: Vec::new(),
            records: Vec::new(),
            finished: false,
        })
    }

    /// Loads the plugin at `path` and runs its `onload` with `options` (the
    /// `-plugin-opt` values, verbatim and in order).
    ///
    /// Loading a plugin runs its code in this process: it is trusted like
    /// the compiler that supplied it.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] naming `path` if it does not exist, cannot be loaded,
    ///   or has no `onload` entry point.
    /// - [`Error::Limit`] if this plugin was already used in this process.
    /// - [`Error::Reported`] if the plugin reported an error or failed to
    ///   initialize (the messages went to `diagnostics`).
    /// - [`Error::Internal`] if files were already claimed.
    /// - [`Error::Unimplemented`] on hosts without plugin support.
    pub fn load_plugin(
        &mut self,
        path: &Path,
        options: &[String],
        diagnostics: &dyn DiagnosticSink,
    ) -> Result<()> {
        backend::load(path, options, diagnostics)
    }

    /// Loads every plugin of a link, as [`LinkOptions::plugins`] lists them.
    ///
    /// # Errors
    ///
    /// The first error of [`load_plugin`](Self::load_plugin).
    pub fn load_plugins(
        &mut self,
        plugins: &[(PathBuf, Vec<String>)],
        diagnostics: &dyn DiagnosticSink,
    ) -> Result<()> {
        for (path, options) in plugins {
            self.load_plugin(path, options, diagnostics)?;
        }
        Ok(())
    }

    /// The loaded plugins, in load order.
    #[must_use]
    pub fn plugins(&self) -> Vec<PluginInfo> {
        backend::plugins()
    }

    /// Offers `file` to each plugin's claim handler, in load order, until one
    /// claims it. Returns the claimed file and its symbols, or `None` if no
    /// plugin wants it.
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] if the file cannot be opened.
    /// - [`Error::Reported`] if a plugin reported a fatal error or a handler
    ///   failed (the messages went to `diagnostics`). Non-fatal error
    ///   messages are only reported to `diagnostics`.
    /// - [`Error::Internal`] after [`all_symbols_read`](Self::all_symbols_read).
    pub fn claim(
        &mut self,
        file: &InputFile,
        diagnostics: &dyn DiagnosticSink,
    ) -> Result<Option<&ClaimedFile>> {
        match backend::claim(file, diagnostics)? {
            Some((record, claimed)) => {
                self.records.push(record);
                self.claimed.push(claimed);
                Ok(self.claimed.last())
            }
            None => Ok(None),
        }
    }

    /// The files claimed so far, in claim order.
    #[must_use]
    pub fn claimed_files(&self) -> &[ClaimedFile] {
        &self.claimed
    }

    /// Reports resolutions and lets the plugins compile.
    ///
    /// `resolve` is called once per claimed file, in claim order, and returns
    /// the resolution of each of its symbols. Then every plugin's
    /// all-symbols-read handler runs, in load order; they ask for the
    /// resolutions and add native objects, which are returned.
    ///
    /// # Errors
    ///
    /// - [`Error::Internal`] if a resolution list does not have one entry
    ///   per symbol, or if called twice.
    /// - [`Error::Reported`] if a plugin reported a fatal error or a handler
    ///   failed. Non-fatal errors are counted in [`LtoOutput::errors`].
    pub fn all_symbols_read(
        &mut self,
        mut resolve: impl FnMut(&ClaimedFile) -> FileResolution,
        diagnostics: &dyn DiagnosticSink,
    ) -> Result<LtoOutput> {
        let mut resolutions = Vec::with_capacity(self.claimed.len());
        for (&record, file) in self.records.iter().zip(&self.claimed) {
            let resolution = resolve(file);
            if let FileResolution::Included(values) = &resolution
                && values.len() != file.symbols.len()
            {
                return Err(Error::Internal(format!(
                    "{}: {} resolutions for {} plugin symbols",
                    file.path.display(),
                    values.len(),
                    file.symbols.len()
                )));
            }
            resolutions.push((record, resolution));
        }
        backend::all_symbols_read(resolutions, diagnostics)
    }

    /// Tells the plugins' new-input handlers about a file added to the link
    /// after [`all_symbols_read`](Self::all_symbols_read), usually one of
    /// [`LtoOutput::files`]. Returns the unique-segment requests the handlers
    /// made.
    ///
    /// # Errors
    ///
    /// As for [`claim`](Self::claim).
    pub fn new_input(
        &mut self,
        file: &InputFile,
        diagnostics: &dyn DiagnosticSink,
    ) -> Result<Vec<UniqueSegment>> {
        backend::new_input(file, diagnostics)
    }

    /// Runs the plugins' cleanup handlers (unless
    /// [`SessionOptions::save_temps`]) and ends the session.
    ///
    /// Plugins delete the objects they added here, so call it once the
    /// output no longer needs their contents.
    ///
    /// # Errors
    ///
    /// [`Error::Reported`] if a plugin reported errors during cleanup.
    pub fn finish(mut self, diagnostics: &dyn DiagnosticSink) -> Result<()> {
        self.finished = true;
        backend::end(Some(diagnostics))
    }
}

impl Drop for Session {
    /// Runs the cleanup handlers and ends the session, if
    /// [`finish`](Session::finish) did not. Messages from cleanup are lost.
    fn drop(&mut self) {
        if !self.finished {
            let _ = backend::end(None);
        }
    }
}

/// The backend for hosts that cannot load plugins.
#[cfg(not(unix))]
mod unsupported {
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::{PluginInfo, SessionOptions};
    use crate::diag::DiagnosticSink;
    use crate::error::{Error, Result};
    use crate::plugin::types::{ClaimedFile, FileResolution, InputFile, LtoOutput, UniqueSegment};

    static ACTIVE: AtomicBool = AtomicBool::new(false);

    pub(super) fn begin(_options: SessionOptions) -> Result<()> {
        if ACTIVE.swap(true, Ordering::SeqCst) {
            return Err(Error::Limit(
                "only one LTO plugin session can be active in a process".to_owned(),
            ));
        }
        Ok(())
    }

    pub(super) fn load(
        path: &Path,
        _options: &[String],
        _diagnostics: &dyn DiagnosticSink,
    ) -> Result<()> {
        Err(Error::Unimplemented(format!(
            "{}: LTO plugins on this host (M6)",
            path.display()
        )))
    }

    pub(super) fn plugins() -> Vec<PluginInfo> {
        Vec::new()
    }

    pub(super) fn claim(
        _file: &InputFile,
        _diagnostics: &dyn DiagnosticSink,
    ) -> Result<Option<(usize, ClaimedFile)>> {
        Ok(None)
    }

    pub(super) fn all_symbols_read(
        _resolutions: Vec<(usize, FileResolution)>,
        _diagnostics: &dyn DiagnosticSink,
    ) -> Result<LtoOutput> {
        Ok(LtoOutput::default())
    }

    pub(super) fn new_input(
        _file: &InputFile,
        _diagnostics: &dyn DiagnosticSink,
    ) -> Result<Vec<UniqueSegment>> {
        Ok(Vec::new())
    }

    pub(super) fn end(_diagnostics: Option<&dyn DiagnosticSink>) -> Result<()> {
        ACTIVE.store(false, Ordering::SeqCst);
        Ok(())
    }
}
