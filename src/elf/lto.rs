//! Link-time optimization: IR inputs claimed through the GNU linker plugin
//! API ([`crate::plugin`]), resolved together with the regular objects,
//! compiled by the plugin, and replaced by the native objects it returns.
//!
//! **Workstream W18** (roadmap M6). See `docs/optimizations.md` ("LTO").
//!
//! # Flow
//!
//! [`resolve`] replaces the driver's single call to
//! [`resolve_symbols_with`]. Without `-plugin` it is exactly that call.
//! With plugins, resolution runs twice:
//!
//! 1. **Claims.** An input is IR when it is LLVM bitcode or an ELF object
//!    with `.gnu.lto_*` sections ([`GccLto`](super::object::GccLto)). The
//!    plugin session is created, and the plugins loaded, when the first one
//!    is met, so a link without IR never loads a plugin and its output does
//!    not change. IR inputs are claimed on the resolving thread in the round
//!    hook ([`RoundHook::after_load`](crate::symbols::RoundHook::after_load)),
//!    round by round, in input-position order (which is what makes the
//!    plugins' view, and so the generated code, independent of the thread
//!    count). A claimed file's symbols become its
//!    [`IrSymbols`]: the names are copied into one buffer per file, added to
//!    the link's [`FileTable`](crate::input::FileTable) so they live as long
//!    as every other input, and undefined names go through `--wrap` like a
//!    regular object's. COMDAT keys take part in the group claims of
//!    [`ComdatHook`], so an IR copy and a native copy of a group are
//!    deduplicated like two native ones.
//!
//!    Archive members whose symbols the archive index does not list (an
//!    archive without an index, or one built by an `ar` without the plugin)
//!    are claimed before resolution with `known_used` false, to learn what
//!    they define, and claimed again if a round extracts them: GCC's plugin
//!    only compiles files claimed as known to be used. A GCC object with
//!    native code (`-ffat-lto-objects`) that no plugin claims is linked
//!    natively; bitcode and slim GCC objects are errors then.
//!
//! 2. **Resolutions.** Once the first resolution settles, each claimed file
//!    gets a [`SymbolResolution`] per symbol ([`symbol_resolution`], after
//!    GNU ld's `get_symbols` and lld's rules): a definition prevails when the
//!    table kept it; it is [`PrevailingDef`](SymbolResolution::PrevailingDef)
//!    when a regular object or the linker (`-u`, the entry point,
//!    `--defsym`) names it, when it is wrapped or a wrapper (`--wrap`), and
//!    for `-r`;
//!    [`PrevailingDefIronlyExp`](SymbolResolution::PrevailingDefIronlyExp)
//!    when it may be referenced from outside the output (shared output,
//!    `--export-dynamic`, `--dynamic-list`, or a shared library in the link
//!    that refers to it or defines it too), unless its visibility or a
//!    version script makes it local; and
//!    [`PrevailingDefIronly`](SymbolResolution::PrevailingDefIronly)
//!    otherwise, which lets the plugin internalize it. Files claimed but
//!    never extracted are [`FileResolution::NotIncluded`].
//!
//! 3. **Second resolution.** The plugins compile and return native objects,
//!    which take the place of the claimed files at the position of the first
//!    one; the libraries they ask for (`-pass-through=-lgcc` and the like) are
//!    appended, searched in the plugins' directories and then the `-L`
//!    paths. Every input that was live stays live, so archive members
//!    extracted in the first resolution are kept; COMDAT group copies
//!    discarded in the first resolution stay discarded, since the kept copy
//!    may now be a plain definition in the generated code; and resolution
//!    runs again from a fresh symbol table. An IR member extracted only now
//!    is an error: it was not part of LTO. Objects the plugin generated may
//!    be IR themselves (GCC's incremental `-r` output); they are linked as
//!    regular objects.
//!
//! 4. **Finish.** [`LtoLink::finish`] runs the plugins' cleanup, which
//!    deletes their temporary objects, after the output is written.

#![deny(clippy::arithmetic_side_effects)]

use crate::args::LinkOptions;
use crate::diag::DiagnosticSink;
use crate::error::{Error, Result};
use crate::symbols::{Resolution, SymbolName, SymbolTable, SymbolUse, resolve_symbols_with};

use super::inputs::{Inputs, LtoMode};
use super::resolve::{ComdatHook, ElfRules};

#[cfg(feature = "plugin")]
pub use plugin_link::{SymbolFacts, Winner, symbol_resolution};

#[cfg(feature = "plugin")]
use crate::plugin::{FileResolution, SymbolResolution};

/// The symbols of an IR file an LTO plugin claimed, as resolution sees them.
///
/// Entry `i` of every vector describes the plugin's symbol `i`.
#[derive(Clone, Debug, Default)]
pub struct IrSymbols<'a> {
    /// Names (with `--wrap` applied to undefined ones), borrowed from a
    /// buffer in the link's file table.
    pub names: Vec<SymbolName<'a>>,
    /// How each takes part in resolution.
    pub uses: Vec<SymbolUse>,
    /// The file's distinct COMDAT keys, in order of first use.
    pub comdats: Vec<&'a [u8]>,
    /// For each symbol, the index plus one of its key in
    /// [`comdats`](Self::comdats), or 0.
    pub comdat_of: Vec<u32>,
    /// For each key, whether another file's copy of the group was kept.
    pub discarded: Vec<bool>,
    /// The claim that produced these symbols: its index among the session's
    /// claims.
    pub claim: usize,
}

impl<'a> IrSymbols<'a> {
    /// Stops the definitions of the COMDAT groups flagged in `discarded` (by
    /// index in [`comdats`](Self::comdats)) from taking part in resolution.
    pub fn discard_comdats(&mut self, discarded: Vec<bool>) {
        for (use_, &comdat) in self.uses.iter_mut().zip(&self.comdat_of) {
            let Some(key) = comdat.checked_sub(1) else {
                continue;
            };
            if matches!(use_, SymbolUse::Definition { .. })
                && discarded.get(key as usize).copied().unwrap_or(false)
            {
                *use_ = SymbolUse::Ignore;
            }
        }
        self.discarded = discarded;
    }

    /// The names the file defines, for a lazy archive member.
    #[must_use]
    pub fn defined_names(&self) -> Vec<SymbolName<'a>> {
        self.names
            .iter()
            .zip(&self.uses)
            .filter(|(_, use_)| matches!(use_, SymbolUse::Definition { .. }))
            .map(|(&name, _)| name)
            .collect()
    }
}

/// The kind of IR an input carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrKind {
    /// LLVM bitcode (`clang -flto`).
    LlvmBitcode,
    /// A GCC object with IR only (`gcc -flto`).
    GccSlim,
    /// A GCC object with IR and native code (`-ffat-lto-objects`).
    GccFat,
}

/// The error for an IR input that cannot be linked in `mode`, naming `file`.
#[must_use]
pub fn ir_error(file: &str, kind: IrKind, mode: LtoMode) -> Error {
    let (what, driver, plugin) = match kind {
        IrKind::LlvmBitcode => ("LLVM bitcode", "clang", "LLVMgold.so"),
        IrKind::GccSlim | IrKind::GccFat => ("GCC LTO", "gcc", "liblto_plugin.so"),
    };
    Error::Plugin(match mode {
        LtoMode::NoPlugin => format!(
            "{file}: {what} input needs an LTO plugin: link it through the compiler \
             driver ({driver} -flto), which passes -plugin {plugin}"
        ),
        LtoMode::Unsupported => format!(
            "{file}: {what} input needs an LTO plugin, and this qld was built without \
             plugin support (cargo feature `plugin`)"
        ),
        LtoMode::Claim => {
            format!("{file}: no LTO plugin claimed this {what} input (it needs {plugin})")
        }
        LtoMode::AfterLto | LtoMode::Generated => format!(
            "{file}: {what} input is needed by the code LTO generated, but was not part \
             of LTO (it defines a symbol that only the generated code references)"
        ),
    })
}

/// The plugin session of a link, kept until the output is written.
#[derive(Debug, Default)]
pub struct LtoLink {
    #[cfg(feature = "plugin")]
    session: Option<crate::plugin::Session>,
}

impl LtoLink {
    /// Whether plugins were loaded (the link had IR inputs).
    #[must_use]
    pub fn is_active(&self) -> bool {
        #[cfg(feature = "plugin")]
        {
            self.session.is_some()
        }
        #[cfg(not(feature = "plugin"))]
        {
            false
        }
    }

    /// Ends the plugin session, if any: the plugins delete their temporary
    /// files. Call it once the output is written.
    ///
    /// # Errors
    ///
    /// [`Error::Reported`] if a plugin reported errors during cleanup.
    pub fn finish(self, diagnostics: &dyn DiagnosticSink) -> Result<()> {
        #[cfg(feature = "plugin")]
        if let Some(session) = self.session {
            return session.finish(diagnostics);
        }
        let _ = diagnostics;
        Ok(())
    }
}

/// Resolves the symbols of `inputs`, running LTO when IR inputs are
/// claimed; see the [module documentation](self). On return, `inputs` holds
/// the files the returned resolution describes.
///
/// # Errors
///
/// Errors of [`resolve_symbols_with`], plugin errors, IR inputs that cannot
/// be linked, and [`Error::Reported`] when a plugin reported errors while
/// generating code.
pub fn resolve<'a>(
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
    rules: &ElfRules,
    inputs: &mut Inputs<'a>,
) -> Result<(SymbolTable<'a>, Resolution<'a>, LtoLink)> {
    #[cfg(feature = "plugin")]
    if !options.plugins.is_empty() {
        return plugin_link::resolve(options, diagnostics, rules, inputs);
    }
    let _ = (options, diagnostics);
    let mut symbols = SymbolTable::new();
    let mut comdat = ComdatHook::default();
    let resolution = resolve_symbols_with(&mut symbols, rules, &mut inputs.files, &mut comdat)?;
    Ok((symbols, resolution, LtoLink::default()))
}

#[cfg(feature = "plugin")]
mod plugin_link {
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use hashbrown::HashMap;
    use rayon::prelude::*;

    use super::{
        ComdatHook, ElfRules, Error, FileResolution, Inputs, IrSymbols, LinkOptions, LtoLink,
        LtoMode, Resolution, Result, SymbolName, SymbolResolution, SymbolTable, SymbolUse,
        resolve_symbols_with,
    };
    use crate::diag::{Diagnostic, DiagnosticSink};
    use crate::elf::dso;
    use crate::elf::export::{self, Mode, VersionScript};
    use crate::elf::inputs::{self, ElfInput, InputRole};
    use crate::elf::object::split_version;
    use crate::elf::read::consts::{STB_LOCAL, STV_HIDDEN, STV_INTERNAL};
    use crate::elf::resolve::AUX_COMDAT;
    use crate::ids::FileId;
    use crate::input::FileFormat;
    use crate::plugin::options::{PluginFlavor, classify, missing_environment};
    use crate::plugin::{
        ClaimedFile, InputFile, PluginMessage, Session, SessionOptions, SymbolKind, Visibility,
    };
    use crate::script::Pattern;
    use crate::symbols::{DefinitionKind, LoadHook, RoundFile, RoundHook, SymbolFlags};

    /// Pass-one flag: a live regular object or the linker names the symbol.
    const LTO_REGULAR: SymbolFlags = SymbolFlags::backend(8);
    /// Pass-one flag: a shared library in the link references or defines
    /// the symbol.
    const LTO_DYNAMIC: SymbolFlags = SymbolFlags::backend(9);
    /// Pass-one flag: a regular object gives the symbol hidden or internal
    /// visibility.
    const LTO_HIDDEN: SymbolFlags = SymbolFlags::backend(10);

    /// What resolved a symbol, as far as an LTO plugin cares.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum Winner {
        /// Nothing defines it (or only an unextracted archive member).
        #[default]
        Undefined,
        /// A claimed IR file.
        Ir,
        /// A regular object, or the linker (`--defsym`).
        Regular,
        /// A shared library.
        Shared,
    }

    /// Everything [`symbol_resolution`] needs to know about one symbol of a
    /// claimed file.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct SymbolFacts {
        /// The plugin reported a reference, not a definition.
        pub undefined: bool,
        /// This definition is the one resolution kept.
        pub prevailing: bool,
        /// The kind of file whose definition resolution kept.
        pub winner: Winner,
        /// A regular object or the linker names the symbol (a reference, a
        /// definition, a common symbol, or a discarded COMDAT copy whose
        /// references bind to the kept one), so the generated code must
        /// define it.
        pub regular_ref: bool,
        /// A shared library in the link references the symbol, or defines
        /// it too (the output's definition is then exported so that the
        /// library binds to it).
        pub dynamic_ref: bool,
        /// Every default-visibility definition is exported: a shared
        /// object, or `--export-dynamic` in a dynamic executable.
        pub export_all: bool,
        /// `--dynamic-list` or `--export-dynamic-symbol` names the symbol.
        pub listed: bool,
        /// The most restrictive visibility the IR definition or a regular
        /// object gives the symbol.
        pub visibility: Visibility,
        /// A version script makes the symbol local.
        pub script_local: bool,
        /// The output is relocatable (`-r`): every symbol may be used later.
        pub relocatable: bool,
        /// `--wrap` names the symbol, or it is a `__wrap_`/`__real_` symbol
        /// of a wrapped one: the generated code is wrapped after LTO, so the
        /// plugin must keep it.
        pub wrapped: bool,
    }

    /// The resolution to report to the plugin for one symbol.
    #[must_use]
    pub fn symbol_resolution(facts: &SymbolFacts) -> SymbolResolution {
        if facts.undefined {
            return match facts.winner {
                Winner::Undefined => SymbolResolution::Undefined,
                Winner::Ir => SymbolResolution::ResolvedIr,
                Winner::Regular => SymbolResolution::ResolvedExec,
                Winner::Shared => SymbolResolution::ResolvedDyn,
            };
        }
        if !facts.prevailing {
            return match facts.winner {
                Winner::Ir => SymbolResolution::PreemptedIr,
                Winner::Regular | Winner::Shared | Winner::Undefined => {
                    SymbolResolution::PreemptedRegular
                }
            };
        }
        if facts.relocatable || facts.regular_ref || facts.wrapped {
            return SymbolResolution::PrevailingDef;
        }
        let visible = matches!(
            facts.visibility,
            Visibility::Default | Visibility::Protected
        ) && !facts.script_local
            && (facts.dynamic_ref || facts.export_all || facts.listed);
        if visible {
            SymbolResolution::PrevailingDefIronlyExp
        } else {
            SymbolResolution::PrevailingDefIronly
        }
    }

    /// Reports a fatal plugin message and exits, as GNU ld does: plugins
    /// continue after a fatal message into code that is only safe if the
    /// linker exited (GCC's plugin dereferences a failed `fopen`).
    fn exit_on_fatal(message: &PluginMessage) {
        eprintln!("{}: error: {}", crate::PROGRAM_NAME, message.text);
        std::process::exit(1);
    }

    /// The claiming state of one link.
    struct Driver<'o> {
        options: &'o LinkOptions,
        diagnostics: &'o dyn DiagnosticSink,
        session: Option<Session>,
        /// Number of files claimed so far.
        claims: usize,
        /// Time spent loading plugins and in their claim handlers.
        claim_time: Duration,
    }

    impl Driver<'_> {
        /// The session, created (and the plugins loaded) on first use.
        fn session(&mut self) -> Result<&mut Session> {
            if self.session.is_none() {
                let mut session = Session::new(SessionOptions {
                    fatal_hook: self.options.exit_on_plugin_fatal.then_some(exit_on_fatal),
                    ..SessionOptions::from_link_options(self.options)
                })?;
                for (path, plugin_options) in &self.options.plugins {
                    let flavor = PluginFlavor::detect(path, plugin_options);
                    let missing =
                        missing_environment(flavor, plugin_options, |name| std::env::var_os(name));
                    if !missing.is_empty() {
                        self.diagnostics.emit(Diagnostic::warning(format!(
                            "{}: {} not set: the plugin cannot compile LTO inputs outside the \
                             compiler driver",
                            path.display(),
                            missing.join(" and ")
                        )));
                    }
                    for option in plugin_options {
                        if classify(flavor, option).exits_process() {
                            self.diagnostics.emit(Diagnostic::warning(format!(
                                "{}: -plugin-opt={option}: the plugin ends the link after \
                                 writing its output",
                                path.display()
                            )));
                        }
                    }
                }
                session.load_plugins(&self.options.plugins, self.diagnostics)?;
                self.session = Some(session);
            }
            self.session
                .as_mut()
                .ok_or_else(|| Error::Internal("LTO plugin session missing".into()))
        }

        /// Offers `file` (input `handle`) to the plugins.
        fn claim<'a>(
            &mut self,
            file: &ElfInput<'a>,
            handle: usize,
            known_used: bool,
        ) -> Result<Option<IrSymbols<'a>>> {
            let start = Instant::now();
            let mut input = input_file(file, handle)?;
            input.known_used = known_used;
            let diagnostics = self.diagnostics;
            let claim = self.claims;
            let session = self.session()?;
            let claimed = session.claim(&input, diagnostics)?;
            let symbols = claimed
                .map(|claimed| ir_symbols(file, claimed, claim))
                .transpose()?;
            if symbols.is_some() {
                self.claims = self.claims.saturating_add(1);
            }
            self.claim_time = self.claim_time.saturating_add(start.elapsed());
            Ok(symbols)
        }
    }

    /// How `file` is described to a plugin: an archive member by the
    /// archive's path and the member's offset, as GNU ld does; a thin
    /// archive member by its own path.
    fn input_file(file: &ElfInput<'_>, handle: usize) -> Result<InputFile> {
        let Some(data) = file.file else {
            return Err(Error::Internal(format!(
                "{}: LTO input not loaded",
                file.display()
            )));
        };
        let bytes = data.data();
        let size = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let handle = u64::try_from(handle).unwrap_or(u64::MAX);
        let within = data
            .parent()
            .and_then(|parent| file.table().get(parent))
            .and_then(|archive| {
                let base = archive.data().as_ptr() as usize;
                let offset = (bytes.as_ptr() as usize).checked_sub(base)?;
                let end = offset.checked_add(bytes.len())?;
                (end <= archive.data().len()).then(|| (archive.path(), offset))
            });
        Ok(match within {
            Some((path, offset)) => InputFile::new(
                path,
                u64::try_from(offset).unwrap_or(u64::MAX),
                size,
                handle,
            ),
            None => InputFile::new(data.path(), 0, size, handle),
        })
    }

    /// Turns a claimed file's symbols into resolution input, with names
    /// owned by the file table.
    fn ir_symbols<'a>(
        file: &ElfInput<'a>,
        claimed: &ClaimedFile,
        claim: usize,
    ) -> Result<IrSymbols<'a>> {
        let mut arena = Vec::new();
        let mut spans = Vec::with_capacity(claimed.symbols.len());
        let mut keys: HashMap<&[u8], u32> = HashMap::new();
        let mut key_order: Vec<&[u8]> = Vec::new();
        for symbol in &claimed.symbols {
            let start = arena.len();
            arena.extend_from_slice(&symbol.name);
            if let Some(version) = &symbol.version {
                arena.push(b'@');
                arena.extend_from_slice(version);
            }
            let comdat = match &symbol.comdat_key {
                Some(key) => match keys.get(key.as_slice()) {
                    Some(&index) => index,
                    None => {
                        key_order.push(key);
                        let index = u32::try_from(key_order.len())
                            .map_err(|_| Error::Limit("too many COMDAT keys".into()))?;
                        keys.insert(key, index);
                        index
                    }
                },
                None => 0,
            };
            spans.push((start, arena.len(), comdat));
        }
        let mut key_spans = Vec::with_capacity(key_order.len());
        for key in &key_order {
            let start = arena.len();
            arena.extend_from_slice(key);
            key_spans.push((start, arena.len()));
        }

        let table = file.table();
        let id = table.add_bytes(
            format!("{} (LTO symbol names)", file.display()),
            Arc::from(arena),
        )?;
        let data: &'a [u8] = table
            .get(id)
            .map(crate::input::InputFile::data)
            .unwrap_or_default();
        let slice = |start: usize, end: usize| -> Result<&'a [u8]> {
            data.get(start..end)
                .ok_or_else(|| Error::Internal("LTO symbol name buffer".into()))
        };
        let wrap = file.wrap();
        let mut names = Vec::with_capacity(spans.len());
        let mut uses = Vec::with_capacity(spans.len());
        let mut comdat_of = Vec::with_capacity(spans.len());
        for (symbol, &(start, end, comdat)) in claimed.symbols.iter().zip(&spans) {
            let mut name = slice(start, end)?;
            if symbol.kind.is_undefined() {
                name = wrap.redirect(name);
            }
            let (base, version) = split_version(name);
            names.push(SymbolName::with_version(base, version));
            let aux = if comdat == 0 { 0 } else { AUX_COMDAT };
            uses.push(match symbol.kind {
                SymbolKind::Definition => SymbolUse::Definition {
                    kind: DefinitionKind::Regular,
                    aux,
                },
                SymbolKind::WeakDefinition => SymbolUse::Definition {
                    kind: DefinitionKind::Weak,
                    aux,
                },
                SymbolKind::Common => SymbolUse::Definition {
                    kind: DefinitionKind::Common,
                    aux: symbol.size & !AUX_COMDAT,
                },
                SymbolKind::Undefined => SymbolUse::Reference { weak: false },
                SymbolKind::WeakUndefined => SymbolUse::Reference { weak: true },
            });
            comdat_of.push(comdat);
        }
        let comdats = key_spans
            .iter()
            .map(|&(start, end)| slice(start, end))
            .collect::<Result<Vec<_>>>()?;
        Ok(IrSymbols {
            names,
            uses,
            discarded: vec![false; comdats.len()],
            comdats,
            comdat_of,
            claim,
        })
    }

    /// Claims the IR files of each round before their symbols are read,
    /// then claims COMDAT groups.
    struct ClaimHook<'d, 'o, 'a> {
        driver: &'d mut Driver<'o>,
        comdat: ComdatHook<'a>,
    }

    impl<'a> RoundHook<ElfInput<'a>> for ClaimHook<'_, '_, 'a> {
        // Regular objects offer their groups as they load; IR files, which
        // are claimed below, in `after_load`.
        fn load_hook(&self) -> Option<&dyn LoadHook<ElfInput<'a>>> {
            self.comdat.load_hook()
        }

        fn after_load(
            &mut self,
            round: usize,
            files: &mut [RoundFile<'_, ElfInput<'a>>],
        ) -> Result<()> {
            // `files` is in input-position order.
            for round_file in files.iter_mut() {
                let file = &mut *round_file.file;
                if file.lto_mode() != LtoMode::Claim || file.pending_ir().is_none() {
                    continue;
                }
                match self.driver.claim(file, round_file.id.index(), true)? {
                    Some(symbols) => {
                        file.ir = Some(Box::new(symbols));
                        // A fat object's native code is replaced by LTO's.
                        file.object = None;
                    }
                    None => file.claim_declined()?,
                }
            }
            self.comdat.after_load(round, files)
        }
    }

    /// Whole-link facts [`file_resolution`] needs.
    struct Context {
        export_all: bool,
        relocatable: bool,
        shared: bool,
        script: Option<VersionScript>,
        patterns: Vec<Pattern>,
        wrap: Vec<Vec<u8>>,
    }

    impl Context {
        fn new(options: &LinkOptions, files: &[ElfInput<'_>]) -> Result<Self> {
            let mode = Mode::new(options, files.iter().any(|f| f.shared.is_some()));
            let export_all = mode.shared
                || (options.export_dynamic
                    && mode.dynamic
                    && mode.kind != crate::args::OutputKind::StaticPie);
            let (script, patterns) = export::read_scripts(options)?;
            Ok(Self {
                export_all,
                relocatable: options.kind == crate::args::OutputKind::Relocatable,
                shared: mode.shared,
                script,
                patterns,
                wrap: options.wrap.iter().map(|w| w.as_bytes().to_vec()).collect(),
            })
        }

        fn wrapped(&self, name: &[u8]) -> bool {
            if self.wrap.is_empty() {
                return false;
            }
            let base = name
                .strip_prefix(b"__wrap_")
                .or_else(|| name.strip_prefix(b"__real_"))
                .unwrap_or(name);
            self.wrap.iter().any(|w| w == name || w == base)
        }
    }

    /// The `--as-needed` libraries that a strong reference from IR keeps
    /// before code generation, by file index: those that give the symbol an
    /// unversioned definition. GNU ld's as-needed check accepts a reference
    /// from a plugin's symbols only on the symbol's own name, which a
    /// versioned default definition (`name@@VERSION`) does not have, so IR
    /// references alone never keep a library for a versioned symbol (the
    /// library comes back for the generated code).
    fn needed_by_ir(
        files: &[ElfInput<'_>],
        symbols: &SymbolTable<'_>,
        resolution: &Resolution<'_>,
    ) -> Vec<bool> {
        let mut needed = vec![false; files.len()];
        for (index, file) in files.iter().enumerate() {
            let Some(ir) = &file.ir else {
                continue;
            };
            let ids = resolution.symbol_ids(FileId::new(index));
            for (use_, &id) in ir.uses.iter().zip(ids) {
                if *use_ != (SymbolUse::Reference { weak: false }) {
                    continue;
                }
                let def = symbols.definition(id);
                if def.kind != DefinitionKind::Shared {
                    continue;
                }
                let owner = def.file.index();
                let Some(shared) = files.get(owner).and_then(|f| f.shared.as_ref()) else {
                    continue;
                };
                let unversioned = shared
                    .symbols
                    .get(def.index as usize)
                    .and_then(|&dynsym| shared.elf.symbol_version(dynsym as usize).ok())
                    .is_some_and(|version| version.info.is_none());
                if unversioned && let Some(slot) = needed.get_mut(owner) {
                    *slot = true;
                }
            }
        }
        needed
    }

    /// Records, in the first resolution's table, which symbols regular
    /// objects, shared libraries and the linker name.
    fn mark_usage(files: &[ElfInput<'_>], symbols: &SymbolTable<'_>, resolution: &Resolution<'_>) {
        files.par_iter().enumerate().for_each(|(index, file)| {
            let id = FileId::new(index);
            if !resolution.is_live(id) || file.ir.is_some() {
                return;
            }
            let ids = resolution.symbol_ids(id);
            if let Some(object) = &file.object {
                let table = object.elf.symbols();
                for (local, &symbol) in ids.iter().enumerate() {
                    if !matches!(object.uses.get(local), Some(SymbolUse::Ignore) | None) {
                        symbols.set_flags(symbol, LTO_REGULAR);
                    }
                    if let Some(raw) = local
                        .checked_add(object.first_global)
                        .and_then(|i| table.get_raw(i))
                        && raw.binding() != STB_LOCAL
                        && matches!(raw.visibility(), STV_HIDDEN | STV_INTERNAL)
                    {
                        symbols.set_flags(symbol, LTO_HIDDEN);
                    }
                }
                for &(local, _) in &object.group_ignored {
                    if let Some(&symbol) = ids.get(local as usize) {
                        symbols.set_flags(symbol, LTO_REGULAR);
                    }
                }
            } else if let Some(shared) = &file.shared {
                // A reference, or a definition too: the output exports a
                // symbol a library also defines, so the library binds to
                // the output's copy (GNU ld's `non_ir_ref_dynamic`).
                for (local, &symbol) in ids.iter().enumerate() {
                    if !matches!(shared.uses.get(local), Some(SymbolUse::Ignore) | None) {
                        symbols.set_flags(symbol, LTO_DYNAMIC);
                    }
                }
            } else if file.role == InputRole::Internal {
                for &symbol in ids {
                    symbols.set_flags(symbol, LTO_REGULAR);
                }
            }
        });
    }

    /// The resolution of every symbol of the claimed file `handle`.
    fn file_resolution(
        context: &Context,
        files: &[ElfInput<'_>],
        symbols: &SymbolTable<'_>,
        resolution: &Resolution<'_>,
        handle: usize,
        claimed: &ClaimedFile,
    ) -> FileResolution {
        let ids = resolution.symbol_ids(FileId::new(handle));
        let values = claimed
            .symbols
            .iter()
            .enumerate()
            .map(|(index, symbol)| {
                let undefined = symbol.kind.is_undefined();
                let Some(&id) = ids.get(index) else {
                    return if undefined {
                        SymbolResolution::Undefined
                    } else {
                        SymbolResolution::PreemptedRegular
                    };
                };
                let def = symbols.definition(id);
                let winner = match def.kind {
                    DefinitionKind::Undefined | DefinitionKind::Lazy => Winner::Undefined,
                    DefinitionKind::Shared => Winner::Shared,
                    DefinitionKind::Regular | DefinitionKind::Weak | DefinitionKind::Common => {
                        if files.get(def.file.index()).is_some_and(|f| f.ir.is_some()) {
                            Winner::Ir
                        } else {
                            Winner::Regular
                        }
                    }
                };
                let prevailing = !undefined
                    && winner == Winner::Ir
                    && def.file.index() == handle
                    && def.index as usize == index;
                let flags = symbols.flags(id);
                let name = symbols.name(id);
                let script_local = context.shared
                    && name.version().is_none()
                    && context
                        .script
                        .as_ref()
                        .and_then(|script| script.lookup(name.bytes()))
                        .is_some_and(|(_, local)| local);
                let listed = name.version().is_none()
                    && context.patterns.iter().any(|p| p.matches(name.bytes()));
                symbol_resolution(&SymbolFacts {
                    undefined,
                    prevailing,
                    winner,
                    regular_ref: flags.contains(LTO_REGULAR),
                    dynamic_ref: flags.contains(LTO_DYNAMIC),
                    export_all: context.export_all,
                    listed,
                    visibility: if flags.contains(LTO_HIDDEN) {
                        Visibility::Hidden
                    } else {
                        symbol.visibility
                    },
                    script_local,
                    relocatable: context.relocatable,
                    wrapped: context.wrapped(name.bytes()) || context.wrapped(&symbol.name),
                })
            })
            .collect();
        FileResolution::Included(values)
    }

    /// [`super::resolve`] for links with plugins.
    pub(super) fn resolve<'a>(
        options: &LinkOptions,
        diagnostics: &dyn DiagnosticSink,
        rules: &ElfRules,
        inputs: &mut Inputs<'a>,
    ) -> Result<(SymbolTable<'a>, Resolution<'a>, LtoLink)> {
        let mut driver = Driver {
            options,
            diagnostics,
            session: None,
            claims: 0,
            claim_time: Duration::ZERO,
        };
        // `LinkOptions::timing`, as in the driver: where LTO links spend
        // their time.
        let timing = options.timing.as_ref();
        let start = Instant::now();
        let lap = |what: &str| {
            if let Some(timing) = timing {
                timing.write_line(&format!(
                    "qld: lto {what}: {:.1} ms",
                    start.elapsed().as_secs_f64() * 1000.0
                ));
            }
        };

        // Members the archive index does not describe, in input order.
        for index in 0..inputs.files.len() {
            let Some(file) = inputs.files.get(index) else {
                break;
            };
            if !file.needs_claim() || file.file.is_none() {
                continue;
            }
            let claimed = driver.claim(file, index, false)?;
            let Some(file) = inputs.files.get_mut(index) else {
                break;
            };
            match claimed {
                Some(symbols) => file.lazy_names = symbols.defined_names(),
                None => file.use_native_lazy_names()?,
            }
        }

        let mut symbols = SymbolTable::new();
        let resolution = {
            let mut hook = ClaimHook {
                driver: &mut driver,
                comdat: ComdatHook::default(),
            };
            resolve_symbols_with(&mut symbols, rules, &mut inputs.files, &mut hook)?
        };
        let Some(mut session) = driver.session.take() else {
            return Ok((symbols, resolution, LtoLink::default()));
        };
        if let Some(timing) = timing {
            timing.write_line(&format!(
                "qld: lto claims ({} files, plugin loading included): {:.1} ms",
                driver.claims,
                driver.claim_time.as_secs_f64() * 1000.0
            ));
        }
        lap("first resolution");
        if !inputs.files.iter().any(|file| file.ir.is_some()) {
            // Only unextracted members were claimed: nothing to compile.
            return Ok((
                symbols,
                resolution,
                LtoLink {
                    session: Some(session),
                },
            ));
        }

        if options.kind != crate::args::OutputKind::Relocatable {
            // What the driver does after resolution: weak references bind to
            // a shared definition over an unextracted member, and symbols
            // only an unneeded --as-needed library defines are unbound, so
            // references to them are reported undefined.
            dso::bind_unextracted(&inputs.files, &symbols, &resolution);
            let kept = needed_by_ir(&inputs.files, &symbols, &resolution);
            let _ = dso::plan_needed_with(&inputs.files, &symbols, rules, &resolution, &kept);
        }
        mark_usage(&inputs.files, &symbols, &resolution);
        let context = Context::new(options, &inputs.files)?;
        let files = &inputs.files;
        let mut claim = 0usize;
        let output = session.all_symbols_read(
            |claimed| {
                let this = claim;
                claim = claim.saturating_add(1);
                let handle = usize::try_from(claimed.handle).unwrap_or(usize::MAX);
                let current = files
                    .get(handle)
                    .and_then(|file| file.ir.as_ref())
                    .is_some_and(|ir| ir.claim == this);
                if !current || !resolution.is_live(FileId::new(handle)) {
                    return FileResolution::NotIncluded;
                }
                file_resolution(&context, files, &symbols, &resolution, handle, claimed)
            },
            diagnostics,
        )?;
        lap("code generation");
        if output.errors > 0 {
            return Err(Error::Reported {
                errors: output.errors,
            });
        }

        // The second resolution, with the generated objects.
        let table = inputs
            .files
            .first()
            .ok_or_else(|| Error::Internal("no internal input file".into()))?
            .table();
        let objects = output
            .files
            .iter()
            .map(|path| table.load_path(path))
            .collect::<Result<Vec<_>>>()?;
        let live: Vec<bool> = (0..inputs.files.len())
            .map(|index| resolution.is_live(FileId::new(index)))
            .collect();
        drop(resolution);
        drop(symbols);

        let old = std::mem::take(&mut inputs.files);
        let mut files: Vec<ElfInput<'a>> = Vec::with_capacity(old.len());
        let mut others = Vec::new();
        let mut placed = false;
        for (index, mut file) in old.into_iter().enumerate() {
            if file.ir.is_some() {
                if !placed {
                    placed = true;
                    for &id in &objects {
                        let Some(object) = table.get(id) else {
                            continue;
                        };
                        if !matches!(object.format(), FileFormat::Elf(i) if i.is_relocatable()) {
                            others.push(id);
                            continue;
                        }
                        session.new_input(
                            &InputFile::new(
                                object.path(),
                                0,
                                u64::try_from(object.data().len()).unwrap_or(u64::MAX),
                                u64::try_from(files.len()).unwrap_or(u64::MAX),
                            ),
                            diagnostics,
                        )?;
                        files.push(file.lto_object(object, file.position));
                    }
                }
                continue;
            }
            file.prepare_after_lto(live.get(index).copied().unwrap_or(false));
            files.push(file);
        }
        inputs::add_after_lto(
            &mut files,
            options,
            &others,
            &output.libraries,
            &output.library_paths,
        )?;
        inputs.files = files;

        let mut symbols = SymbolTable::new();
        let mut comdat = ComdatHook::default();
        let resolution = resolve_symbols_with(&mut symbols, rules, &mut inputs.files, &mut comdat)?;
        lap("second resolution");
        Ok((
            symbols,
            resolution,
            LtoLink {
                session: Some(session),
            },
        ))
    }
}
