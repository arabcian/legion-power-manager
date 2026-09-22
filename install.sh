#!/bin/bash
# Legion Power Manager 2 — build and install (Rust helpers + Qt6 GUI).
#
#   sudo ./install.sh                  build + install to /usr
#   sudo ./install.sh --remove-legacy  also remove the old Python install
#   DESTDIR=/tmp/stage ./install.sh --no-build   stage only (packaging)
#
# Layout:
#   /usr/bin/legion-power-manager                   GUI (Qt6)
#   /usr/bin/nvcurve                                nvcurve CLI (Rust)
#   /usr/bin/lpm-gamemode                           Lutris/Steam game-mode hook (runs as the user)
#   /usr/libexec/legion-power-manager/*-helper      pkexec targets (root:root)
#   /usr/share/polkit-1/actions/com.legion-power-manager.policy
#   /etc/polkit-1/rules.d/49-legion-power-manager.rules
#   /etc/init.d/nvcurve-autoload                    OpenRC boot-time GPU profile
#   /etc/init.d/lpm-tune                            OpenRC boot-time tuning preset
#   $PREFIX/lib/systemd/system/{nvcurve-autoload,lpm-tune}.service   systemd equivalents
# Both init flavours are installed; only the running init uses its files.
set -euo pipefail
cd "$(dirname "$0")"

PREFIX=${PREFIX:-/usr}
DESTDIR=${DESTDIR:-}
LIBEXEC="$PREFIX/libexec/legion-power-manager"
UNITDIR=${UNITDIR:-$PREFIX/lib/systemd/system}
BUILD=1 LEGACY=0
for a in "$@"; do
    case "$a" in
        --no-build) BUILD=0 ;;
        --remove-legacy) LEGACY=1 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done
[[ -n "$DESTDIR" || $EUID -eq 0 ]] || { echo "run as root (or set DESTDIR)" >&2; exit 1; }

as_user() { if [[ $EUID -eq 0 && -n "${SUDO_USER:-}" ]]; then sudo -u "$SUDO_USER" "$@"; else "$@"; fi; }

if (( BUILD )); then
    as_user cargo build --release --locked
    as_user cmake -S gui -B gui/build -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$PREFIX" \
        -DLPM_HELPER_DIR="$LIBEXEC"
    as_user cmake --build gui/build -j"$(nproc)"
fi

own=(-o root -g root); [[ $EUID -eq 0 ]] || own=()
T=target/release
install -d "${own[@]}" -m 0755 "$DESTDIR$LIBEXEC" "$DESTDIR$PREFIX/bin"
install "${own[@]}" -m 0755 "$T/legion-profile-helper" "$T/fwattr-helper" "$T/ryzen-co-helper" "$T/tune-helper" \
    "$DESTDIR$LIBEXEC/"
install "${own[@]}" -m 0700 "$T/nvcurve-root-helper" "$DESTDIR$LIBEXEC/"
install "${own[@]}" -m 0755 "$T/nvcurve" "$T/lpm-gamemode" "$DESTDIR$PREFIX/bin/"
DESTDIR="$DESTDIR" cmake --install gui/build

install -d "${own[@]}" -m 0755 "$DESTDIR$PREFIX/share/polkit-1/actions" "$DESTDIR/etc/polkit-1/rules.d" \
    "$DESTDIR/etc/init.d" "$DESTDIR/etc/nvcurve/profiles"
install "${own[@]}" -m 0644 packaging/polkit/com.legion-power-manager.policy "$DESTDIR$PREFIX/share/polkit-1/actions/"
install "${own[@]}" -m 0644 packaging/polkit/49-legion-power-manager.rules "$DESTDIR/etc/polkit-1/rules.d/"
# Service files carry @BINDIR@/@LIBEXEC@ so a non-/usr PREFIX points at the right binaries.
subst() { sed -e "s|@BINDIR@|$PREFIX/bin|g" -e "s|@LIBEXEC@|$LIBEXEC|g" "$1"; }
for s in nvcurve-autoload lpm-tune; do
    subst "packaging/openrc/$s" > "$DESTDIR/etc/init.d/$s"
    chmod 0755 "$DESTDIR/etc/init.d/$s"
done
install -d "${own[@]}" -m 0755 "$DESTDIR$UNITDIR"
for u in nvcurve-autoload lpm-tune; do
    subst "packaging/systemd/$u.service" > "$DESTDIR$UNITDIR/$u.service"
    chmod 0644 "$DESTDIR$UNITDIR/$u.service"
done
[[ $EUID -eq 0 ]] && chown root:root "$DESTDIR"/etc/init.d/{nvcurve-autoload,lpm-tune} "$DESTDIR$UNITDIR"/{nvcurve-autoload,lpm-tune}.service

if (( LEGACY )) && [[ -z "$DESTDIR" ]]; then
    # Old Python layout (and the step-1/2 drop-in binaries that replaced its .py helpers).
    rm -rf /usr/lib/legion-power-manager
    rm -f /usr/local/bin/nvcurve /etc/xdg/autostart/legion-power-manager-autostart.desktop
    rm -f /etc/polkit-1/localauthority/50-local.d/49-legion-power-manager.pkla
    echo "Removed the legacy Python install."
fi

if [[ -z "$DESTDIR" ]]; then
    command -v gtk-update-icon-cache >/dev/null && gtk-update-icon-cache -q "$PREFIX/share/icons/hicolor" || true
    echo
    echo "Installed. Optional boot services:"
    if [[ -d /run/systemd/system ]]; then
        systemctl daemon-reload || true
        echo "  systemctl enable nvcurve-autoload.service   # GPU V/F profile"
        echo "  systemctl enable lpm-tune.service           # Optimizations boot preset"
    else
        echo "  rc-update add nvcurve-autoload default   # GPU V/F profile"
        echo "  rc-update add lpm-tune boot              # Optimizations boot preset"
    fi
    if [[ -x /usr/local/bin/lutris-game-tune-wrapper ]]; then
        echo
        echo "Note: lutris-game-tune is still installed (setuid wrapper). lpm-gamemode replaces it;"
        echo "switch the Lutris hooks (Optimizations → Game launch) before running its uninstall.sh."
    fi
fi
