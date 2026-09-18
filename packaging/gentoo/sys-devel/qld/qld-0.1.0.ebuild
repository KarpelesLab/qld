# Copyright 2026 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

EAPI=8

# The crate list is Cargo.lock's; regenerate it for a new version with
# `pycargoebuild` (app-portage/pycargoebuild) on the release tarball.
RUST_MIN_VER="1.89.0"
CRATES="
	allocator-api2@0.2.21
	crossbeam-deque@0.8.8
	crossbeam-epoch@0.9.21
	crossbeam-utils@0.8.23
	either@1.18.0
	equivalent@1.0.2
	foldhash@0.2.0
	hashbrown@0.16.1
	libc@0.2.189
	memmap2@0.9.11
	rayon-core@1.13.0
	rayon@1.12.0
"

inherit cargo

DESCRIPTION="A fast, parallel linker compatible with GNU ld, gold, lld and mold"
HOMEPAGE="https://github.com/KarpelesLab/qld"
SRC_URI="
	https://github.com/KarpelesLab/qld/archive/refs/tags/v${PV}.tar.gz -> ${P}.tar.gz
	${CARGO_CRATE_URIS}
"

LICENSE="MIT"
# Dependent crate licenses
LICENSE+=" ZLIB"
SLOT="0"
KEYWORDS="~amd64 ~arm64"
IUSE="+plugin"

src_configure() {
	local myfeatures=(
		$(usev plugin)
	)
	cargo_src_configure --no-default-features
}

src_install() {
	cargo_src_install

	# The names compiler drivers look for, as sys-devel/mold installs them:
	# clang -fuse-ld=qld runs ld.qld (ld64.qld for Darwin targets), and
	# gcc -B/usr/libexec/qld runs /usr/libexec/qld/ld.
	dosym qld /usr/bin/ld.qld
	dosym qld /usr/bin/ld64.qld
	dosym -r /usr/bin/qld /usr/libexec/qld/ld

	dodoc README.md
}

pkg_postinst() {
	elog "To link with qld:"
	elog "  clang -fuse-ld=qld ...        (finds /usr/bin/ld.qld)"
	elog "  gcc -B/usr/libexec/qld ...    (gcc has no -fuse-ld=qld)"
	elog "  RUSTFLAGS='-C link-arg=-fuse-ld=qld' with linker = clang"
	if ! use plugin; then
		elog
		elog "USE=-plugin: LTO links (gcc -flto, clang -flto) are not supported."
	fi
}
