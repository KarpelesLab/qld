# Packaging: build, install, verify

How to build qld for installation, install it with the names compiler
drivers look for, and check that gcc and clang find it. The recipes and the
release workflow are in [packaging/](../../packaging/README.md);
`packaging.sh` here tests them without touching the system.

## Build

```sh
# As the release workflow does: no debug info, stripped (5 MB, not 60 MB)
CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=symbols \
  sh packaging/release-build.sh x86_64-unknown-linux-gnu

# The same, then qld linking itself (falls back to the first build on failure)
CARGO_PROFILE_RELEASE_DEBUG=0 CARGO_PROFILE_RELEASE_STRIP=symbols \
  sh packaging/release-build.sh --dogfood x86_64-unknown-linux-musl
```

The last line of the output is the binary to ship. Plain `cargo build
--release` works too; the binary then keeps the line tables of the release
profile.

## Install

```sh
# Into /usr/local (needs write access), or anywhere with --prefix
sh packaging/install.sh target/release/qld
# A staged install, as distribution packages do
sh packaging/install.sh --prefix /usr --destdir /tmp/stage target/release/qld
# A release archive, which works from wherever it is extracted
sh packaging/dist.sh target/release/qld x86_64-unknown-linux-gnu 0.1.0 dist
```

Either way the result is `bin/qld`, `bin/ld.qld` and `bin/ld64.qld` (links
to `qld`) and `libexec/qld/ld` (a link to `../../bin/qld`).

## Verify

```sh
qld --version
clang -fuse-ld=qld hello.c -o hello          # finds ld.qld on PATH
gcc -B/usr/local/libexec/qld hello.c -o hello # gcc has no -fuse-ld=qld
readelf -p .comment hello | grep 'Linker: qld'
gcc -B/usr/local/libexec/qld -print-prog-name=ld   # which ld gcc will run
clang -fuse-ld=qld -### hello.c 2>&1 | tail -n1    # which linker clang will run
```

`Linker: qld` in `.comment` is the proof: without it, the driver silently
used another linker.

`gcc -fuse-ld=qld` does not work with any gcc release: gcc's `-fuse-ld=`
accepts only `bfd`, `gold`, `lld`, `mold` and (gcc 16) `wild`, and rejects
other names with `unrecognized command-line option`. Use `-B` as above. See
[packaging/README.md](../../packaging/README.md#how-compiler-drivers-find-qld).

## packaging.sh

```sh
tests/projects/packaging.sh target/release/qld /tmp/qld-packaging
tests/projects/packaging.sh --archive dist/qld-0.1.0-x86_64-unknown-linux-gnu.tar.gz /tmp/qld-packaging
```

With a binary, it installs it twice under the scratch directory: with
`install.sh --prefix /usr --destdir SCRATCH/destdir` (the layout of the
distribution packages) and as a `dist.sh` archive extracted into
`SCRATCH/prefix` (and checks the archive's `.sha256`). With `--archive`, it
checks the given release archive, extracted the same way; the release
workflow runs it on every Linux and arm64 macOS archive. For each install it
checks:

- `bin/ld.qld`, `bin/ld64.qld` and `libexec/qld/ld` resolve to `bin/qld`;
- `ld64.qld` takes the ld64 command line (`-arch arm64`) and `ld.qld` does
  not;
- `clang --target=arm64-apple-macos11 -fuse-ld=qld -###` runs `ld64.qld`;
- Linux: `gcc -B libexec/qld`; `clang -fuse-ld=qld` with `bin` on `PATH`,
  and with `-B bin` instead; `clang --ld-path=bin/ld.qld`. Each links a
  hello world that must carry `Linker: qld` and run. `gcc -fuse-ld=qld` is
  tried too, and reported as a note when gcc rejects the name (a failure if
  gcc accepts it but does not link with qld);
- macOS: `clang -fuse-ld=qld` (checked with `-###` to run `ld64.qld`) and
  `clang --ld-path=bin/ld64.qld` link a hello world that runs.

`CC` and `CLANG` name the compilers. A missing compiler is a `SKIP`, or a
failure with `QLD_REQUIRE_PACKAGING_TOOLS=1`.

## Results

On the development machine (Gentoo, x86-64, gcc 15.3, clang 22):

- `packaging.sh` with the dogfooded static musl build: all checks pass, for
  the DESTDIR install and the archive; gcc 15.3 (and gcc 13) reject
  `-fuse-ld=qld` as expected.
- `release-build.sh --dogfood` for `x86_64-unknown-linux-gnu` and
  `x86_64-unknown-linux-musl`: the second stage is linked by qld
  (`Linker: qld` in `.comment`), runs, and links a working hello world. The
  musl build is a static PIE.
- `dist.sh` makes byte-identical archives on repeated runs.
- The Gentoo ebuild, in a scratch overlay (`ebuild qld-0.1.0.ebuild
  manifest clean install` with `FEATURES=test`, the distfiles made from
  `git archive` and the crates in `~/.cargo/registry`): builds, runs the
  test suite (763 passed, 0 failed) and installs `/usr/bin/{qld,ld.qld,
  ld64.qld}` and `/usr/libexec/qld/ld -> ../../bin/qld`.
- The Arch `PKGBUILD`'s `prepare`, `build` and `package` functions, run by
  hand (no makepkg here): the expected tree, with `/usr/lib/qld/ld`.
- `debian/rules`' configure, build (offline, from `cargo vendor`), install
  and clean overrides, run by hand (no debhelper here): the expected tree.
  `dpkg-buildpackage` itself was not run.
- cargo-deb 3.8 with the metadata in `packaging/README.md`: the `.deb` has
  the three links (`dpkg-deb -c`).
- The Homebrew formula: `ruby -c` only.
- `release.yml`: actionlint 1.7 (with shellcheck) reports nothing. It has
  not run: pushes and tags are the maintainers'.

## Not checked here

- The release workflow on GitHub (the runners, Windows `7z` packaging,
  `lipo`, `gh release`); it needs a tag push.
- macOS and Windows installs; `brew install`/`brew test`; `makepkg` and
  `dpkg-buildpackage` proper.
- gcc 16, which was not installed.
