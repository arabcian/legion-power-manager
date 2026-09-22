#!/bin/bash
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }
PREFIX=${PREFIX:-/usr}
rc-update del nvcurve-autoload default 2>/dev/null || true
rm -rf "$PREFIX/libexec/legion-power-manager"
rm -f "$PREFIX/bin/legion-power-manager" "$PREFIX/bin/nvcurve" \
      "$PREFIX/share/applications/legion-power-manager.desktop" \
      "$PREFIX/share/icons/hicolor/scalable/apps/legion-power-manager.svg" \
      "$PREFIX/share/polkit-1/actions/com.legion-power-manager.policy" \
      /etc/xdg/autostart/legion-power-manager.desktop \
      /etc/polkit-1/rules.d/49-legion-power-manager.rules /etc/init.d/nvcurve-autoload
echo "Removed. /etc/nvcurve (GPU profiles/config) and ~/.config/ryzen-curve-optimizer were kept."
