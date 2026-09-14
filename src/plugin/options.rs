//! Recognizing `-plugin-opt` values.
//!
//! qld passes every `-plugin-opt` (and `--plugin-opt=`) value to the plugin
//! verbatim and in command-line order: the plugin, not the linker, defines
//! what they mean. This module only *recognizes* the forms the two compiler
//! drivers send, so the driver can explain problems before the plugin hits
//! them: a missing GCC environment, or an LLVM option that makes the plugin
//! end the process.
//!
//! # GCC (`liblto_plugin.so`)
//!
//! `collect2` passes the `lto-wrapper` path as the first option (no leading
//! dash), then dash-prefixed options: `-fresolution=FILE` (where the plugin
//! writes resolutions for `lto-wrapper`), `-pass-through=-lgcc` (libraries
//! the plugin hands back through `add_input_library`), `-linker-output-known`,
//! `-save-temps`, `-nop`, `-ltrans-objects=FILE`, and any other option,
//! which the plugin forwards to `lto-wrapper`.
//!
//! The plugin runs `lto-wrapper`, which runs the GCC driver again to compile
//! the IR. `lto-wrapper` refuses to run unless two environment variables are
//! set: `COLLECT_GCC` (the driver's path) and `COLLECT_GCC_OPTIONS` (its
//! quoted options). `collect2` exports both before running the linker. Like
//! GNU ld, qld does not set them: it inherits them from `gcc`, so a link
//! driven by `gcc -flto -fuse-ld=...` works unchanged. Running
//! `qld -plugin liblto_plugin.so ...` directly needs them in the environment,
//! for example `COLLECT_GCC=gcc COLLECT_GCC_OPTIONS="'-flto' '-O2'"`, plus
//! the `lto-wrapper` path and `-fresolution=` options that `collect2` would
//! have passed; [`missing_environment`] reports what is absent.
//!
//! # LLVM (`LLVMgold.so`)
//!
//! Clang passes bare keywords: `mcpu=`, `O0`..`O3`, `thinlto`, `jobs=N`,
//! `cache-dir=DIR`, `cache-policy=`, `obj-path=`, `extra-library-path=`,
//! `thinlto-index-only[=FILE]`, `emit-llvm`, `emit-asm`, `disable-output`,
//! `save-temps`, `dwo_dir=`, `sample-profile=`, and others. Unrecognized
//! values (for example `-generate-arange-section`) go to LLVM's option
//! parser.
//!
//! Four LLVM options make the plugin call `exit(0)` from its
//! all-symbols-read handler, after writing its output: `thinlto-index-only`,
//! `emit-llvm`, `emit-asm` and `disable-output`. GNU ld and gold end the same
//! way. In a library session that ends the host process; see
//! [`PluginOption::exits_process`].

use std::path::Path;

/// The toolchain a plugin belongs to, which decides how its options read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PluginFlavor {
    /// GCC's `liblto_plugin.so`.
    Gcc,
    /// LLVM's `LLVMgold.so`.
    Llvm,
    /// Neither could be recognized.
    Unknown,
}

impl PluginFlavor {
    /// Guesses the flavor from the plugin's file name, then from its options.
    #[must_use]
    pub fn detect(path: &Path, options: &[String]) -> Self {
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if name.contains("lto_plugin") || name.contains("lto-plugin") {
            return Self::Gcc;
        }
        if name.contains("llvmgold") {
            return Self::Llvm;
        }
        let gcc = options.iter().any(|option| {
            option.starts_with("-fresolution=") || option.starts_with("-pass-through=")
        });
        if gcc {
            return Self::Gcc;
        }
        let llvm = options.iter().any(|option| {
            matches!(
                classify(Self::Llvm, option),
                PluginOption::OptLevel(_)
                    | PluginOption::Cpu(_)
                    | PluginOption::ThinLto
                    | PluginOption::Jobs(_)
                    | PluginOption::CacheDir(_)
            )
        });
        if llvm { Self::Llvm } else { Self::Unknown }
    }
}

/// What one `-plugin-opt` value means to its plugin.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PluginOption<'a> {
    /// GCC: the `lto-wrapper` program (the value without a leading dash).
    LtoWrapper(&'a str),
    /// GCC: `-fresolution=FILE`.
    ResolutionFile(&'a str),
    /// GCC: `-pass-through=ARG`, an input the plugin gives back to the linker.
    PassThrough(&'a str),
    /// GCC: `-ltrans-objects=FILE`, already-compiled objects to use.
    LtransObjects(&'a str),
    /// GCC `-save-temps` or LLVM `save-temps`: keep temporary files.
    SaveTemps,
    /// LLVM: `O0` to `O3`. Holds the digit; out-of-range levels are
    /// [`Other`](Self::Other), and the plugin rejects them.
    OptLevel(u8),
    /// LLVM: `mcpu=CPU`.
    Cpu(&'a str),
    /// LLVM: `thinlto`.
    ThinLto,
    /// LLVM: `jobs=N`.
    Jobs(&'a str),
    /// LLVM: `cache-dir=DIR`.
    CacheDir(&'a str),
    /// LLVM: `cache-policy=POLICY`.
    CachePolicy(&'a str),
    /// LLVM: `obj-path=PATH`.
    ObjPath(&'a str),
    /// LLVM: `extra-library-path=DIR`.
    ExtraLibraryPath(&'a str),
    /// LLVM: `thinlto-index-only`, with the optional linked-objects file.
    ThinLtoIndexOnly(Option<&'a str>),
    /// LLVM: `emit-llvm`.
    EmitLlvm,
    /// LLVM: `emit-asm`.
    EmitAsm,
    /// LLVM: `disable-output`.
    DisableOutput,
    /// Anything else, passed through like everything above.
    Other(&'a str),
}

impl PluginOption<'_> {
    /// `true` for the LLVM options after which the plugin ends the process
    /// (`exit(0)`) at the end of its all-symbols-read handler instead of
    /// returning native objects.
    #[must_use]
    pub const fn exits_process(&self) -> bool {
        matches!(
            self,
            Self::ThinLtoIndexOnly(_) | Self::EmitLlvm | Self::EmitAsm | Self::DisableOutput
        )
    }
}

/// Classifies one option value as `flavor`'s plugin reads it. The value is
/// what follows `-plugin-opt=`.
#[must_use]
pub fn classify(flavor: PluginFlavor, option: &str) -> PluginOption<'_> {
    match flavor {
        PluginFlavor::Gcc => classify_gcc(option),
        PluginFlavor::Llvm => classify_llvm(option),
        PluginFlavor::Unknown if option.starts_with('-') => classify_gcc(option),
        PluginFlavor::Unknown => match classify_llvm(option) {
            PluginOption::Other(_) => PluginOption::Other(option),
            known => known,
        },
    }
}

fn classify_gcc(option: &str) -> PluginOption<'_> {
    if let Some(file) = option.strip_prefix("-fresolution=") {
        PluginOption::ResolutionFile(file)
    } else if let Some(arg) = option.strip_prefix("-pass-through=") {
        PluginOption::PassThrough(arg)
    } else if let Some(file) = option.strip_prefix("-ltrans-objects=") {
        PluginOption::LtransObjects(file)
    } else if option == "-save-temps" {
        PluginOption::SaveTemps
    } else if !option.starts_with('-') && !option.is_empty() {
        PluginOption::LtoWrapper(option)
    } else {
        PluginOption::Other(option)
    }
}

fn classify_llvm(option: &str) -> PluginOption<'_> {
    type Make = fn(&str) -> PluginOption<'_>;
    let prefixed: [(&str, Make); 7] = [
        ("mcpu=", |value| PluginOption::Cpu(value)),
        ("jobs=", |value| PluginOption::Jobs(value)),
        ("cache-dir=", |value| PluginOption::CacheDir(value)),
        ("cache-policy=", |value| PluginOption::CachePolicy(value)),
        ("obj-path=", |value| PluginOption::ObjPath(value)),
        ("extra-library-path=", |value| {
            PluginOption::ExtraLibraryPath(value)
        }),
        ("thinlto-index-only=", |value| {
            PluginOption::ThinLtoIndexOnly(Some(value))
        }),
    ];
    for (prefix, make) in prefixed {
        if let Some(value) = option.strip_prefix(prefix) {
            return make(value);
        }
    }
    match option.as_bytes() {
        [b'O', level @ b'0'..=b'3'] => PluginOption::OptLevel(level - b'0'),
        b"thinlto" => PluginOption::ThinLto,
        b"thinlto-index-only" => PluginOption::ThinLtoIndexOnly(None),
        b"emit-llvm" => PluginOption::EmitLlvm,
        b"emit-asm" => PluginOption::EmitAsm,
        b"disable-output" => PluginOption::DisableOutput,
        b"save-temps" => PluginOption::SaveTemps,
        _ => PluginOption::Other(option),
    }
}

/// Environment variables a plugin of `flavor` needs to compile.
#[must_use]
pub fn required_environment(flavor: PluginFlavor) -> &'static [&'static str] {
    match flavor {
        PluginFlavor::Gcc => &["COLLECT_GCC", "COLLECT_GCC_OPTIONS"],
        PluginFlavor::Llvm | PluginFlavor::Unknown => &[],
    }
}

/// The variables of [`required_environment`] that `lookup` does not find.
///
/// GCC's plugin needs them only when it runs `lto-wrapper`, so nothing is
/// missing when the options make it skip that step (`-nop`,
/// `-ltrans-objects=`). Pass `|name| std::env::var_os(name)` to check the
/// process environment.
pub fn missing_environment(
    flavor: PluginFlavor,
    options: &[String],
    lookup: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Vec<&'static str> {
    if flavor == PluginFlavor::Gcc
        && options.iter().any(|option| {
            option == "-nop" || matches!(classify(flavor, option), PluginOption::LtransObjects(_))
        })
    {
        return Vec::new();
    }
    required_environment(flavor)
        .iter()
        .copied()
        .filter(|name| lookup(name).is_none_or(|value| value.is_empty()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|&v| v.to_owned()).collect()
    }

    #[test]
    fn gcc_forms() {
        let flavor = PluginFlavor::Gcc;
        assert_eq!(
            classify(
                flavor,
                "/usr/libexec/gcc/x86_64-pc-linux-gnu/15/lto-wrapper"
            ),
            PluginOption::LtoWrapper("/usr/libexec/gcc/x86_64-pc-linux-gnu/15/lto-wrapper")
        );
        assert_eq!(
            classify(flavor, "-fresolution=/tmp/cc1.res"),
            PluginOption::ResolutionFile("/tmp/cc1.res")
        );
        assert_eq!(
            classify(flavor, "-pass-through=-lgcc_s"),
            PluginOption::PassThrough("-lgcc_s")
        );
        assert_eq!(
            classify(flavor, "-linker-output-known"),
            PluginOption::Other("-linker-output-known")
        );
        assert_eq!(classify(flavor, "-save-temps"), PluginOption::SaveTemps);
    }

    #[test]
    fn llvm_forms() {
        let flavor = PluginFlavor::Llvm;
        assert_eq!(classify(flavor, "O2"), PluginOption::OptLevel(2));
        assert_eq!(classify(flavor, "O9"), PluginOption::Other("O9"));
        assert_eq!(classify(flavor, "mcpu=znver3"), PluginOption::Cpu("znver3"));
        assert_eq!(classify(flavor, "thinlto"), PluginOption::ThinLto);
        assert_eq!(classify(flavor, "jobs=8"), PluginOption::Jobs("8"));
        assert_eq!(
            classify(flavor, "cache-dir=/tmp/c"),
            PluginOption::CacheDir("/tmp/c")
        );
        assert_eq!(
            classify(flavor, "thinlto-index-only=list.txt"),
            PluginOption::ThinLtoIndexOnly(Some("list.txt"))
        );
        assert!(classify(flavor, "thinlto-index-only").exits_process());
        assert!(classify(flavor, "emit-llvm").exits_process());
        assert!(!classify(flavor, "O3").exits_process());
        assert_eq!(
            classify(flavor, "-generate-arange-section"),
            PluginOption::Other("-generate-arange-section")
        );
    }

    #[test]
    fn flavor_detection() {
        assert_eq!(
            PluginFlavor::detect(Path::new("/usr/lib/llvm/22/lib64/LLVMgold.so"), &[]),
            PluginFlavor::Llvm
        );
        assert_eq!(
            PluginFlavor::detect(Path::new("/usr/libexec/gcc/x/15/liblto_plugin.so"), &[]),
            PluginFlavor::Gcc
        );
        assert_eq!(
            PluginFlavor::detect(Path::new("p.so"), &strings(&["-fresolution=x.res"])),
            PluginFlavor::Gcc
        );
        assert_eq!(
            PluginFlavor::detect(Path::new("p.so"), &strings(&["O2", "thinlto"])),
            PluginFlavor::Llvm
        );
        assert_eq!(
            PluginFlavor::detect(Path::new("p.so"), &[]),
            PluginFlavor::Unknown
        );
    }

    #[test]
    fn gcc_environment() {
        let flavor = PluginFlavor::Gcc;
        let none = |_: &str| None;
        assert_eq!(
            missing_environment(flavor, &[], none),
            ["COLLECT_GCC", "COLLECT_GCC_OPTIONS"]
        );
        let some = |name: &str| (name == "COLLECT_GCC").then(|| "gcc".into());
        assert_eq!(
            missing_environment(flavor, &[], some),
            ["COLLECT_GCC_OPTIONS"]
        );
        assert!(missing_environment(flavor, &strings(&["-nop"]), none).is_empty());
        assert!(missing_environment(PluginFlavor::Llvm, &[], none).is_empty());
    }
}
