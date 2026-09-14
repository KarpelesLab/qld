//! Unit tests for hint rendering and matching; `tests/hints.rs` builds real
//! libraries.

use super::*;

fn libm() -> LibraryMatch {
    LibraryMatch {
        object: "libm.so.6".into(),
        path: PathBuf::from("/usr/lib64/libm.so"),
        flag: "-lm".into(),
        kind: LibraryKind::Shared,
        version: Some("GLIBC_2.2.5".into()),
    }
}

#[test]
fn parses_versioned_references() {
    assert_eq!(Undefined::parse(b"cos"), Undefined::new(b"cos"));
    assert_eq!(
        Undefined::parse(b"memcpy@GLIBC_2.14"),
        Undefined::versioned(b"memcpy", b"GLIBC_2.14")
    );
    assert_eq!(
        Undefined::parse(b"memcpy@@GLIBC_2.14"),
        Undefined::versioned(b"memcpy", b"GLIBC_2.14")
    );
    assert_eq!(Undefined::parse(b"odd@"), Undefined::new(b"odd"));
}

#[test]
fn renders_missing_library_like_the_spec() {
    let hint = Hint::MissingLibrary {
        symbol: b"cos".to_vec(),
        library: libm(),
    };
    assert_eq!(
        render(&hint, true),
        "'cos' is defined in libm.so.6 (/usr/lib64/libm.so); did you forget -lm?"
    );
    let diagnostic = attach(Diagnostic::error("undefined symbol: cos"), &[hint], true);
    assert_eq!(diagnostic.notes.len(), 1);
}

#[test]
fn renders_paths_once_when_the_name_is_the_file() {
    let hint = Hint::MissingLibrary {
        symbol: b"foo".to_vec(),
        library: LibraryMatch {
            object: "libfoo.so".into(),
            path: PathBuf::from("/opt/lib/libfoo.so"),
            flag: "-lfoo".into(),
            kind: LibraryKind::Shared,
            version: None,
        },
    };
    assert_eq!(
        render(&hint, true),
        "'foo' is defined in /opt/lib/libfoo.so; did you forget -lfoo?"
    );
}

#[test]
fn renders_versions_and_linkage() {
    let mut newer = libm();
    newer.version = Some("GLIBC_2.29".into());
    let hint = Hint::VersionMismatch {
        symbol: b"exp".to_vec(),
        wanted: Some("GLIBC_2.99".into()),
        available: vec![
            AvailableVersion {
                library: libm(),
                default: false,
            },
            AvailableVersion {
                library: newer,
                default: true,
            },
        ],
    };
    assert_eq!(
        render(&hint, true),
        "'exp' is not defined at version GLIBC_2.99; available: GLIBC_2.2.5, GLIBC_2.29 (default) in libm.so.6 (/usr/lib64/libm.so)"
    );

    let hint = Hint::NearMiss {
        symbol: b"foo".to_vec(),
        near: NearMiss {
            candidate: b"_Z3fooi".to_vec(),
            kind: NearMissKind::CppDefinition,
        },
    };
    assert_eq!(
        render(&hint, true),
        "did you mean to declare foo(int) as extern \"C\"?"
    );
    assert_eq!(
        render(&hint, false),
        "did you mean to declare _Z3fooi as extern \"C\"?"
    );

    let hint = Hint::NearMiss {
        symbol: b"_ZN2ns3barEv".to_vec(),
        near: NearMiss {
            candidate: b"bar".to_vec(),
            kind: NearMissKind::CDefinition,
        },
    };
    assert_eq!(
        render(&hint, true),
        "did you mean: extern \"C\" bar? ('ns::bar()' has C++ linkage)"
    );
}

#[test]
fn library_stems() {
    for (path, stem) in [
        ("/usr/lib64/libm.so", "m"),
        ("/usr/lib64/libm.a", "m"),
        ("/usr/lib64/libm.so.6", "m"),
        ("/opt/weird.so", "weird"),
        ("libc++.so.1", "c++"),
        ("libfoo.a.so", "foo.a"),
    ] {
        assert_eq!(library_stem(Path::new(path)), stem, "{path}");
    }
}

#[test]
fn natural_version_order() {
    use std::cmp::Ordering::*;
    assert_eq!(natural_cmp("GLIBC_2.2.5", "GLIBC_2.14"), Less);
    assert_eq!(natural_cmp("GLIBC_2.14", "GLIBC_2.14"), Equal);
    assert_eq!(natural_cmp("VER_10", "VER_9"), Greater);
    assert_eq!(natural_cmp("A", "A1"), Less);
    assert_eq!(natural_cmp("", "x"), Less);
}

#[test]
fn version_rules() {
    let definition = |version: Option<&str>, hidden| Definition {
        object: 0,
        version: version.map(String::from),
        hidden,
        member: None,
    };
    assert!(version_matches(&definition(None, false), None));
    assert!(version_matches(&definition(Some("V1"), false), None));
    assert!(!version_matches(&definition(Some("V1"), true), None));
    assert!(version_matches(&definition(Some("V1"), true), Some(b"V1")));
    assert!(!version_matches(
        &definition(Some("V1"), false),
        Some(b"V2")
    ));
    assert!(!version_matches(&definition(None, false), Some(b"V2")));
}

#[test]
fn scope_directories_apply_sysroot_and_dedupe() {
    let scope = SearchScope {
        search_paths: vec![
            PathBuf::from("=/usr/lib"),
            PathBuf::from("/opt/lib"),
            PathBuf::from("=/usr/lib"),
        ],
        sysroot: Some(PathBuf::from("/sysroot")),
    };
    assert_eq!(
        scope.directories(),
        vec![PathBuf::from("/sysroot/usr/lib"), PathBuf::from("/opt/lib")]
    );
}

#[test]
fn empty_queries_do_nothing() {
    let hinter = Hinter::new(SearchScope::default(), Vec::new());
    assert!(hinter.hints(&[], &[]).is_empty());
    let hints = hinter.hints(&[Undefined::new(b"nothing_defines_this")], &[]);
    assert_eq!(hints, vec![Vec::new()]);
}
