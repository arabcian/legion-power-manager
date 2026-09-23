#!/bin/bash
# Legion Power Manager 2 — build and install (Rust helpers + Qt6 GUI).
#
#   sudo ./install.sh                  build + install to /usr
#   sudo ./install.sh --remove-legacy  also remove the old Python install
#   DESTDIR=/tmp/stage ./install.sh --no-build   stage only (packaging)
#
# Build optimizations (local build → tuned for this machine by default):
#   --no-native   portable binaries (no -march=native / -C target-cpu=native)
#   --no-lto      disable link-time optimization for the GUI (Rust always uses fat LTO)
#   --no-pgo      skip the profile-guided GUI build (default: instrumented build,
#                 offscreen training
#                 run over every tab, then the final build with the profile)
#   --no-harden   drop stack protector / FORTIFY=3 / CET / full RELRO / PIE on the GUI
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
BUILD=1 LEGACY=0 NATIVE=1 LTO=1 PGO=1 HARDEN=1
for a in "$@"; do
    case "$a" in
        --no-build) BUILD=0 ;;
        --remove-legacy) LEGACY=1 ;;
        --no-native) NATIVE=0 ;;
        --no-lto) LTO=0 ;;
        --pgo) PGO=1 ;;
        --no-pgo) PGO=0 ;;
        --no-harden) HARDEN=0 ;;
        *) echo "unknown option: $a" >&2; exit 2 ;;
    esac
done
[[ -n "$DESTDIR" || $EUID -eq 0 ]] || { echo "run as root (or set DESTDIR)" >&2; exit 1; }

as_user() { if [[ $EUID -eq 0 && -n "${SUDO_USER:-}" ]]; then sudo -u "$SUDO_USER" "$@"; else "$@"; fi; }

onoff() { (( $1 )) && echo ON || echo OFF; }

if (( BUILD )); then
    # ── Rust: fat LTO + codegen-units=1 + panic=abort come from Cargo.toml ──
    rustflags=${RUSTFLAGS:-}
    (( NATIVE )) && rustflags+=" -C target-cpu=native"
    as_user env RUSTFLAGS="$rustflags" cargo build --release --locked

    # ── GUI ──
    gui_cmake() {  # gui_cmake <builddir> <pgo-mode>
        as_user cmake -S gui -B "$1" -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX="$PREFIX" \
            -DLPM_HELPER_DIR="$LIBEXEC" -DLPM_LTO="$(onoff $LTO)" -DLPM_NATIVE="$(onoff $NATIVE)" \
            -DLPM_HARDEN="$(onoff $HARDEN)" -DLPM_PGO="$2" -DLPM_PGO_DIR="$PWD/gui/build-pgo/profile"
        as_user cmake --build "$1" -j"$(nproc)"
    }
    if (( PGO )); then
        rm -rf gui/build-pgo gui/build
        gui_cmake gui/build-pgo generate
        echo ">> PGO training run (offscreen, every tab)…"
        # Private runtime dir: the single-instance lock must not see a running GUI.
        rt=$(as_user mktemp -d)
        as_user env QT_QPA_PLATFORM=offscreen XDG_RUNTIME_DIR="$rt" LPM_PGO_TRAIN=5 \
            timeout 180 gui/build-pgo/legion-power-manager >/dev/null 2>&1 || true
        rm -rf "$rt"
        prof=gui/build-pgo/profile
        if compgen -G "$prof/*.profraw" >/dev/null; then    # clang
            as_user llvm-profdata merge -o "$prof/default.profdata" "$prof"/*.profraw
        fi
        if [[ -z $(find "$prof" -type f 2>/dev/null | head -1) ]]; then
            echo "!! training produced no profile — building without PGO" >&2
            gui_cmake gui/build ""
        else
            gui_cmake gui/build use
        fi
    else
        gui_cmake gui/build ""
    fi
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
sed "s|@LIBEXEC@|$LIBEXEC|g" packaging/polkit/com.legion-power-manager.policy > "$DESTDIR$PREFIX/share/polkit-1/actions/com.legion-power-manager.policy"
[[ $EUID -eq 0 ]] && chown root:root "$DESTDIR$PREFIX/share/polkit-1/actions/com.legion-power-manager.policy"; chmod 0644 "$DESTDIR$PREFIX/share/polkit-1/actions/com.legion-power-manager.policy"
sed "s|@LIBEXEC@|$LIBEXEC|g" packaging/polkit/49-legion-power-manager.rules > "$DESTDIR/etc/polkit-1/rules.d/49-legion-power-manager.rules"
[[ $EUID -eq 0 ]] && chown root:root "$DESTDIR/etc/polkit-1/rules.d/49-legion-power-manager.rules"; chmod 0644 "$DESTDIR/etc/polkit-1/rules.d/49-legion-power-manager.rules"
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
