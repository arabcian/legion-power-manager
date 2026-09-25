# Copyright 2026 Cihan
# Distributed under the terms of the GNU General Public License v2

EAPI=8

inherit cmake systemd xdg

DESCRIPTION="Power profile, firmware attribute and CPU/GPU curve tuning for Lenovo Legion laptops"
HOMEPAGE="https://localhost/legion-power-manager"
# Self-contained tarball from ./make-dist.sh (crates vendored, offline build).
SRC_URI="${P}.tar.xz"

LICENSE="GPL-2+"
# Vendored crates
LICENSE+=" Apache-2.0 MIT Unicode-3.0"
SLOT="0"
KEYWORDS="~amd64"
RESTRICT="fetch"

DEPEND="dev-qt/qtbase:6[gui,network,widgets]"
RDEPEND="
	${DEPEND}
	sys-auth/polkit
"
BDEPEND=">=virtual/rust-1.75"

pkg_nofetch() {
	einfo "Build the tarball with ./make-dist.sh in the source tree and copy"
	einfo "dist/${P}.tar.xz into your DISTDIR (usually /var/cache/distfiles)."
}

src_configure() {
	CMAKE_USE_DIR="${S}/gui"
	local mycmakeargs=(
		-DLPM_HELPER_DIR="${EPREFIX}/usr/libexec/${PN}"
	)
	cmake_src_configure
}

src_compile() {
	# Vendored crates via .cargo/config.toml; portage handles stripping.
	export CARGO_HOME="${T}/cargo" CARGO_PROFILE_RELEASE_STRIP=false
	cargo build --release --frozen --offline || die "cargo build failed"
	cmake_src_compile
}

src_test() {
	cargo test --release --frozen --offline || die "cargo test failed"
}

src_install() {
	local r="${S}/target/release"
	exeinto /usr/libexec/${PN}
	doexe "${r}"/{legion-profile-helper,fwattr-helper,ryzen-co-helper,tune-helper,intel-uv-helper,legion-gpu-helper,legion-firmware-helper,lpm-boot-guard}
	exeopts -m0700
	doexe "${r}"/nvcurve-root-helper
	dobin "${r}"/{nvcurve,lpm-gamemode,lpm-intel-uv}

	cmake_src_install

	insinto /usr/share/polkit-1/actions
	sed -i "s|@LIBEXEC@|${EPREFIX}/usr/libexec/legion-power-manager|g" packaging/polkit/* || die
	doins packaging/polkit/com.legion-power-manager.policy
	insinto /etc/polkit-1/rules.d
	doins packaging/polkit/49-legion-power-manager.rules
	local s
	for s in nvcurve-autoload lpm-tune lpm-intel-uv lpm-intel-uv-daemon lpm-boot-guard; do
		sed -e "s|@BINDIR@|${EPREFIX}/usr/bin|g" \
			-e "s|@LIBEXEC@|${EPREFIX}/usr/libexec/legion-power-manager|g" \
			packaging/openrc/${s} > "${T}"/${s}.initd || die
		newinitd "${T}"/${s}.initd ${s}
		sed -e "s|@BINDIR@|${EPREFIX}/usr/bin|g" \
			-e "s|@LIBEXEC@|${EPREFIX}/usr/libexec/legion-power-manager|g" \
			packaging/systemd/${s}.service > "${T}"/${s}.service || die
		systemd_dounit "${T}"/${s}.service
	done
	sed -e "s|@LIBEXEC@|${EPREFIX}/usr/libexec/legion-power-manager|g" \
		packaging/sleep/lpm-intel-uv > "${T}"/50-lpm-intel-uv || die
	exeinto /$(get_libdir)/elogind/system-sleep
	exeopts -m0755
	doexe "${T}"/50-lpm-intel-uv
	keepdir /etc/nvcurve/profiles

	dodoc README.md
}

pkg_postinst() {
	xdg_pkg_postinst
	elog "Boot-time GPU profile (set with ★ Default in the NVIDIA tab):"
	elog "  OpenRC:  rc-update add nvcurve-autoload default"
	elog "  systemd: systemctl enable nvcurve-autoload.service"
	elog "Optimizations boot preset (set with ⏻ Apply at boot):"
	elog "  OpenRC:  rc-update add lpm-tune boot"
	elog "  systemd: systemctl enable lpm-tune.service"
	elog "Intel undervolt boot/resume profile (⏻ in the Intel Undervolt tab):"
	elog "  OpenRC:  rc-update add lpm-intel-uv boot   (resume: elogind hook installed)"
	elog "  systemd: systemctl enable lpm-intel-uv.service"
	elog "  or the daemon (AC/battery switch, periodic re-apply, hwphint):"
	elog "  OpenRC:  rc-update add lpm-intel-uv-daemon default / systemd: lpm-intel-uv-daemon.service"
	elog "Experimental GPU power (Optimizations \u2192 Experimental, NVIDIA laptops) needs sys-power/acpi_call"
	elog "  and the Custom platform profile; values are written via Lenovo WMI (\\WS-free)."
	elog "Lutris hooks: /usr/bin/lpm-gamemode PRE / POST / RUN (see the Game launch sub-tab)."
	elog "The Ryzen tab needs a root-owned ryzenadj in /usr/bin, /usr/sbin,"
	elog "/usr/local/{bin,sbin} or /opt/ryzenadj."
}
