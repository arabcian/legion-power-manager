#!/bin/bash
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }
PREFIX=${PREFIX:-/usr}
rc-update del nvcurve-autoload default 2>/dev/null || true
rc-update del lpm-tune boot 2>/dev/null || true
# Put every tuned value back before the helper disappears.
if [[ -x "$PREFIX/libexec/legion-power-manager/tune-helper" ]]; then
    printf '%s' '{"op":"restore"}' | "$PREFIX/libexec/legion-power-manager/tune-helper" >/dev/null || true
fi
rm -rf "$PREFIX/libexec/legion-power-manager"
rm -f "$PREFIX/bin/legion-power-manager" "$PREFIX/bin/nvcurve" "$PREFIX/bin/lpm-gamemode" /etc/init.d/lpm-tune \
      "$PREFIX/share/applications/legion-power-manager.desktop" \
      "$PREFIX/share/icons/hicolor/scalable/apps/legion-power-manager.svg" \
      "$PREFIX/share/polkit-1/actions/com.legion-power-manager.policy" \
      /etc/xdg/autostart/legion-power-manager.desktop \
      /etc/polkit-1/rules.d/49-legion-power-manager.rules /etc/init.d/nvcurve-autoload
rm -rf /run/legion-power-manager
echo "Removed. Kept: /etc/nvcurve, /etc/legion-power-manager (boot preset),"
echo "~/.config/ryzen-curve-optimizer and ~/.config/legion-power-manager (tuning presets)."
