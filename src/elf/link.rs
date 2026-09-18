//! The ELF link driver: runs the pipeline in `docs/architecture.md` for an ELF
//! target, from resolved options to a written output file.
//!
//! Each stage lives in its own module; this file only sequences them and
//! turns problems into diagnostics:
//!
//! 1. [`inputs`]: search paths, archives, input scripts; then the thread
//!    pool is sized from the input size unless `--threads` was given (in a
//!    pool of more than 16 threads, every stage but the relocation scan and
//!    section merging runs in a pool of 16: see `Narrow`);
//! 2. [`resolve_symbols_with`] with [`ElfRules`], claiming COMDAT groups as
//!    rounds load files ([`resolve::ComdatHook`]), then dropping the
//!    discarded copies' sections;
//! 3. [`place`]: output section assignment;
//! 4. linker-defined symbols ([`defined`]);
//! 5. `.eh_frame` splitting, `--gc-sections` and `--why-live` ([`gc`]);
//! 6. the relocation scan ([`scan`]) and undefined symbols;
//! 7. common symbols, merged sections, `--icf` ([`icf`]), live `.eh_frame`
//!    records;
//! 8. synthetic sections and the symbol table plan;
//! 9. [`layout`], symbol addresses, [`write`](mod@write), and the link map
//!    ([`map`]).
//!
//! Relocatable output (`-r`) leaves after step 2: optional `--gc-sections`
//! (which then needs `-e` or `-u` roots), then [`relocatable`].

use std::time::Instant;

use rayon::prelude::*;

use crate::args::{LinkOptions, OutputKind, StripMode};
use crate::debug::tombstone::{Style as TombstoneStyle, Tombstones};
use crate::diag::{Diagnostic, DiagnosticSink};
use crate::error::{Error, Result};
use crate::input::FileTable;
use crate::passes::IcfMode;
#[cfg(doc)]
use crate::symbols::resolve_symbols_with;
use crate::symbols::{SymbolName, SymbolTable};

use super::common;
use super::defined;
use super::dso;
use super::dynsym;
use super::ehframe;
use super::export::{self, Mode};
use super::gc;
use super::icf;
use super::inputs::{self, InternalNames, parse_number};
use super::layout::{self, LayoutInput, TrailerSizes};
use super::map;
use super::merge;
use super::object::{ParseConfig, SectionKind, WrapTable};
use super::place;
use super::refs::{Def, Refs};
use super::reloc;
use super::relocatable;
use super::resolve::{self, ElfRules};
use super::rules::RuleSet;
use super::scan::{self, UndefinedRef};
use super::sections::Sections;
use super::symtab;
use super::synth::{self, Synth};
use super::values::Addresses;
use super::write::{self, WriteInput};
use super::xref;

/// At most this many references are listed per undefined symbol.
const MAX_REFERENCES: usize = 3;

/// Links an ELF output described by `options`.
///
/// Called by [`crate::link`] inside a rayon pool sized by `--threads`.
///
/// # Errors
///
/// Returns [`Error::Unimplemented`] for output kinds of later milestones,
/// [`Error::Reported`] when errors were reported to `diagnostics`, and any
/// I/O or parse error.
pub fn link(options: &LinkOptions, diagnostics: &dyn DiagnosticSink) -> Result<()> {
    // `-plugin` needs no check here: compiler drivers always pass it, and
    // an IR input is reported as Unimplemented (M6) when it is loaded.
    let prepared = super::script_layout::prepare(options)?;
    let options = &prepared.options;
    check_supported(options)?;
    let timing = std::env::var_os("QLD_TIMING").is_some();
    let start = Instant::now();
    let lap = |what: &str| {
        if timing {
            eprintln!(
                "qld: {what}: {:.1} ms",
                start.elapsed().as_secs_f64() * 1000.0
            );
        }
    };

    let wrap = WrapTable::new(&options.wrap);
    let mut internal = InternalNames::new(options);
    prepared.add_internal_names(&mut internal.names);
    let script = prepared.script.as_ref();
    let table = FileTable::new();
    let config = ParseConfig {
        strip_debug: options.strip >= StripMode::Debug,
        wrap: &wrap,
        table: &table,
    };
    // Without `--threads` and outside a caller's pool, the link runs in
    // pools of its own and never starts rayon's global pool (one thread per
    // core): mapping the inputs gains nothing from more than a few threads,
    // and 849 objects took 50 ms on 64 threads against 10 ms on 16.
    let own_pools = options.threads.is_none() && rayon::current_thread_index().is_none();
    // In a larger pool (`--threads` above MAX_DEFAULT_THREADS, or a
    // caller's pool), the stages that get slower with more threads run in a
    // pool of MAX_DEFAULT_THREADS; see `Narrow`.
    let mut narrow = Narrow {
        pool: (!own_pools && rayon::current_num_threads() > MAX_DEFAULT_THREADS)
            .then(|| thread_pool(MAX_DEFAULT_THREADS))
            .transpose()?,
        widen: None,
    };
    let mut inputs = if own_pools {
        let threads = available_threads().min(INPUT_THREADS);
        thread_pool(threads)?.install(|| inputs::collect(options, &table, &internal, config))?
    } else {
        narrow.run(|| inputs::collect(options, &table, &internal, config))?
    };
    lap("inputs");

    let threads = input_sized_threads(options, &table, own_pools);
    if own_pools
        && threads == Some(MAX_DEFAULT_THREADS)
        && available_threads() > MAX_DEFAULT_THREADS
    {
        narrow.widen = Some(available_threads());
    }
    match threads {
        Some(threads) => thread_pool(threads)?.install(|| {
            link_inputs(
                options,
                diagnostics,
                &mut inputs,
                &internal,
                script,
                &lap,
                &narrow,
            )
        }),
        None => link_inputs(
            options,
            diagnostics,
            &mut inputs,
            &internal,
            script,
            &lap,
            &narrow,
        ),
    }
}

/// A pool of [`MAX_DEFAULT_THREADS`] threads for the stages that do not
/// scale past it, when the link runs in a larger pool (an explicit
/// `--threads`, or a caller's pool); `None` otherwise.
///
/// Past 16 threads, most stages get slower on the 32-core development
/// machine, not faster: idle rayon workers spin looking for work between
/// the many short parallel steps, share cores with the busy ones, and page
/// faults and `mmap` contend in the kernel. Measured at 16 and 64 threads
/// (clang, `libclang-cpp`, clang with debug information): mapping the
/// inputs took 5 ms against 28-53 ms (1,014 objects), resolution 50
/// against 62 ms, the dynamic symbol plan 15 against 24 ms, layout 9
/// against 15 ms, and the write of a 1.2 GiB output 353 against 451 ms.
/// The relocation scan and section merging still gain from the larger pool
/// (merging clang's 18 million debug strings: 182 ms on 16 threads, 153 on
/// 64), so they keep it. With this, `--threads=64` links as fast as 16
/// threads or faster: clang 155 ms against 203 before, clang with debug
/// information 670 against 771, `libclang-cpp` 121 against 189.
///
/// The other way round, a large link in qld's own pool of 16 threads merges
/// sections in a pool of one thread per core when there are many pieces
/// (`widen`, see [`merge::merge`]).
struct Narrow {
    pool: Option<rayon::ThreadPool>,
    /// Threads for section merging, when they are more than the link's.
    widen: Option<usize>,
}

impl Narrow {
    /// Runs `op` in the narrow pool, if there is one.
    fn run<R: Send>(&self, op: impl FnOnce() -> R + Send) -> R {
        match &self.pool {
            Some(pool) => pool.install(op),
            None => op(),
        }
    }
}

/// Threads used to map and index the inputs when `--threads` is not given.
const INPUT_THREADS: usize = 16;

/// The number of threads this process may run: the available parallelism.
fn available_threads() -> usize {
    std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get)
}

/// A rayon pool of `threads` threads.
fn thread_pool(threads: usize) -> Result<rayon::ThreadPool> {
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build()
        .map_err(|e| Error::Internal(format!("cannot create thread pool: {e}")))
}

/// Rejects options whose effect is not implemented yet, rather than
/// silently producing a different binary.
fn check_supported(options: &LinkOptions) -> Result<()> {
    let unimplemented = |what: &str, milestone: &str| {
        Err(Error::Unimplemented(format!(
            "{what} (roadmap {milestone})"
        )))
    };
    if let Some(format) = &options.output_format
        && !matches!(
            format.as_str(),
            "elf64-x86-64" | "elf64-x86_64" | "binary" | "ihex" | "srec"
        )
    {
        return unimplemented(&format!("--oformat {format}"), "M4");
    }
    if options.kind == OutputKind::Relocatable
        && options
            .compress_debug_sections
            .as_deref()
            .is_some_and(|c| c != "none")
    {
        return unimplemented("--compress-debug-sections with -r", "M5");
    }
    for (name, expr) in &options.defsym {
        if inputs::parse_defsym(expr).is_none() {
            return unimplemented(
                &format!("--defsym {name}={expr}: expressions beyond `symbol+offset`"),
                "M3",
            );
        }
    }
    Ok(())
}

/// Input bytes per worker thread when `--threads` is not given.
const BYTES_PER_THREAD: u64 = 4 << 20;
/// Most threads used when `--threads` is not given.
const MAX_DEFAULT_THREADS: usize = 16;

/// The thread count for a link whose thread count was not set explicitly:
/// one thread per [`BYTES_PER_THREAD`] of input, at most
/// [`MAX_DEFAULT_THREADS`] and the current pool's size (with `own_pools`,
/// the available parallelism). `None` keeps the current pool.
///
/// Small links are dominated by the fixed cost of spreading tiny tasks over
/// many threads (a static "hello world" takes 10 ms on one thread and 28 ms
/// on 64). The output does not depend on the thread count.
fn input_sized_threads(options: &LinkOptions, table: &FileTable, own_pools: bool) -> Option<usize> {
    if options.threads.is_some() {
        return None;
    }
    let top_level = || table.iter().filter(|(_, file)| file.parent().is_none());
    let mut bytes: u64 = top_level()
        .map(|(_, file)| u64::try_from(file.data().len()).unwrap_or(u64::MAX))
        .fold(0u64, u64::saturating_add);
    // Compressed debug sections count at their uncompressed size: inflating
    // them and writing them out is the work (256 objects whose 320 MiB of
    // debug information compress to under a megabyte linked on one thread).
    // Only links that would get fewer than the most threads look, and only
    // at section headers.
    let most = BYTES_PER_THREAD.saturating_mul(MAX_DEFAULT_THREADS as u64);
    if bytes < most {
        for (_, file) in top_level() {
            bytes = bytes.saturating_add(compressed_growth(file));
            if bytes >= most {
                break;
            }
        }
    }
    let wanted = usize::try_from(bytes.div_ceil(BYTES_PER_THREAD))
        .unwrap_or(usize::MAX)
        .clamp(1, MAX_DEFAULT_THREADS);
    if own_pools {
        return Some(wanted.min(available_threads()));
    }
    (wanted < rayon::current_num_threads()).then_some(wanted)
}

/// How many bytes the compressed sections of a relocatable ELF input grow
/// by when inflated (0 for anything else, and for malformed files, which
/// parsing reports later).
fn compressed_growth(file: &crate::input::InputFile) -> u64 {
    use crate::elf::read::{Elf64Le, ObjectFile, Source};
    use crate::input::FileFormat;
    if !matches!(file.format(), FileFormat::Elf(ident) if ident.is_relocatable()) {
        return 0;
    }
    let Ok(object) = ObjectFile::<Elf64Le>::parse(file.data(), Source::new(file.path())) else {
        return 0;
    };
    object
        .elf()
        .sections()
        .iter()
        .filter(|header| header.is_compressed())
        .filter_map(|header| {
            let (chdr, _) = object.compressed_data(&header).ok()??;
            Some(chdr.ch_size.saturating_sub(header.sh_size))
        })
        .fold(0u64, u64::saturating_add)
}

/// Everything after input collection, run in the link's thread pool.
#[allow(clippy::too_many_lines)]
fn link_inputs<'a>(
    options: &LinkOptions,
    diagnostics: &'a dyn DiagnosticSink,
    inputs: &mut inputs::Inputs<'a>,
    internal: &InternalNames,
    script: Option<&'a super::script_layout::LayoutScript>,
    lap: &(dyn Fn(&str) + Sync),
    narrow: &Narrow,
) -> Result<()> {
    let rules = ElfRules {
        allow_multiple_definition: options.allow_multiple_definition,
    };
    // Resolution, and LTO when a plugin claims IR inputs (`lto` module).
    let (mut symbols, resolution, lto) =
        narrow.run(|| super::lto::resolve(options, diagnostics, &rules, inputs))?;
    let files = &inputs.files;
    narrow.run(|| dso::bind_unextracted(files, &symbols, &resolution));
    lap("resolution");

    let mut sections = narrow.run(|| Sections::new(files, &resolution))?;
    let relocatable = options.kind == OutputKind::Relocatable;
    if relocatable {
        relocatable::revive_sections(files, &mut sections, options);
    } else if options.emit_relocs {
        // GNU ld keeps `.note.GNU-stack` as an output section with -q.
        relocatable::revive_named(files, &mut sections, b".note.GNU-stack");
    }
    if options
        .output_format
        .as_deref()
        .is_some_and(|f| super::rawout::Format::from_name(f).is_some())
    {
        // Raw formats link through BFD's generic linker, which copies
        // property notes rather than merging them.
        relocatable::revive_named(files, &mut sections, b".note.gnu.property");
    }
    let (mut errors, cref) = narrow.run(|| {
        resolve::deduplicate_comdat(files, &mut sections);
        let errors = resolve::report_duplicates(
            files,
            &resolution,
            &sections,
            options.demangle,
            diagnostics,
        );
        report_gnu_warnings(files, &symbols, diagnostics);
        xref::trace_symbols(files, &resolution, options, diagnostics);
        xref::warn_common(files, &resolution, options, diagnostics);
        let cref = xref::cross_reference(files, &symbols, &resolution, options);
        (errors, cref)
    });

    if relocatable {
        if errors > 0 && !options.noinhibit_exec {
            return Err(Error::Reported { errors });
        }
        link_relocatable(
            options,
            diagnostics,
            files,
            &symbols,
            &resolution,
            sections,
            internal,
            lap,
        )?;
        map::write_cref(options, cref.as_deref())?;
        lto.finish(diagnostics)?;
        options.output_complete();
        return Ok(());
    }

    let needed = narrow.run(|| dso::plan_needed(files, &symbols, &rules, &resolution));
    let mode = Mode::new(options, files.iter().any(|f| f.shared.is_some()));
    if mode.dynamic && !mode.shared {
        narrow.run(|| dso::mark_dependency_symbols(files, &symbols, &needed, options));
    }
    let always: &[&str] = if mode.dynamic && mode.executable() && options.export_dynamic {
        for name in defined::ALWAYS_DEFINED {
            symbols.intern(SymbolName::new(name.as_bytes()));
        }
        defined::ALWAYS_DEFINED
    } else {
        &[]
    };

    let rule_set = RuleSet::for_link(script, diagnostics);
    let mut placement = narrow.run(|| place::place(&rule_set, files, &sections, options));
    for id in &placement.discarded {
        if let Some(slot) = sections.live.get_mut(id.index()) {
            *slot = false;
        }
    }
    let linker = narrow
        .run(|| defined::register(&symbols, files, &placement, options, mode.dynamic, always));
    let (mut version_script, dynamic_patterns) = export::read_scripts(options)?;
    if version_script.is_none()
        && let Some(nodes) = script.map(|s| &s.version).filter(|v| !v.is_empty())
    {
        version_script = Some(export::VersionScript::new(nodes)?);
    }
    let exports = narrow.run(|| {
        export::plan(
            files,
            &symbols,
            &resolution,
            &needed,
            options,
            mode,
            version_script,
            &dynamic_patterns,
            &linker,
        )
    })?;
    lap("placement");

    let mut eh_frames = narrow.run(|| ehframe::split(files, &sections))?;
    if options.gc_sections {
        let refs = Refs {
            files,
            symbols: &symbols,
            resolution: &resolution,
            sections: &sections,
        };
        let why_live = !options.why_live.is_empty();
        let (removed, graph) = narrow
            .run(|| gc::collect(&refs, &placement, &eh_frames, &linker, internal, why_live))?;
        if options.print_gc_sections {
            gc::print_removed(&refs, &removed, diagnostics);
        }
        if let Some(graph) = &graph {
            gc::report_why_live(&refs, graph, &options.why_live, diagnostics);
        }
        for id in &removed {
            if let Some(slot) = sections.live.get_mut(id.index()) {
                *slot = false;
            }
        }
        eh_frames
            .sections
            .retain(|s| sections.live.get(s.id.index()).copied().unwrap_or(false));
        narrow.run(|| placement.compute_flags(files, &sections));
        lap("gc");
    }

    let refs = Refs {
        files,
        symbols: &symbols,
        resolution: &resolution,
        sections: &sections,
    };
    let context = reloc::Context {
        mode,
        relax: options.relax,
        copy_relocs: options.copy_relocs,
        arch: super::arch::Arch::of(options, files),
    };
    // Section merging needs neither the scan's results nor anything it
    // changes (symbol flags), and neither stage keeps every thread busy on
    // its own: they run side by side. A merge error counts only if the scan
    // reports none, as when the merge ran after it.
    let (scan, merged) = rayon::join(
        || scan::scan(&refs, &context),
        || {
            merge::merge(
                files,
                &sections,
                &placement,
                options.optimize >= 2,
                narrow.widen,
            )
        },
    );
    for file in &scan.files {
        for error in &file.errors {
            diagnostics.emit(error.clone());
            errors = errors.saturating_add(1);
        }
    }
    errors = errors.saturating_add(report_undefined(
        &refs,
        &scan,
        options,
        mode,
        &needed,
        diagnostics,
    ));
    errors = errors.saturating_add(dso::check_shlib_undefined(
        files,
        &symbols,
        &resolution,
        &needed,
        options,
        diagnostics,
    ));
    if scan.text_relocs() && mode.dynamic {
        let message = if mode.shared {
            "creating DT_TEXTREL in a shared object"
        } else {
            "creating DT_TEXTREL in a PIE"
        };
        if options.error_textrel {
            diagnostics.emit(Diagnostic::error(message.to_string()));
            errors = errors.saturating_add(1);
        } else if options.warn_textrel || mode.pic {
            diagnostics.emit(Diagnostic::warning(message.to_string()));
        }
    }
    if errors > 0 && !options.noinhibit_exec {
        return Err(Error::Reported { errors });
    }
    lap("scan");

    let commons = narrow.run(|| common::allocate(&refs));
    let merged = merged?;
    lap("merge");
    let icf_mode = match options.icf.as_deref() {
        Some("all") => Some(IcfMode::All),
        Some("safe") => Some(IcfMode::Safe),
        _ => None,
    };
    if let Some(icf_mode) = icf_mode {
        let fold_into = icf::fold(
            &refs,
            &placement,
            &merged,
            icf_mode,
            options.print_icf_sections,
            diagnostics,
        )?;
        sections.apply_folding(fold_into);
        lap("icf");
    }
    let refs = Refs {
        files,
        symbols: &symbols,
        resolution: &resolution,
        sections: &sections,
    };
    narrow.run(|| eh_frames.finalize(&refs));

    let mut synth = Synth {
        arch: context.arch,
        ..Synth::default()
    };
    narrow.run(|| synth.plan_entries(&refs, &scan, mode));
    // DT_RELR is for position-independent output; GNU ld ignores the
    // option otherwise.
    synth.relr = options.pack_relative_relocs && mode.pic;
    synth.relr_size = synth
        .relr_count()
        .div_ceil(32)
        .saturating_add(8)
        .saturating_mul(8);
    synth.ibt = synth::plan_ibt(files, options);
    synth.build_id = synth::plan_build_id(options);
    synth.property_note = synth::plan_property_note(files, options);
    synth.interp = synth::plan_interp(options, mode, context.arch);
    synth.fde_count = u64::try_from(eh_frames.live_fdes()).unwrap_or(0);
    synth.eh_frame_hdr = options.eh_frame_hdr && synth.fde_count > 0;
    synth.eh_frame_end = eh_frames.sections.iter().any(|s| s.size > 0);
    synth.common = (commons.size, commons.align);

    let nonempty_outputs = narrow.run(|| nonempty_outputs(files, &sections, &placement));
    let has_output = |name: &[u8]| {
        placement
            .outputs
            .iter()
            .zip(&nonempty_outputs)
            .any(|(output, &nonempty)| nonempty && output.name == name)
    };
    let soname = options
        .soname
        .as_ref()
        .map(|s| s.as_bytes().to_vec())
        .or_else(|| {
            options
                .output_path()
                .file_name()
                .map(|n| n.as_encoded_bytes().to_vec())
        });
    let dynamic = narrow.run(|| {
        dynsym::plan(&dynsym::PlanInput {
            refs: &refs,
            needed: &needed,
            mode,
            options,
            synth: &synth,
            exports: &exports,
            scan: &scan,
            has_output: &has_output,
            soname,
        })
    })?;
    synth.dynamic_sizes = dynamic.sizes();
    synth.verneed_count = dynamic.verneed_count;
    synth.verdef_count = dynamic.verdef_count;
    lap("dynamic");

    let plan = narrow.run(|| symtab::plan(&refs, &linker, options));
    let trailers = TrailerSizes {
        symtab: plan.symtab_size(),
        strtab: if plan.is_empty() {
            0
        } else {
            u64::try_from(plan.strtab_size).unwrap_or(u64::MAX)
        },
        first_global: u32::try_from(plan.first_global).unwrap_or(0),
    };
    let exec_stack = files
        .iter()
        .filter_map(|f| f.object.as_ref())
        .any(|o| o.exec_stack);
    let mut layout = narrow.run(|| {
        layout::layout(&LayoutInput {
            options,
            refs,
            rules: &rule_set,
            files,
            sections: &sections,
            placement: &placement,
            merged: &merged,
            eh_frames: &eh_frames,
            synth: &synth,
            trailers,
            exec_stack,
            mode,
            compressed: &[],
            relax: None,
        })
    })?;
    // `.relr.dyn`'s size depends on the addresses it encodes: lay out with an
    // estimate, and again with the real size while it does not fit (growth
    // rarely moves anything, as the next segment starts on a page boundary).
    let mut relr = Vec::new();
    if synth.relr_count() > 0 {
        // Shrinking to the exact size is tried once; after that a smaller
        // encoding is padded with empty bitmaps.
        let mut shrunk = false;
        for attempt in 0..8 {
            let addresses = Addresses::new(
                refs, &layout, &merged, &eh_frames, &synth, &commons, &placement, &linker, options,
            );
            let places = write::relr_addresses(&addresses, &context, &dynamic, &scan);
            relr = write::encode_relr(&places);
            let size = u64::try_from(relr.len())
                .unwrap_or(u64::MAX)
                .saturating_mul(8);
            if size == synth.relr_size || (size < synth.relr_size && shrunk) {
                break;
            }
            if attempt == 7 {
                if size <= synth.relr_size {
                    break;
                }
                return Err(Error::Internal(".relr.dyn size did not converge".into()));
            }
            shrunk |= size < synth.relr_size;
            synth.relr_size = size;
            layout = layout::layout(&LayoutInput {
                options,
                refs,
                rules: &rule_set,
                files,
                sections: &sections,
                placement: &placement,
                merged: &merged,
                eh_frames: &eh_frames,
                synth: &synth,
                trailers,
                exec_stack,
                mode,
                compressed: &[],
                relax: None,
            })?;
        }
    }
    lap("layout");

    let mut plan = plan;
    plan.add_section_symbols(layout.section_symbols as usize);
    let tombstones = Tombstones::new(TombstoneStyle::Lld)
        .with_rules(
            options
                .dead_reloc_in_nonalloc
                .iter()
                .map(|(glob, value)| (glob.as_bytes(), *value)),
        )
        .map_err(|e| Error::Option(e.0))?;

    // --compress-debug-sections: render and compress the debug sections
    // with the final addresses, then lay out again with their new sizes
    // (they follow every allocated section, so no address moves).
    let compression = options
        .compress_debug_sections
        .as_deref()
        .and_then(|value| {
            let level = if options.optimize >= 2 {
                crate::debug::compress::deflate::Level::DEFAULT
            } else {
                crate::debug::compress::deflate::Level::FASTEST
            };
            crate::debug::section::OutputCompression::from_option(value, level)
        });
    let mut prerendered = Vec::new();
    if let Some(compression) = compression {
        let addresses = Addresses::new(
            refs, &layout, &merged, &eh_frames, &synth, &commons, &placement, &linker, options,
        );
        prerendered = write::prerender_debug_sections(
            &WriteInput {
                options,
                addresses: &addresses,
                symtab: &plan,
                linker: &linker,
                dynamic: &dynamic,
                scan: &scan,
                context,
                tombstones: &tombstones,
                relr: &relr,
                entry: 0,
                prerendered: &[],
                diagnostics,
            },
            compression,
        )?;
        let sizes: Vec<layout::CompressedOutput> = prerendered
            .iter()
            .filter(|p| p.compressed)
            .filter_map(|p| {
                let section = layout.sections.get(p.position as usize)?;
                Some(layout::CompressedOutput {
                    output: section.output,
                    size: u64::try_from(p.bytes.len()).ok()?,
                    gnu: !compression.is_gabi(),
                })
            })
            .collect();
        drop(addresses);
        if !sizes.is_empty() {
            layout = layout::layout(&LayoutInput {
                options,
                refs,
                rules: &rule_set,
                files,
                sections: &sections,
                placement: &placement,
                merged: &merged,
                eh_frames: &eh_frames,
                synth: &synth,
                trailers,
                exec_stack,
                mode,
                compressed: &sizes,
                relax: None,
            })?;
        }
        lap("compress");
    }

    let addresses = narrow.run(|| {
        Addresses::new(
            refs, &layout, &merged, &eh_frames, &synth, &commons, &placement, &linker, options,
        )
    });
    let entry = entry_address(&addresses, options, mode, diagnostics);
    narrow.run(|| {
        write::write(&WriteInput {
            options,
            addresses: &addresses,
            symtab: &plan,
            linker: &linker,
            dynamic: &dynamic,
            scan: &scan,
            context,
            tombstones: &tombstones,
            relr: &relr,
            entry,
            prerendered: &prerendered,
            diagnostics,
        })
    })?;
    narrow.run(|| map::write(options, &addresses, &plan, cref.as_deref()))?;
    lap("write");
    lto.finish(diagnostics)?;
    // Freeing the link's data and unmapping the inputs come after this.
    options.output_complete();
    Ok(())
}

/// The rest of a relocatable (`-r`) link: `--gc-sections` when asked (GNU
/// ld requires `-e` or `-u` roots for it), then [`relocatable::write`].
#[allow(clippy::too_many_arguments)]
fn link_relocatable<'a>(
    options: &LinkOptions,
    diagnostics: &dyn DiagnosticSink,
    files: &[inputs::ElfInput<'a>],
    symbols: &SymbolTable<'a>,
    resolution: &crate::symbols::Resolution<'a>,
    mut sections: Sections,
    internal: &InternalNames,
    lap: &(dyn Fn(&str) + Sync),
) -> Result<()> {
    if options.gc_sections {
        if options.entry.is_none() && options.undefined.is_empty() {
            return Err(Error::Option(
                "--gc-sections requires a defined symbol root specified by -e or -u".into(),
            ));
        }
        let rule_set = RuleSet::default_rules();
        let placement = place::place(&rule_set, files, &sections, options);
        let eh_frames = ehframe::split(files, &sections)?;
        let refs = Refs {
            files,
            symbols,
            resolution,
            sections: &sections,
        };
        let linker = defined::LinkerSymbols::default();
        let why_live = !options.why_live.is_empty();
        let (removed, graph) =
            gc::collect(&refs, &placement, &eh_frames, &linker, internal, why_live)?;
        if options.print_gc_sections {
            gc::print_removed(&refs, &removed, diagnostics);
        }
        if let Some(graph) = &graph {
            gc::report_why_live(&refs, graph, &options.why_live, diagnostics);
        }
        for &id in &removed {
            // Sections only relocatable output copies (`.note.GNU-stack`,
            // non-allocated `SHF_EXCLUDE` sections) are not layout rules'
            // roots, but GNU ld keeps them.
            let consumed = sections
                .locate(id)
                .and_then(|(file, index)| files.get(file)?.object.as_ref()?.section(index))
                .is_some_and(|s| s.kind == SectionKind::Ignored && !s.is_alloc());
            if !consumed && let Some(slot) = sections.live.get_mut(id.index()) {
                *slot = false;
            }
        }
        lap("gc");
    }
    let refs = Refs {
        files,
        symbols,
        resolution,
        sections: &sections,
    };
    let commons = options.define_common.then(|| common::allocate(&refs));
    relocatable::write(&relocatable::RelocatableInput {
        options,
        refs,
        commons: commons.as_ref(),
    })?;
    lap("write");
    Ok(())
}

/// Whether each placement output receives a non-empty live input section.
fn nonempty_outputs(
    files: &[super::inputs::ElfInput<'_>],
    sections: &Sections,
    placement: &place::Placement<'_>,
) -> Vec<bool> {
    let outputs = placement.outputs.len();
    // Per file in parallel (a bit set per file would be as large as the
    // output list, so each file lists what it fills), then combined.
    let filled: Vec<Vec<u32>> = files
        .par_iter()
        .enumerate()
        .map(|(file_index, file)| {
            let mut filled = Vec::new();
            let Some(object) = &file.object else {
                return filled;
            };
            for (index, section) in object.sections.iter().enumerate() {
                let Some(id) = sections.id(file_index, u32::try_from(index).unwrap_or(u32::MAX))
                else {
                    continue;
                };
                if section.header.sh_size == 0 || !sections.is_live(id) {
                    continue;
                }
                if let Some(output) = placement.output_of(id)
                    && filled.last() != Some(&output)
                {
                    filled.push(output);
                }
            }
            filled
        })
        .collect();
    let mut nonempty = vec![false; outputs];
    for output in filled.into_iter().flatten() {
        if let Some(slot) = nonempty.get_mut(output as usize) {
            *slot = true;
        }
    }
    nonempty
}

/// A symbol name for diagnostics: demangled when `demangle` is set, with
/// its `@VERSION`.
fn symbol_display(name: SymbolName<'_>, demangle: bool) -> String {
    let base = crate::hints::display_symbol(name.bytes(), demangle);
    match name.version() {
        Some(version) => format!("{base}@{}", String::from_utf8_lossy(version)),
        None => base.into_owned(),
    }
}

/// Library and near-miss hints ([`crate::hints`]) for each group of
/// undefined references. Runs only when a link has undefined symbols.
fn undefined_hints(
    refs: &Refs<'_, '_>,
    groups: &[&[UndefinedRef]],
    options: &LinkOptions,
    needed: &dso::Needed,
) -> Vec<Vec<crate::hints::Hint>> {
    use crate::hints::{Hinter, LinkedLibrary, SearchScope, Undefined};
    let mut linked: Vec<LinkedLibrary> = Vec::new();
    for (index, file) in refs.files.iter().enumerate() {
        let library = match file.role {
            inputs::InputRole::Shared => LinkedLibrary {
                path: file.path(),
                dropped_as_needed: !needed.is_needed(index),
                static_only: false,
            },
            inputs::InputRole::Member => LinkedLibrary::new(file.path()),
            _ => continue,
        };
        if !linked.iter().any(|l| l.path == library.path) {
            linked.push(library);
        }
    }
    let undefined: Vec<Undefined<'_>> = groups
        .iter()
        .filter_map(|g| g.first())
        .map(|r| {
            let name = refs.symbols.name(r.symbol);
            match name.version() {
                Some(version) => Undefined::versioned(name.bytes(), version),
                None => Undefined::new(name.bytes()),
            }
        })
        .collect();
    let defined: Vec<&[u8]> = refs
        .symbols
        .ids()
        .filter(|&id| {
            matches!(
                refs.symbols.definition_kind(id),
                crate::symbols::DefinitionKind::Regular
                    | crate::symbols::DefinitionKind::Weak
                    | crate::symbols::DefinitionKind::Common
                    | crate::symbols::DefinitionKind::Shared
            ) && refs.symbols.name(id).version().is_none()
        })
        .map(|id| refs.symbols.name(id).bytes())
        .collect();
    Hinter::new(SearchScope::from_options(options), linked).hints(&undefined, &defined)
}

/// Reports undefined symbols, lld-style. Returns the number of errors.
fn report_undefined(
    refs: &Refs<'_, '_>,
    scan: &scan::ScanResult,
    options: &LinkOptions,
    mode: Mode,
    needed: &dso::Needed,
    diagnostics: &dyn DiagnosticSink,
) -> usize {
    // A shared object may leave symbols for the dynamic linker to find,
    // unless `--no-undefined` or `-z defs`.
    if mode.shared && options.no_undefined != Some(true) {
        return 0;
    }
    let mut all: Vec<UndefinedRef> = scan
        .files
        .iter()
        .flat_map(|f| f.undefined.iter().copied())
        .collect();
    all.sort_unstable_by_key(|r| (r.symbol, r.file, r.section, r.offset));
    let mut errors = 0usize;
    let mut line_tables: std::collections::BTreeMap<
        usize,
        Option<crate::debug::dwarf::LineLookup>,
    > = std::collections::BTreeMap::new();
    let mut groups: Vec<&[UndefinedRef]> = all.chunk_by(|a, b| a.symbol == b.symbol).collect();
    groups.sort_by_key(|group| group.first().map(|r| (r.file, r.section, r.offset)));
    let ignore = matches!(
        options.unresolved_symbols,
        Some(
            crate::args::UnresolvedSymbols::IgnoreAll
                | crate::args::UnresolvedSymbols::IgnoreInObjectFiles
        )
    );
    // Symbols that a library's own dependency defines: that library is
    // missing from the command line.
    let names: Vec<&[u8]> = groups
        .iter()
        .filter_map(|g| g.first())
        .map(|r| refs.symbols.name(r.symbol).bytes())
        .collect();
    let in_dependencies = if names.is_empty() || ignore {
        Vec::new()
    } else {
        dso::defined_in_dependencies(refs.files, needed, options, &names)
    };
    let hints = if names.is_empty() || ignore {
        Vec::new()
    } else {
        undefined_hints(refs, &groups, options, needed)
    };
    for (group_index, group) in groups.into_iter().enumerate() {
        let Some(first) = group.first() else {
            continue;
        };
        let name = refs.symbols.name(first.symbol);
        if ignore
            || options
                .ignore_unresolved_symbols
                .iter()
                .any(|s| s.as_bytes() == name.bytes())
        {
            continue;
        }
        let order = refs.files.get(first.file).map_or(0, |f| f.position.raw());
        let shown = symbol_display(name, options.demangle);
        let mut diagnostic = if options.warn_unresolved_symbols {
            Diagnostic::warning(format!("undefined symbol: {shown}"))
        } else {
            Diagnostic::error(format!("undefined symbol: {shown}"))
        };
        diagnostic = diagnostic.order(order);
        for reference in group.iter().take(MAX_REFERENCES) {
            let mut location =
                scan::location(refs, reference.file, reference.section, reference.offset);
            // The source line, from the object's DWARF line table, parsed
            // once per file and only when an error is reported.
            let lookup = line_tables.entry(reference.file).or_insert_with(|| {
                refs.files
                    .get(reference.file)
                    .and_then(|f| f.object.as_ref())
                    .and_then(|o| crate::debug::dwarf::LineLookup::parse(&o.elf).ok())
            });
            if let Some(lookup) = lookup {
                location.source = lookup.find(reference.section, reference.offset);
            }
            diagnostic = diagnostic.at(location);
        }
        if group.len() > MAX_REFERENCES {
            diagnostic = diagnostic.note(format!(
                "referenced {} more times",
                group.len().saturating_sub(MAX_REFERENCES)
            ));
        }
        if let Some(Some((library, needed_by))) = in_dependencies.get(group_index) {
            diagnostic = diagnostic.note(format!(
                "'{shown}' is defined in {}, which {} needs but which is not in the link \
                 (DSO missing from command line); add it to the command line",
                library.display(),
                needed_by.display()
            ));
        }
        if let Some(hints) = hints.get(group_index) {
            diagnostic = crate::hints::attach(diagnostic, hints, options.demangle);
        }
        diagnostics.emit(diagnostic);
        if !options.warn_unresolved_symbols {
            errors = errors.saturating_add(1);
        }
    }
    for name in &options.require_defined {
        let defined = refs
            .symbols
            .lookup(&SymbolName::new(name.as_bytes()))
            .is_some_and(|id| !matches!(refs.global_target(id, false).def, Def::Undefined { .. }));
        if !defined {
            diagnostics.emit(Diagnostic::error(format!(
                "required symbol '{name}' is not defined"
            )));
            errors = errors.saturating_add(1);
        }
    }
    errors
}

/// Emits the messages of `.gnu.warning.SYM` sections whose symbol is
/// referenced (and of plain `.gnu.warning` sections), as GNU ld does.
fn report_gnu_warnings(
    files: &[super::inputs::ElfInput<'_>],
    symbols: &SymbolTable<'_>,
    diagnostics: &dyn DiagnosticSink,
) {
    for file in files {
        let Some(object) = &file.object else {
            continue;
        };
        for &index in &object.warnings {
            let Some(section) = object.section(index) else {
                continue;
            };
            let symbol = section.name.strip_prefix(b".gnu.warning.");
            let used = match symbol {
                Some(name) => symbols.lookup(&SymbolName::new(name)).is_some_and(|id| {
                    symbols
                        .flags(id)
                        .contains(crate::symbols::SymbolFlags::REFERENCED)
                }),
                None => section.name == b".gnu.warning",
            };
            if !used {
                continue;
            }
            let text = object.elf.section_data(&section.header).unwrap_or_default();
            let text = text.split(|&b| b == 0).next().unwrap_or_default();
            diagnostics.emit(
                Diagnostic::warning(String::from_utf8_lossy(text).into_owned())
                    .order(file.position.raw()),
            );
        }
    }
}

fn entry_address(
    addresses: &Addresses<'_, '_>,
    options: &LinkOptions,
    mode: Mode,
    diagnostics: &dyn DiagnosticSink,
) -> u64 {
    if mode.shared && options.entry.is_none() {
        return 0;
    }
    let name = options.entry.as_deref().unwrap_or("_start");
    if let Some(value) = parse_number(name) {
        return value;
    }
    let found = addresses
        .refs
        .symbols
        .lookup(&SymbolName::new(name.as_bytes()))
        .filter(|&id| {
            !matches!(
                addresses.refs.global_target(id, false).def,
                Def::Undefined { .. }
            )
        })
        .and_then(|id| addresses.globals.get(id.index()).copied());
    match found {
        Some(value) => value,
        None => {
            let text = addresses.layout.by_name(b".text").map_or(0, |s| s.addr);
            diagnostics.emit(Diagnostic::warning(format!(
                "cannot find entry symbol {name}; defaulting to {text:#x}"
            )));
            text
        }
    }
}
