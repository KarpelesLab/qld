//! Reading the scripts of a link before inputs are collected.
//!
//! [`prepare`] walks the command line's inputs once:
//!
//! - every `-T` script (and `--default-script`) is read, with `INCLUDE`s
//!   resolved against the current directory and the `-L` paths;
//! - a text input that is a linker script with commands beyond
//!   `INPUT`/`GROUP` (an *implicit* script) is read the same way;
//! - `-b binary` inputs are wrapped in objects ([`super::super::binary_input`]).
//!
//! The commands that are not about layout take effect on a copy of the
//! options: `ENTRY`, `OUTPUT`, `OUTPUT_FORMAT`, `SEARCH_DIR`, `EXTERN`,
//! `STARTUP`, `FORCE_COMMON_ALLOCATION`. A script's `INPUT`, `GROUP` and `LIB`
//! lists stay at the script's position on the command line, as an in-memory
//! input script the input stage expands. Layout commands go into the
//! [`LayoutScript`].
//!
//! As in GNU ld, a `-T` script replaces the built-in layout unless it uses
//! `INSERT` (or lld's `OVERWRITE_SECTIONS`), while implicit scripts always
//! add to it.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::args::{InputFormat, InputKind, InputSpec, LinkOptions, MagicMode, OutputKind};
use crate::error::{Error, Result};
use crate::input::identify::FileFormat;
use crate::input::source::InputProvider;
use crate::input::{LibraryNaming, SearchContext};
use crate::script::{
    CommandKind, FsReader, InputFile, InputName, Script, ScriptReader, parse_script,
};
use crate::symbols::SymbolUse;

use super::plan::{Builder, LayoutScript};

/// The options and layout plan of a link, after reading its scripts.
#[derive(Debug)]
pub struct Prepared {
    /// The options with script commands applied and scripts replaced by
    /// their input lists.
    pub options: LinkOptions,
    /// The layout plan, when scripts or options call for the script engine.
    pub script: Option<LayoutScript>,
    /// For a relocatable link without `-T`: [`Prepared::script`] with the
    /// built-in x86-64 relocatable layout ([`super::defaults::relocatable_script`])
    /// added, which the driver uses when the inputs are x86-64.
    pub relocatable_default: Option<LayoutScript>,
}

impl Prepared {
    /// Adds the symbols scripts refer to as references of the linker's
    /// internal file, so that archive members defining them are loaded
    /// and garbage collection keeps their sections. Symbols a script
    /// defines itself, and names only read by `PROVIDE`s, are skipped.
    pub fn add_internal_names(&self, names: &mut Vec<(Vec<u8>, SymbolUse)>) {
        let Some(script) = &self.script else {
            return;
        };
        for (name, provide_only) in &script.referenced {
            // GNU ld does not read the right-hand side of a PROVIDE nothing
            // needs, so those names are not references.
            if *provide_only
                || script.defined.contains(name)
                || !script.value_reads.contains(name)
                || names.iter().any(|(n, _)| n == name)
            {
                continue;
            }
            names.push((
                name.clone(),
                SymbolUse::Reference {
                    weak: *provide_only,
                },
            ));
        }
        // Assigned symbols need IDs even when nothing else names them.
        for name in script.defined.iter().chain(&script.provided) {
            if !names.iter().any(|(n, _)| n == name) {
                names.push((name.clone(), SymbolUse::Ignore));
            }
        }
    }
}

/// Whether a command is one the input stage handles by itself.
fn input_only(kind: &CommandKind) -> bool {
    matches!(
        kind,
        CommandKind::Input(_)
            | CommandKind::Group(_)
            | CommandKind::Lib(_)
            | CommandKind::OutputFormat { .. }
            | CommandKind::OutputArch(_)
            | CommandKind::SearchDir(_)
            | CommandKind::Target(_)
    )
}

/// Whether an input path is named like an object, archive or shared
/// library, which are never sniffed for scripts.
fn looks_like_binary_input(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    if name.ends_with(".o") || name.ends_with(".a") || name.ends_with(".lo") {
        return true;
    }
    // `.so` and `.so.N[.M...]`.
    if let Some(at) = name.find(".so") {
        let rest = name.get(at.saturating_add(3)..).unwrap_or("");
        return rest.is_empty()
            || (rest.starts_with('.')
                && rest
                    .get(1..)
                    .is_some_and(|v| v.split('.').all(|p| p.chars().all(|c| c.is_ascii_digit()))));
    }
    false
}

/// The files of a link: [`LinkOptions::input_provider`] first, then the
/// file system.
type Provider = Option<Arc<dyn InputProvider>>;

fn read_file(provider: &Provider, path: &Path) -> Result<Vec<u8>> {
    if let Some(data) = provider.as_ref().and_then(|p| p.read(path)) {
        return Ok(data.to_vec());
    }
    std::fs::read(path).map_err(|e| Error::io(path, e))
}

/// Reads up to 4 KiB of a file to identify it; `None` when unreadable.
fn sniff(provider: &Provider, path: &Path) -> Option<FileFormat> {
    if let Some(data) = provider.as_ref().and_then(|p| p.read(path)) {
        return Some(crate::input::identify(data.get(..4096).unwrap_or(&data)));
    }
    let mut file = std::fs::File::open(path).ok()?;
    let mut head = vec![0u8; 4096];
    let mut filled = 0usize;
    while let Some(rest) = head.get_mut(filled..) {
        if rest.is_empty() {
            break;
        }
        match file.read(rest) {
            Ok(0) | Err(_) => break,
            Ok(n) => filled = filled.saturating_add(n),
        }
    }
    head.truncate(filled);
    Some(crate::input::identify(&head))
}

/// The file names input section descriptions spell without wildcards or
/// `archive:member` syntax, in script order and without duplicates.
fn literal_file_names(script: &Script) -> Vec<Vec<u8>> {
    use crate::script::{OutputSectionCommandKind, SectionsCommandKind};
    let mut names: Vec<Vec<u8>> = Vec::new();
    let mut add = |commands: &[crate::script::OutputSectionCommand]| {
        for command in commands {
            if let OutputSectionCommandKind::Input(description) = &command.kind {
                let pattern = &description.file.pattern;
                let text = pattern.as_bytes();
                if !pattern.is_wildcard()
                    && !text.contains(&b':')
                    && !names.iter().any(|n| n == text)
                {
                    names.push(text.to_vec());
                }
            }
        }
    };
    for command in &script.commands {
        let (CommandKind::Sections(list) | CommandKind::OverwriteSections(list)) = &command.kind
        else {
            continue;
        };
        for item in list {
            match &item.kind {
                SectionsCommandKind::OutputSection(section) => add(&section.commands),
                SectionsCommandKind::Overlay(overlay) => {
                    for section in &overlay.sections {
                        add(&section.commands);
                    }
                }
                _ => {}
            }
        }
    }
    names
}

/// Renders an `INPUT`/`GROUP`/`LIB` list as script text.
fn input_list(keyword: &str, list: &[InputFile], out: &mut Vec<u8>) -> Result<()> {
    out.extend_from_slice(keyword.as_bytes());
    out.extend_from_slice(b" (");
    let mut in_as_needed = false;
    for entry in list {
        if entry.as_needed != in_as_needed {
            out.extend_from_slice(if entry.as_needed {
                b" AS_NEEDED ("
            } else {
                b" )"
            });
            in_as_needed = entry.as_needed;
        }
        match &entry.name {
            InputName::Library(name) => {
                out.extend_from_slice(b" -l");
                out.extend_from_slice(name);
            }
            InputName::Path(path) => {
                if path.contains(&b'"') {
                    return Err(Error::Option(format!(
                        "cannot handle input file name with a quote: {}",
                        String::from_utf8_lossy(path)
                    )));
                }
                out.extend_from_slice(b" \"");
                out.extend_from_slice(path);
                out.push(b'"');
            }
        }
    }
    if in_as_needed {
        out.extend_from_slice(b" )");
    }
    out.extend_from_slice(b" )\n");
    Ok(())
}

/// Reads `INCLUDE`d scripts from the link's input provider, then from the
/// file system.
struct IncludeReader {
    provider: Provider,
    files: FsReader,
}

impl ScriptReader for IncludeReader {
    fn read_include(&mut self, name: &[u8], from: &Path) -> std::io::Result<(PathBuf, Vec<u8>)> {
        if let Some(provider) = &self.provider {
            let name = PathBuf::from(String::from_utf8_lossy(name).into_owned());
            let mut candidates = vec![name.clone()];
            if name.is_relative() {
                candidates.extend(self.files.search_dirs.iter().map(|dir| dir.join(&name)));
            }
            for candidate in candidates {
                if let Some(data) = provider.read(&candidate) {
                    return Ok((candidate, data.to_vec()));
                }
            }
        }
        self.files.read_include(name, from)
    }
}

struct Loader<'o> {
    options: &'o LinkOptions,
    out: LinkOptions,
    scripts: Vec<(Script, bool)>,
    /// `ENTRY` from the scripts, the last one winning.
    entry: Option<Vec<u8>>,
    output_format: Option<String>,
    startup: Vec<PathBuf>,
}

impl Loader<'_> {
    fn parse(&self, data: &[u8], path: &Path) -> Result<Script> {
        let mut reader = IncludeReader {
            provider: self.options.input_provider.clone(),
            files: FsReader {
                search_dirs: self.out.search_paths.clone(),
            },
        };
        parse_script(data, path, &mut reader).map_err(|e| Error::Script(Box::new(e)))
    }

    /// Applies a script's non-layout commands, and returns the in-memory
    /// input script holding its input lists, if it has any.
    fn absorb(&mut self, script: &Script, _path: &Path) -> Result<Option<Vec<u8>>> {
        let mut inputs = Vec::new();
        for command in &script.commands {
            match &command.kind {
                CommandKind::Input(list) => input_list("INPUT", list, &mut inputs)?,
                CommandKind::Group(list) => input_list("GROUP", list, &mut inputs)?,
                CommandKind::Lib(list) => input_list("LIB", list, &mut inputs)?,
                CommandKind::Entry(name) => self.entry = Some(name.clone()),
                CommandKind::Output(name) => {
                    if self.out.output.is_none() {
                        self.out.output =
                            Some(PathBuf::from(String::from_utf8_lossy(name).into_owned()));
                    }
                }
                CommandKind::OutputFormat {
                    default,
                    big,
                    little,
                } => {
                    let chosen = match self.options.endian {
                        Some(crate::target::Endianness::Big) => big.as_ref().unwrap_or(default),
                        Some(crate::target::Endianness::Little) => {
                            little.as_ref().unwrap_or(default)
                        }
                        None => default,
                    };
                    self.output_format = Some(String::from_utf8_lossy(chosen).into_owned());
                }
                CommandKind::SearchDir(dir) => {
                    if !self.options.nostdlib {
                        self.out
                            .search_paths
                            .push(PathBuf::from(String::from_utf8_lossy(dir).into_owned()));
                    }
                }
                CommandKind::Extern(names) => {
                    for name in names {
                        let name = String::from_utf8_lossy(name).into_owned();
                        if !self.out.undefined.contains(&name) {
                            self.out.undefined.push(name);
                        }
                    }
                }
                CommandKind::Startup(name) => self
                    .startup
                    .push(PathBuf::from(String::from_utf8_lossy(name).into_owned())),
                CommandKind::ForceCommonAllocation => self.out.define_common = true,
                _ => {}
            }
        }
        if inputs.is_empty() {
            return Ok(None);
        }
        Ok(Some(inputs))
    }
}

/// Reads the link's scripts and binary inputs; see the [module
/// documentation](self).
///
/// # Errors
///
/// I/O errors reading scripts and binary inputs, [`Error::NotFound`] for a
/// `-T` script that does not exist, and script syntax errors.
pub fn prepare(options: &LinkOptions) -> Result<Prepared> {
    let mut loader = Loader {
        options,
        out: options.clone(),
        scripts: Vec::new(),
        entry: None,
        output_format: None,
        startup: Vec::new(),
    };
    loader.out.inputs.clear();
    // Script lookup sees the input provider's files too.
    let fs = crate::input::FileTable::for_link(options);
    let provider = options.input_provider.clone();
    let mut specs: Vec<InputSpec> = options.inputs.clone();
    if let Some(default) = &options.default_script {
        let position = specs.last().map_or(0, |s| s.position.saturating_add(1));
        specs.push(InputSpec {
            kind: InputKind::Script(default.clone()),
            attrs: crate::args::InputAttrs::default(),
            position,
        });
    }
    // Files a `-T` script names literally, which GNU ld loads when it
    // reaches the script: (insertion point, names).
    let mut moves: Vec<(usize, Vec<Vec<u8>>)> = Vec::new();
    for spec in specs {
        match &spec.kind {
            InputKind::Script(path) => {
                let search = SearchContext {
                    search_paths: &loader.out.search_paths,
                    sysroot: options.sysroot.as_deref(),
                    naming: LibraryNaming::Elf,
                    fs: &fs,
                };
                let found = search.find_script(path).ok_or_else(|| {
                    Error::NotFound(format!("cannot open linker script file {}", path.display()))
                })?;
                let data = read_file(&provider, &found)?;
                let script = loader.parse(&data, &found)?;
                let named = literal_file_names(&script);
                if !named.is_empty() {
                    moves.push((loader.out.inputs.len(), named));
                }
                if let Some(text) = loader.absorb(&script, &found)? {
                    loader.out.inputs.push(InputSpec {
                        kind: InputKind::Bytes {
                            name: found.to_string_lossy().into_owned(),
                            data: Arc::from(text),
                        },
                        ..spec.clone()
                    });
                }
                loader.scripts.push((script, true));
            }
            InputKind::File(path) if spec.attrs.format == InputFormat::Binary => {
                let data = read_file(&provider, path)?;
                let name = path.as_os_str().as_encoded_bytes().to_vec();
                let object = crate::elf::binary_input::convert(&name, &data)?;
                loader.out.inputs.push(InputSpec {
                    kind: InputKind::Bytes {
                        name: path.to_string_lossy().into_owned(),
                        data: Arc::from(object),
                    },
                    ..spec.clone()
                });
            }
            InputKind::File(path)
                if !looks_like_binary_input(path)
                    && matches!(sniff(&provider, path), Some(FileFormat::Text(_))) =>
            {
                // An implicit script. Scripts with only input lists are left
                // to the input stage, which also reports syntax errors.
                let data = read_file(&provider, path)?;
                let Ok(script) = loader.parse(&data, path) else {
                    loader.out.inputs.push(spec.clone());
                    continue;
                };
                if script.commands.iter().all(|c| input_only(&c.kind)) {
                    loader.out.inputs.push(spec.clone());
                    continue;
                }
                if let Some(text) = loader.absorb(&script, path)? {
                    loader.out.inputs.push(InputSpec {
                        kind: InputKind::Bytes {
                            name: path.to_string_lossy().into_owned(),
                            data: Arc::from(text),
                        },
                        ..spec.clone()
                    });
                }
                loader.scripts.push((script, false));
            }
            _ => loader.out.inputs.push(spec.clone()),
        }
    }
    // Move later command-line files a script names to the script's place,
    // in the order the script names them.
    for (at, names) in moves {
        let mut at = at;
        for name in names {
            let found = loader.out.inputs.iter().enumerate().skip(at).find_map(|(i, s)| {
                matches!(&s.kind, InputKind::File(p) if p.as_os_str().as_encoded_bytes() == name.as_slice())
                    .then_some(i)
            });
            if let Some(from) = found {
                let spec = loader.out.inputs.remove(from);
                loader.out.inputs.insert(at, spec);
                at = at.saturating_add(1);
            }
        }
    }
    for (offset, path) in loader.startup.iter().enumerate() {
        loader.out.inputs.insert(
            offset,
            InputSpec {
                kind: InputKind::File(path.clone()),
                attrs: crate::args::InputAttrs::default(),
                position: 0,
            },
        );
    }
    if loader.out.entry.is_none()
        && let Some(entry) = &loader.entry
    {
        loader.out.entry = Some(String::from_utf8_lossy(entry).into_owned());
    }
    if loader.out.output_format.is_none() {
        loader.out.output_format = loader.output_format.take();
    }

    let explicit_override = loader.scripts.iter().any(|(script, explicit)| {
        *explicit
            && !script.commands.iter().any(|c| {
                matches!(
                    c.kind,
                    CommandKind::Insert { .. } | CommandKind::OverwriteSections(_)
                )
            })
    });
    let layout_options = !options.section_starts.is_empty()
        || options.magic != MagicMode::Normal
        || options.rodata_segment.is_some()
        || options.ldata_segment.is_some();
    let relocatable = options.kind == OutputKind::Relocatable;
    // `--defsym` assignments come first in GNU ld's statement list (they
    // are read with the command line, before the default script). The
    // script engine evaluates them when there is a script; without one,
    // `crate::elf::defined` does after layout. Relocatable links evaluate
    // them as a script of their own.
    let defsyms = defsym_script(options)?;
    let script = if loader.scripts.is_empty()
        && (!layout_options || relocatable)
        && !(relocatable && !options.defsym.is_empty())
    {
        None
    } else if relocatable {
        // Relocatable links have no default layout: sections no script
        // statement takes are orphans of the relocatable writer, and
        // addresses (`-Ttext`, ...) do not apply.
        let mut builder = Builder::default();
        builder.add(&defsyms)?;
        for (script, _) in &loader.scripts {
            builder.add(script)?;
        }
        Some(builder.finish()?)
    } else {
        let mut builder = Builder::default();
        builder.add(&defsyms)?;
        if !explicit_override {
            let text = super::defaults::default_script(&loader.out);
            let default = parse_script(
                text.as_bytes(),
                Path::new("<default script>"),
                &mut FsReader::default(),
            )
            .map_err(|e| Error::Internal(format!("built-in linker script: {e}")))?;
            builder.add(&default)?;
        }
        for (script, _) in &loader.scripts {
            builder.add(script)?;
        }
        Some(builder.finish()?)
    };
    let relocatable_default = if relocatable && loader.scripts.is_empty() {
        let default = parse_script(
            super::defaults::relocatable_script().as_bytes(),
            Path::new("<default script>"),
            &mut FsReader::default(),
        )
        .map_err(|e| Error::Internal(format!("built-in linker script: {e}")))?;
        let mut builder = Builder::default();
        builder.add(&defsyms)?;
        builder.add(&default)?;
        Some(builder.finish()?)
    } else {
        None
    };
    Ok(Prepared {
        options: loader.out,
        script,
        relocatable_default,
    })
}

/// The `--defsym` options as a script of assignments.
fn defsym_script(options: &LinkOptions) -> Result<Script> {
    let mut script = Script {
        commands: Vec::with_capacity(options.defsym.len()),
        files: vec![PathBuf::from("--defsym")],
    };
    for (name, expr) in &options.defsym {
        let assignment = crate::elf::defined::defsym_assignment(name, expr)?;
        script.commands.push(crate::script::Command {
            span: crate::script::Span::default(),
            kind: CommandKind::Assignment(assignment),
        });
    }
    Ok(script)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binary_input_names() {
        assert!(looks_like_binary_input(Path::new("a/b.o")));
        assert!(looks_like_binary_input(Path::new("libc.so.6")));
        assert!(looks_like_binary_input(Path::new("libfoo.so")));
        assert!(!looks_like_binary_input(Path::new("link.ld")));
        assert!(!looks_like_binary_input(Path::new("libsome.script")));
    }

    #[test]
    fn input_lists_round_trip() {
        let list = [
            InputFile {
                name: InputName::Path(b"/lib/libc.so.6".to_vec()),
                as_needed: false,
            },
            InputFile {
                name: InputName::Library(b"m".to_vec()),
                as_needed: true,
            },
        ];
        let mut text = Vec::new();
        input_list("GROUP", &list, &mut text).unwrap();
        let script = parse_script(&text, Path::new("x"), &mut crate::script::NoIncludes).unwrap();
        assert_eq!(script.commands.len(), 1);
        match &script.commands[0].kind {
            CommandKind::Group(parsed) => assert_eq!(parsed.as_slice(), &list),
            other => panic!("unexpected {other:?}"),
        }
    }
}
