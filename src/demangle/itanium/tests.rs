//! Unit tests for the Itanium demangler; the bulk of the vectors live in
//! `tests/data/demangle/`.

use super::demangle;

fn check(mangled: &str, expected: &str) {
    assert_eq!(
        demangle(mangled.as_bytes()).as_deref(),
        Some(expected),
        "{mangled}"
    );
}

#[test]
fn basics() {
    check("_ZN3foo3barEv", "foo::bar()");
    check(
        "_ZNSt6vectorISt6vectorIiSaIiEESaIS1_EE9push_backERKS1_",
        "std::vector<std::vector<int, std::allocator<int> >, std::allocator<std::allocator<int> > >::push_back(std::allocator<int> const&)",
    );
}
