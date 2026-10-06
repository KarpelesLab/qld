//! `extern "C++"` patterns in version scripts and dynamic lists, compared
//! with GNU ld.
//!
//! GNU ld matches a pattern in an `extern "C++"` block against the
//! demangled name (`ns::f(int)`), so a script can select one overload of
//! a function. These tests link a small C++ library with both linkers and
//! compare the dynamic symbols and their versions.
//!
//! Tools: `g++`, GNU `ld` and `readelf`, on x86-64 Linux. A test prints
//! `SKIPPED:` and passes when one is missing.

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

fn run(dir: &Path, program: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .args(args)
        .current_dir(dir)
        .env("LC_ALL", "C")
        .output()
        .unwrap_or_else(|error| panic!("cannot run {}: {error}", program.display()))
}

fn ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

const LIBRARY: &str = r#"
namespace ns {
int f(int x) { return x; }
int f(double x) { return (int)x; }
int g() { return 2; }
struct S { int m(); static int n; };
int S::m() { return 3; }
int S::n = 4;
}
int plain(int x) { return x + 1; }
extern "C" int cfunc(void) { return 5; }
int other() { return 6; }
"#;

struct Tools {
    dir: PathBuf,
    ld: PathBuf,
    readelf: PathBuf,
}

/// Compiles the library into `name/lib.o`, or says why it cannot.
fn setup(name: &str) -> Option<Tools> {
    if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
        println!("SKIPPED: host is not x86-64 Linux");
        return None;
    }
    let (Some(cxx), Some(ld), Some(readelf)) = (
        in_path("g++"),
        in_path("ld.bfd").or_else(|| in_path("ld")),
        in_path("readelf"),
    ) else {
        println!("SKIPPED: needs g++, GNU ld and readelf");
        return None;
    };
    let dir = common::scratch::scratch_dir("version_cxx", name);
    fs::write(dir.join("lib.cc"), LIBRARY).unwrap();
    ok(
        &run(&dir, &cxx, &["-fPIC", "-c", "lib.cc", "-o", "lib.o"]),
        "g++",
    );
    Some(Tools { dir, ld, readelf })
}

impl Tools {
    /// Links `lib.o` with `args` by both linkers and returns each output's
    /// defined dynamic symbols as `binding name@version`, sorted.
    fn compare(&self, args: &[&str]) -> (Vec<String>, Vec<String>) {
        let mut gnu = args.to_vec();
        gnu.extend(["lib.o", "-o", "gnu.so"]);
        ok(&run(&self.dir, &self.ld, &gnu), "GNU ld");
        let mut ours = vec!["--no-fork"];
        ours.extend(args);
        ours.extend(["lib.o", "-o", "qld.so"]);
        ok(
            &run(&self.dir, Path::new(env!("CARGO_BIN_EXE_qld")), &ours),
            "qld",
        );
        (self.symbols("qld.so"), self.symbols("gnu.so"))
    }

    fn symbols(&self, file: &str) -> Vec<String> {
        let output = run(&self.dir, &self.readelf, &["--dyn-syms", "-W", file]);
        ok(&output, "readelf");
        let mut symbols: Vec<String> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                let fields: Vec<&str> = line.split_whitespace().collect();
                // Num: Value Size Type Bind Vis Ndx Name
                let (bind, ndx, name) = (fields.get(4)?, fields.get(6)?, fields.get(7)?);
                let number = fields.first()?.strip_suffix(':')?;
                (*ndx != "UND" && number.parse::<u32>().is_ok()).then(|| format!("{bind} {name}"))
            })
            .collect();
        symbols.sort();
        symbols
    }
}

#[test]
fn version_script_cxx_patterns_match_demangled_names() {
    let Some(tools) = setup("script") else {
        return;
    };
    fs::write(
        tools.dir.join("v.map"),
        r#"V1 {
  global:
    extern "C++" {
      "ns::f(int)";
      ns::S::*;
      plain*;
    };
    cfunc;
  local: *;
};
V2 {
  global:
    extern "c++" { ns::g*; };
} V1;
"#,
    )
    .unwrap();
    let (ours, theirs) = tools.compare(&["-shared", "--version-script=v.map"]);
    assert_eq!(ours, theirs);
    // Not only equal, but what the script says: one overload of `f`.
    assert!(
        ours.contains(&"GLOBAL _ZN2ns1fEi@@V1".to_string()),
        "{ours:?}"
    );
    assert!(
        ours.contains(&"GLOBAL _ZN2ns1gEv@@V2".to_string()),
        "{ours:?}"
    );
    assert!(!ours.iter().any(|s| s.contains("_ZN2ns1fEd")), "{ours:?}");
}

#[test]
fn dynamic_list_cxx_patterns_match_demangled_names() {
    let Some(tools) = setup("list") else {
        return;
    };
    fs::write(
        tools.dir.join("d.list"),
        "{\n  extern \"C++\" { \"ns::f(double)\"; other*; };\n  cfunc;\n};\n",
    )
    .unwrap();
    let (ours, theirs) = tools.compare(&["-pie", "-e", "cfunc", "--dynamic-list=d.list"]);
    assert_eq!(ours, theirs);
    assert_eq!(
        ours,
        ["GLOBAL _Z5otherv", "GLOBAL _ZN2ns1fEd", "GLOBAL cfunc"],
        "{ours:?}"
    );
}
