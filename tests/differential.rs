//! The differential runner: links every fixture with GNU ld and with qld, and
//! compares the normalized properties listed in `docs/testing.md` — dynamic
//! symbols (binding, visibility, version), `DT_*` tags, the multiset of
//! dynamic relocation types, section presence and flags, and program behavior
//! (exit code and stdout).
//!
//! Fixtures are skipped while qld reports "not implemented yet", and when GNU
//! ld or `readelf` is missing. `diff.ignore` in a fixture filters property
//! lines that legitimately differ; `diff.skip` opts a fixture out.
//!
//! `QLD_DIFF_CANDIDATE` replaces qld with another linker, which is how the
//! comparison itself is validated: `QLD_DIFF_CANDIDATE=ld` must show no
//! differences (see the ignored `differential_self_check` test).

mod common;

use std::fmt::Write as _;
use std::path::Path;
use std::time::Instant;

use common::fixture::{
    self, Fixture, JobResult, LinkError, Linker, Log, Status, TargetEnv, copy_dir,
};
use common::process;
use common::readelf;
use common::tools::{Triple, tools};

fn diff_job(
    fixture: &Fixture,
    reference: &Linker,
    candidate: &Linker,
    scratch: &Path,
) -> JobResult {
    let start = Instant::now();
    let dir = scratch.join(fixture::job_slug(&fixture.name));
    let mut log = Log::default();
    let status = match diff_inner(fixture, reference, candidate, &dir, &mut log) {
        Ok(status) | Err(status) => status,
    };
    JobResult {
        name: fixture.name.clone(),
        status,
        log,
        dir,
        elapsed: start.elapsed(),
    }
}

fn link_into(
    fixture: &Fixture,
    env: &TargetEnv,
    linker: &Linker,
    compiled: &Path,
    dir: &Path,
    log: &mut Log,
) -> Result<(), Status> {
    fixture::fresh_dir(dir)?;
    copy_dir(compiled, dir)
        .map_err(|e| Status::Fail(format!("cannot copy {}: {e}", compiled.display())))?;
    match fixture::link(fixture, env, linker, dir, &[], log) {
        Ok(()) => Ok(()),
        Err(LinkError::Unimplemented(message)) => Err(Status::skip(message)),
        Err(LinkError::Failed(message)) => Err(Status::Fail(format!(
            "{} failed to link:\n{message}",
            linker.name()
        ))),
        Err(LinkError::Status(status)) => Err(status),
    }
}

/// Property lines of every output, plus the program's behavior.
fn observe(
    fixture: &Fixture,
    env: &TargetEnv,
    dir: &Path,
    readelf_path: &Path,
    log: &mut Log,
) -> Result<Vec<String>, Status> {
    let mut outputs = fixture.link_outputs();
    outputs.dedup();
    let mut lines = Vec::new();
    for output in &outputs {
        let properties =
            readelf::properties(readelf_path, &dir.join(output)).map_err(Status::Fail)?;
        lines.extend(properties.into_iter().map(|p| format!("[{output}] {p}")));
    }
    if let Some(run) = fixture::run_program(fixture, env, dir, log)? {
        lines.push(format!("run: {}", run.describe_status()));
        for line in run.stdout_text().lines() {
            lines.push(format!("run: stdout: {line}"));
        }
    }
    Ok(lines)
}

fn diff_inner(
    fixture: &Fixture,
    reference: &Linker,
    candidate: &Linker,
    dir: &Path,
    log: &mut Log,
) -> Result<Status, Status> {
    if let Some(reason) = &fixture.skip {
        return Err(Status::skip(format!("fixture disabled: {reason}")));
    }
    if let Some(reason) = &fixture.diff_skip {
        return Err(Status::skip(format!("diff.skip: {reason}")));
    }
    fixture::check_required_files(fixture)?;
    if fixture.gnu_ld == fixture::GnuLdExpectation::Fail
        && (*reference == Linker::GnuLd || *candidate == Linker::GnuLd)
    {
        return Err(Status::skip(
            "GNU ld rejects this fixture (gnu_ld = \"fail\")",
        ));
    }
    let env = TargetEnv::resolve(None)?;
    for linker in [reference, candidate] {
        if *linker == Linker::GnuLd {
            fixture::check_gnu_ld_version(fixture, &linker.binary(&env)?)?;
        }
    }
    if !fixture.targets.is_empty()
        && !fixture
            .targets
            .iter()
            .any(|t| Triple::parse(t).as_ref() == Some(&env.triple))
    {
        return Err(Status::skip(format!(
            "differential runs on the host ({}) only",
            env.triple
        )));
    }
    let readelf_path = tools()
        .readelf
        .clone()
        .ok_or_else(|| Status::missing("readelf"))?;
    // Fail fast on missing linkers before compiling anything.
    reference.binary(&env)?;
    candidate.binary(&env)?;

    let compiled = dir.join("compiled");
    fixture::prepare(fixture, &env, &compiled, log)?;

    // The candidate goes first: while qld is unimplemented this is where the
    // fixture is skipped, without linking it with the reference as well.
    let candidate_dir = dir.join(candidate.slug() + "-candidate");
    link_into(fixture, &env, candidate, &compiled, &candidate_dir, log)?;
    let reference_dir = dir.join(reference.slug() + "-reference");
    link_into(fixture, &env, reference, &compiled, &reference_dir, log)?;

    let expected = observe(fixture, &env, &reference_dir, &readelf_path, log)?;
    let actual = observe(fixture, &env, &candidate_dir, &readelf_path, log)?;
    let (only_reference, only_candidate) =
        readelf::compare(&expected, &actual, &fixture.diff_ignores(&env.triple.arch));
    if only_reference.is_empty() && only_candidate.is_empty() {
        return Ok(Status::Pass(Some(format!(
            "{} properties match",
            expected.len()
        ))));
    }
    let mut message = format!(
        "normalized properties differ between {} (-) and {} (+):\n",
        reference.name(),
        candidate.name()
    );
    for line in &only_reference {
        let _ = writeln!(message, "- {line}");
    }
    for line in &only_candidate {
        let _ = writeln!(message, "+ {line}");
    }
    let _ = write!(
        message,
        "(add substrings of lines that legitimately differ to `diff.ignore` in {})",
        fixture.dir.join("test.toml").display()
    );
    Err(Status::Fail(message))
}

fn run_differential(suite: &str, reference: Linker, candidate: Linker) {
    let fixtures = Fixture::load_all(&fixture::fixtures_root()).unwrap_or_else(|e| panic!("{e}"));
    let fixtures = fixture::filter_from_env(fixtures);
    let scratch = fixture::scratch_root(&format!(
        "{suite}-{}-vs-{}",
        reference.slug(),
        candidate.slug()
    ));
    println!(
        "comparing {} fixtures: {} against {}",
        fixtures.len(),
        candidate.name(),
        reference.name()
    );
    let results = process::parallel_map(&fixtures, process::job_count(), |fixture| {
        diff_job(fixture, &reference, &candidate, &scratch)
    });
    if let Some(failures) = fixture::report(suite, &results) {
        panic!("{failures}");
    }
}

/// qld (or `QLD_DIFF_CANDIDATE`) against GNU ld.
#[test]
fn differential() {
    run_differential(
        "differential",
        Linker::GnuLd,
        Linker::from_env("QLD_DIFF_CANDIDATE"),
    );
}

/// GNU ld against itself: validates that the normalization is stable and the
/// fixtures produce comparable output. `cargo test --test differential -- --ignored`
#[test]
#[ignore = "self-check of the differential runner against GNU ld; run with --ignored"]
fn differential_self_check() {
    run_differential("diff-self-check", Linker::GnuLd, Linker::GnuLd);
}

const DYNSYM_SAMPLE: &str = "
Symbol table '.dynsym' contains 7 entries:
   Num:    Value          Size Type    Bind   Vis      Ndx Name
     0: 0000000000000000     0 NOTYPE  LOCAL  DEFAULT  UND
     1: 0000000000000000     0 NOTYPE  WEAK   DEFAULT  UND __gmon_start__
     2: 0000000000000000     0 FUNC    WEAK   DEFAULT  UND __cxa_finalize@GLIBC_2.2.5 (4)
     3: 0000000000001120    10 FUNC    GLOBAL DEFAULT   12 bar@@VERS_2
     4: 0000000000001100    10 FUNC    GLOBAL DEFAULT   12 foo@VERS_1
     5: 0000000000000000     0 OBJECT  GLOBAL DEFAULT  ABS VERS_1
     6: 0000000000001130     8 FUNC    GLOBAL PROTECTED   12 prot
     7: 0000000000001000     0 SECTION LOCAL  DEFAULT    9

Symbol table '.symtab' contains 1 entry:
   Num:    Value          Size Type    Bind   Vis      Ndx Name
     1: 0000000000000000     0 FILE    LOCAL  DEFAULT  ABS crt1.o
";

#[test]
fn parses_dynamic_symbols() {
    let symbols = readelf::parse_dyn_syms(DYNSYM_SAMPLE);
    let rendered: Vec<String> = symbols
        .iter()
        .map(|s| {
            format!(
                "{}{} {} {} {} {}",
                s.name, s.version, s.kind, s.bind, s.visibility, s.section
            )
        })
        .collect();
    assert_eq!(
        rendered,
        [
            "__gmon_start__ NOTYPE WEAK DEFAULT UND",
            "__cxa_finalize@GLIBC_2.2.5 FUNC WEAK DEFAULT UND",
            "bar@@VERS_2 FUNC GLOBAL DEFAULT DEF",
            "foo@VERS_1 FUNC GLOBAL DEFAULT DEF",
            "VERS_1 OBJECT GLOBAL DEFAULT ABS",
            "prot FUNC GLOBAL PROTECTED DEF",
        ]
    );
}

#[test]
fn parses_dynamic_section() {
    let text = "
Dynamic section at offset 0x2d98 contains 4 entries:
  Tag        Type                         Name/Value
 0x0000000000000001 (NEEDED)             Shared library: [libvers.so]
 0x000000000000001d (RUNPATH)            Library runpath: [$ORIGIN]
 0x000000006ffffffb (FLAGS_1)            Flags: NOW PIE
 0x0000000000000000 (NULL)               0x0
";
    let entries = readelf::parse_dynamic(text);
    assert_eq!(entries.len(), 4);
    assert_eq!(
        entries[0],
        ("NEEDED".into(), "Shared library: [libvers.so]".into())
    );
    assert_eq!(entries[2], ("FLAGS_1".into(), "Flags: NOW PIE".into()));
    assert_eq!(entries[3].0, "NULL");
}

#[test]
fn parses_relocations() {
    let text = "
Relocation section '.rela.dyn' at offset 0x5c8 contains 3 entries:
    Offset             Info             Type               Symbol's Value  Symbol's Name + Addend
0000000000003d88  0000000000000008 R_X86_64_RELATIVE                         11b0
0000000000003fd8  0000000100000006 R_X86_64_GLOB_DAT      0000000000000000 __libc_start_main@GLIBC_2.34 + 0
0000000000003ff8  0000000800000001 R_X86_64_64            0000000000000000 negative - 8

Relocation section '.rela.plt' at offset 0x688 contains 1 entry:
    Offset             Info             Type               Symbol's Value  Symbol's Name + Addend
0000000000003fc0  0000000400000007 R_X86_64_JUMP_SLOT     0000000000000000 bar@VERS_2 + 0

Relocation section '.relr.dyn' at offset 0x5b0 contains 1 entry which relocates 2 locations:
Index: Entry            Address           Symbolic Address
0000:  0000000000003dc8 0000000000003dc8  _DYNAMIC
";
    let rendered: Vec<String> = readelf::parse_relocs(text)
        .into_iter()
        .map(|r| format!("{} {} {}", r.section, r.kind, r.symbol))
        .collect();
    assert_eq!(
        rendered,
        [
            ".rela.dyn R_X86_64_RELATIVE ",
            ".rela.dyn R_X86_64_GLOB_DAT __libc_start_main@GLIBC_2.34",
            ".rela.dyn R_X86_64_64 negative",
            ".rela.plt R_X86_64_JUMP_SLOT bar@VERS_2",
        ]
    );
}

#[test]
fn parses_section_headers() {
    let text = "
Section Headers:
  [Nr] Name              Type            Address          Off    Size   ES Flg Lk Inf Al
  [ 0]                   NULL            0000000000000000 000000 000000 00      0   0  0
  [ 1] .note.gnu.build-id NOTE            00000000000002a8 0002a8 000024 00   A  0   0  4
  [12] .text             PROGBITS        0000000000001040 001040 000113 00  AX  0   0 16
  [26] .comment          PROGBITS        0000000000000000 003018 00001b 01  MS  0   0  1
  [27] .shstrtab         STRTAB          0000000000000000 003033 0000f8 00      0   0  1
Key to Flags:
  W (write), A (alloc), X (execute), M (merge), S (strings), I (info),
";
    let rendered: Vec<String> = readelf::parse_sections(text)
        .into_iter()
        .map(|s| format!("{} {} [{}]", s.name, s.kind, s.flags))
        .collect();
    assert_eq!(
        rendered,
        [
            ".note.gnu.build-id NOTE [A]",
            ".text PROGBITS [AX]",
            ".comment PROGBITS [MS]",
            ".shstrtab STRTAB []",
        ]
    );
}

#[test]
fn compares_property_multisets() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let left = s(&["a", "b", "b", "dynamic: DEBUG", "c"]);
    let right = s(&["a", "b", "c", "d"]);
    let (only_left, only_right) = readelf::compare(&left, &right, &s(&["DEBUG"]));
    assert_eq!(only_left, ["b"]);
    assert_eq!(only_right, ["d"]);
}
