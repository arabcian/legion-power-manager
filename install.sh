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
#   $PREFIX/lib/udev/rules.d/70-legion-power-manager-lighting.rules   keyboard lighting (uaccess)
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
        # Private HOME/config too: the training run must not read (or write)
        # the user's real scenes, presets and guards. The GUI itself also keeps
        # every root helper and automatic scene off while LPM_PGO_TRAIN is set.
        rt=$(as_user mktemp -d)
        as_user mkdir -p "$rt/home" "$rt/run" && as_user chmod 700 "$rt/run"
        as_user env QT_QPA_PLATFORM=offscreen XDG_RUNTIME_DIR="$rt/run" HOME="$rt/home" \
            XDG_CONFIG_HOME="$rt/home/.config" XDG_CACHE_HOME="$rt/home/.cache" XDG_DATA_HOME="$rt/home/.local/share" \
            LPM_PGO_TRAIN=5 timeout 180 gui/build-pgo/legion-power-manager >/dev/null 2>&1 || true
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
install "${own[@]}" -m 0755 "$T/legion-profile-helper" "$T/fwattr-helper" "$T/ryzen-co-helper" "$T/tune-helper" "$T/intel-uv-helper" "$T/legion-gpu-helper" "$T/legion-firmware-helper" "$T/lighting-helper" "$T/amdgpu-helper" "$T/lpm-boot-guard" \
    "$DESTDIR$LIBEXEC/"
install "${own[@]}" -m 0700 "$T/nvcurve-root-helper" "$DESTDIR$LIBEXEC/"
install "${own[@]}" -m 0755 "$T/nvcurve" "$T/lpm-gamemode" "$T/lpm-intel-uv" "$DESTDIR$PREFIX/bin/"
DESTDIR="$DESTDIR" cmake --install gui/build --strip

install -d "${own[@]}" -m 0755 "$DESTDIR$PREFIX/share/polkit-1/actions" "$DESTDIR/etc/polkit-1/rules.d" \
    "$DESTDIR/etc/init.d" "$DESTDIR/etc/nvcurve/profiles"
sed "s|@LIBEXEC@|$LIBEXEC|g" packaging/polkit/com.legion-power-manager.policy > "$DESTDIR$PREFIX/share/polkit-1/actions/com.legion-power-manager.policy"
[[ $EUID -eq 0 ]] && chown root:root "$DESTDIR$PREFIX/share/polkit-1/actions/com.legion-power-manager.policy"; chmod 0644 "$DESTDIR$PREFIX/share/polkit-1/actions/com.legion-power-manager.policy"
sed "s|@LIBEXEC@|$LIBEXEC|g" packaging/polkit/49-legion-power-manager.rules > "$DESTDIR/etc/polkit-1/rules.d/49-legion-power-manager.rules"
[[ $EUID -eq 0 ]] && chown root:root "$DESTDIR/etc/polkit-1/rules.d/49-legion-power-manager.rules"; chmod 0644 "$DESTDIR/etc/polkit-1/rules.d/49-legion-power-manager.rules"
# Keyboard lighting: uaccess on the Spectrum controller's hidraw node.
UDEVDIR=${UDEVDIR:-$PREFIX/lib/udev/rules.d}
install -d "${own[@]}" -m 0755 "$DESTDIR$UDEVDIR"
install "${own[@]}" -m 0644 packaging/udev/70-legion-power-manager-lighting.rules "$DESTDIR$UDEVDIR/"
if [[ -z "$DESTDIR" ]] && command -v udevadm >/dev/null; then
    udevadm control --reload 2>/dev/null || true
    udevadm trigger --subsystem-match=hidraw --action=change 2>/dev/null || true
fi
# Service files carry @BINDIR@/@LIBEXEC@ so a non-/usr PREFIX points at the right binaries.
subst() { sed -e "s|@BINDIR@|$PREFIX/bin|g" -e "s|@LIBEXEC@|$LIBEXEC|g" "$1"; }
for s in nvcurve-autoload lpm-tune lpm-intel-uv lpm-intel-uv-daemon lpm-boot-guard; do
    subst "packaging/openrc/$s" > "$DESTDIR/etc/init.d/$s"
    chmod 0755 "$DESTDIR/etc/init.d/$s"
done
install -d "${own[@]}" -m 0755 "$DESTDIR$UNITDIR"
for u in nvcurve-autoload lpm-tune lpm-intel-uv lpm-intel-uv-daemon lpm-boot-guard; do
    subst "packaging/systemd/$u.service" > "$DESTDIR$UNITDIR/$u.service"
    chmod 0644 "$DESTDIR$UNITDIR/$u.service"
done
[[ $EUID -eq 0 ]] && chown root:root "$DESTDIR"/etc/init.d/{nvcurve-autoload,lpm-tune,lpm-intel-uv,lpm-intel-uv-daemon,lpm-boot-guard} "$DESTDIR$UNITDIR"/{nvcurve-autoload,lpm-tune,lpm-intel-uv,lpm-intel-uv-daemon,lpm-boot-guard}.service
# elogind resume hook (re-applies the Intel undervolt boot profile; systemd uses the unit's sleep targets).
ELOGIND_SLEEP=${ELOGIND_SLEEP:-}
if [[ -z $ELOGIND_SLEEP ]]; then
    for d in /lib64/elogind /usr/lib64/elogind /lib/elogind /usr/lib/elogind; do
        [[ -d $d ]] && { ELOGIND_SLEEP=$d/system-sleep; break; }
    done
fi
if [[ -n $ELOGIND_SLEEP ]]; then
    install -d "${own[@]}" -m 0755 "$DESTDIR$ELOGIND_SLEEP"
    subst packaging/sleep/lpm-intel-uv > "$DESTDIR$ELOGIND_SLEEP/50-lpm-intel-uv"
    chmod 0755 "$DESTDIR$ELOGIND_SLEEP/50-lpm-intel-uv"
    [[ $EUID -eq 0 ]] && chown root:root "$DESTDIR$ELOGIND_SLEEP/50-lpm-intel-uv"
fi

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
    echo "Installed. Optional boot services (each pulls in lpm-boot-guard, which pauses them"
    echo "after a boot that crashed right after applying them):"
    if [[ -d /run/systemd/system ]]; then
        systemctl daemon-reload || true
        echo "  systemctl enable lpm-boot-guard.service     # records clean shutdowns (login-scene guard)"
        echo "  systemctl enable nvcurve-autoload.service   # GPU V/F profile"
        echo "  systemctl enable lpm-tune.service           # Optimizations boot preset"
        grep -q GenuineIntel /proc/cpuinfo && echo "  systemctl enable lpm-intel-uv.service       # Intel undervolt boot/resume profile (or lpm-intel-uv-daemon)"
    else
        echo "  rc-update add lpm-boot-guard default     # records clean shutdowns (login-scene guard)"
        echo "  rc-update add nvcurve-autoload default   # GPU V/F profile"
        echo "  rc-update add lpm-tune boot              # Optimizations boot preset"
        grep -q GenuineIntel /proc/cpuinfo && echo "  rc-update add lpm-intel-uv boot          # Intel undervolt boot profile (or lpm-intel-uv-daemon default)"
    fi
    if [[ -x /usr/local/bin/lutris-game-tune-wrapper ]]; then
        echo
        echo "Note: lutris-game-tune is still installed (setuid wrapper). lpm-gamemode replaces it;"
        echo "switch the Lutris hooks (Optimizations → Game launch) before running its uninstall.sh."
    fi
fi
