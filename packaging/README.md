# Packaging qld

Release archives and distribution recipes for qld. They all install the
same four names:

| Path | What | Used by |
| --- | --- | --- |
| `bin/qld` | the linker | direct use, `clang --ld-path=` |
| `bin/ld.qld` → `qld` | GNU flavor | `clang -fuse-ld=qld` (ELF and MinGW targets) |
| `bin/ld64.qld` → `qld` | Apple ld64 flavor | `clang -fuse-ld=qld` (Darwin targets) |
| `libexec/qld/ld` → `../../bin/qld` | GNU flavor | `gcc -B<prefix>/libexec/qld` |

This is what lld (`ld.lld`, `ld64.lld`) and mold (`ld.mold`,
`libexec/mold/ld`) install. qld picks its command line from the name it is
run as: `ld64` and `ld64.*` select ld64, every other name GNU ld (see
`select_flavor` in `src/args/parse.rs`). On Windows the names are copies
(`ld.qld.exe`, …), not links.

## Files

| File | What |
| --- | --- |
| `install.sh` | Installs a built binary and the three names (`--prefix`, `--destdir`, `--bindir`, `--libexecdir`, `--copy`). The recipes below and the release workflow use it. |
| `dist.sh` | Makes a release archive: `qld-VERSION-TARGET.tar.gz` (`.zip` for Windows) and its `.sha256`. Reproducible with GNU tar. |
| `release-build.sh` | Builds one target's release binary; with `--dogfood`, links qld with itself and falls back to the system linker's build if that fails. |
| `gentoo/sys-devel/qld/` | `qld-0.1.0.ebuild` (release), `qld-9999.ebuild` (git), `metadata.xml` |
| `arch/PKGBUILD` | Arch Linux / AUR |
| `debian/` | A debhelper `debian/` directory |
| `homebrew/qld.rb` | Homebrew formula template |
| `../.github/workflows/release.yml` | The release workflow |

`tests/projects/packaging.sh` installs with `install.sh` and `dist.sh` into
a temporary directory and checks that gcc and clang find qld; see
[tests/projects/packaging.md](../tests/projects/packaging.md).

## How compiler drivers find qld

### clang

`clang -fuse-ld=qld` runs `ld.qld`, or `ld64.qld` when the target is Darwin.
It looks in the `-B` directories, then next to clang, then on `PATH`. So once
`bin/` is on `PATH`, `-fuse-ld=qld` works, for C, C++ and any target clang
supports. `clang --ld-path=/path/to/ld.qld` (clang 12 and later) names the
linker exactly. Check what clang picks with:

```sh
clang -fuse-ld=qld -### hello.c 2>&1 | tail -n1                       # ".../ld.qld" ...
clang --target=arm64-apple-macos11 -fuse-ld=qld -### hello.c 2>&1 | tail -n1   # ".../ld64.qld"
```

### gcc

**gcc has no `-fuse-ld=qld`.** Its `-fuse-ld=` takes a fixed list, not an
arbitrary name: `bfd`, `gold`, `lld` and `mold` in gcc 12 to 15, and `wild`
added for gcc 16. Anything else is an error:

```
gcc: error: unrecognized command-line option '-fuse-ld=qld'; did you mean '-fuse-ld=lld'?
```

(checked with gcc 13 and 15; gcc 16's documentation lists the same set plus
`wild`). The generic `--ld-path=` that clang has was proposed for gcc in 2020
and 2021 but not merged.

Instead, point gcc at the directory that holds an `ld` link to qld, as with
mold. gcc (collect2) looks for `ld` in the `-B` directories first:

```sh
gcc -B/usr/libexec/qld hello.c -o hello
gcc -B/usr/libexec/qld -print-prog-name=ld     # prints /usr/libexec/qld/ld
readelf -p .comment hello | grep 'Linker: qld'
```

(Arch: `/usr/lib/qld`; Homebrew on Linux: `$(brew --prefix qld)/libexec/qld`;
release archives: `<dir>/libexec/qld`.) A gcc configured with
`--with-ld=/path/to/ld` ignores `-B` for the linker; `-print-prog-name=ld`
shows which one it will run.

A build system that can only say `-fuse-ld=lld` (CMake's `LINKER_TYPE`,
say) can be pointed at a directory holding an `ld.lld` link to qld, plus
`-B` that directory. That is a workaround; do not install `ld.lld` as a
package file.

### macOS

Use clang: `clang -fuse-ld=qld` runs `ld64.qld`. Homebrew gcc on macOS
runs `ld`, which would get the GNU command line, so `libexec/qld/ld` is not
useful there (the Homebrew formula does not install it on macOS).

### Rust

```toml
# .cargo/config.toml, clang as the linker driver
[target.x86_64-unknown-linux-gnu]
linker = "clang"
rustflags = ["-C", "link-arg=-fuse-ld=qld", "-C", "linker-features=-lld"]
```

or with gcc: `-C link-arg=-B/usr/libexec/qld -C linker-features=-lld`.
`linker-features=-lld` is for `x86_64-unknown-linux-gnu` only, where rustc
1.90 and later link with its bundled rust-lld by default.

## Release archives

`.github/workflows/release.yml` runs on `v*` tags (and by hand, see the
comment at its top). For each target it builds with
`packaging/release-build.sh`, packages with `packaging/dist.sh`, smoke-tests
the archive, and the `publish` job attaches every archive, its `.sha256` and
a combined `SHA256SUMS` to the GitHub release for the tag, which it creates
as a draft and publishes once everything is uploaded. 0.x versions are marked
as pre-releases.

| Target | Runner | Notes |
| --- | --- | --- |
| `x86_64-unknown-linux-gnu` | ubuntu-22.04 | glibc 2.35 or later; linked by qld |
| `x86_64-unknown-linux-musl` | ubuntu-22.04 | static; linked by qld; **no LTO** |
| `aarch64-unknown-linux-gnu` | ubuntu-22.04-arm | as x86_64 |
| `aarch64-unknown-linux-musl` | ubuntu-22.04-arm | as x86_64 |
| `x86_64-apple-darwin` | macos-latest | cross-compiled; run under Rosetta |
| `aarch64-apple-darwin` | macos-latest | |
| `universal-apple-darwin` | macos-latest | `lipo` of the two above |
| `x86_64-pc-windows-msvc` | windows-latest | |
| `aarch64-pc-windows-msvc` | windows-latest | cross-compiled, not run |

- **musl binaries cannot do LTO.** They are static, and a static musl
  program cannot `dlopen` the compiler's LTO plugin: `gcc -flto` fails with
  `liblto_plugin.so: Dynamic loading not supported`. Use the gnu build for
  LTO links.
- **Dogfooding.** On Linux, `release-build.sh --dogfood` builds qld, then
  builds it again with the first build as the linker (`gcc -B`). The second
  build ships only if its `.comment` says `Linker: qld`, it runs, and it
  links a working hello world; otherwise the job warns and ships the first
  build.
- The shipped binaries have no debug information
  (`CARGO_PROFILE_RELEASE_DEBUG=0`, `CARGO_PROFILE_RELEASE_STRIP=symbols`):
  5 MB instead of 60 MB.

To make a release: set `version` in `Cargo.toml` (the workflow fails if the
tag and the version differ), commit, then `git tag vX.Y.Z && git push origin
vX.Y.Z`. Then update the recipes below (checksums, crate list).

## Gentoo

`gentoo/sys-devel/qld/` holds a release ebuild and a live one. Both install
`/usr/bin/{qld,ld.qld,ld64.qld}` and `/usr/libexec/qld/ld`, like
`sys-devel/mold`, and have a `plugin` USE flag (on by default) for LTO.

```sh
# a local overlay
mkdir -p /var/db/repos/local/{metadata,profiles}
echo local > /var/db/repos/local/profiles/repo_name
printf 'masters = gentoo\nthin-manifests = true\n' > /var/db/repos/local/metadata/layout.conf
cp -r packaging/gentoo/sys-devel /var/db/repos/local/
# and in /etc/portage/repos.conf/local.conf:
#   [local]
#   location = /var/db/repos/local
cd /var/db/repos/local/sys-devel/qld && ebuild qld-0.1.0.ebuild manifest
emerge -av sys-devel/qld            # or =sys-devel/qld-9999 (accept_keywords "**")
```

For a new version, regenerate `CRATES` with `pycargoebuild` on the release
tarball (`app-portage/pycargoebuild`) and copy the ebuild to the new version.

## Arch Linux

```sh
cd packaging/arch && updpkgsums && makepkg -si
```

Installs `/usr/lib/qld/ld` rather than `/usr/libexec/qld/ld`, as Arch keeps
libexec files under `/usr/lib`: use `gcc -B/usr/lib/qld`.

## Debian and Ubuntu

`debian/` is a debhelper (compat 13) packaging directory:

```sh
cp -r packaging/debian debian
dpkg-buildpackage -b -us -uc       # with Debian's cargo and rustc >= 1.89
dpkg-buildpackage -b -us -uc -d    # with a rustup toolchain (skips Build-Depends)
```

Crates are downloaded from crates.io. For builders without network access
(Launchpad PPAs), vendor them first: `cargo vendor --locked debian/vendor`;
`debian/rules` then builds with `--offline` against it (and
`debian/copyright` then needs stanzas for the vendored crates). Debian 13
ships rustc 1.85, older than qld's 1.89 minimum, so the distribution's
toolchain only works on newer releases. This is a packaging skeleton for
local and PPA builds, not a Debian-policy package: the archive's Rust team
packages each crate separately (`dh-cargo`, `librust-*-dev`).

### cargo-deb

[cargo-deb](https://github.com/kornelski/cargo-deb) builds a `.deb` from
`Cargo.toml` metadata. `Cargo.toml` is frozen in this repository, so this
table is not there yet; add it when a cargo-deb build is wanted:

```toml
[package.metadata.deb]
maintainer = "qld packagers <packaging@qld.invalid>"
section = "devel"
extended-description = """\
qld links with the GNU ld, gold, lld and mold command lines. This package \
installs ld.qld and ld64.qld for clang -fuse-ld=qld, and \
/usr/libexec/qld/ld for gcc -B/usr/libexec/qld."""
assets = [
    ["target/release/qld", "usr/bin/", "755"],
    # Symbolic links, made before `cargo deb` (see below).
    ["target/deb-links/ld.qld", "usr/bin/", "777"],
    ["target/deb-links/ld64.qld", "usr/bin/", "777"],
    ["target/deb-links/libexec/ld", "usr/libexec/qld/", "777"],
    ["README.md", "usr/share/doc/qld/", "644"],
]
```

cargo-deb keeps an asset that is a symbolic link as a link, even a
dangling one. The links cannot be committed under `packaging/`: `cargo
package` fails on dangling links. So make them under `target/` first:

```sh
cargo build --release
mkdir -p target/deb-links/libexec
ln -sf qld target/deb-links/ld.qld
ln -sf qld target/deb-links/ld64.qld
ln -sf ../../bin/qld target/deb-links/libexec/ld
cargo deb --no-build
dpkg-deb -c target/debian/qld_*.deb     # ld.qld -> qld, ld64.qld -> qld, ld -> ../../bin/qld
```

(Checked with cargo-deb 3.x and `dpkg-deb`, on a copy of the tree.)

## Homebrew

`homebrew/qld.rb` builds from the source tarball. Put it in a tap
(`KarpelesLab/homebrew-tap`, `Formula/qld.rb`), set `url` and `sha256` for
the release, then:

```sh
brew install --build-from-source KarpelesLab/tap/qld
brew test qld && brew audit --strict --new qld
```

It installs `bin/{qld,ld.qld,ld64.qld}`, and `libexec/qld/ld` on Linux only.
