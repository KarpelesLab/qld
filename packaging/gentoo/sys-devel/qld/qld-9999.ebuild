# Copyright 2026 Gentoo Authors
# Distributed under the terms of the GNU General Public License v2

EAPI=8

RUST_MIN_VER="1.89.0"

inherit cargo git-r3

DESCRIPTION="A fast, parallel linker compatible with GNU ld, gold, lld and mold"
HOMEPAGE="https://github.com/KarpelesLab/qld"
EGIT_REPO_URI="https://github.com/KarpelesLab/qld.git"

LICENSE="MIT"
# Dependent crate licenses
LICENSE+=" ZLIB"
SLOT="0"
IUSE="+plugin"

src_unpack() {
	git-r3_src_unpack
	cargo_live_src_unpack
}

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
