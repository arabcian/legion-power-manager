#!/bin/bash
set -euo pipefail
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }
PREFIX=${PREFIX:-/usr}
UNITDIR=${UNITDIR:-$PREFIX/lib/systemd/system}
if [[ -d /run/systemd/system ]]; then
    systemctl disable nvcurve-autoload.service lpm-tune.service 2>/dev/null || true
else
    rc-update del nvcurve-autoload default 2>/dev/null || true
    rc-update del lpm-tune boot 2>/dev/null || true
fi
# Put every tuned value back before the helper disappears.
if [[ -x "$PREFIX/libexec/legion-power-manager/tune-helper" ]]; then
    th="$PREFIX/libexec/legion-power-manager/tune-helper"
    if [[ ! -L $th && $(stat -c '%u' "$th") == 0 && $(( 0$(stat -c '%a' "$th") & 022 )) == 0 ]]; then
        printf '%s' '{"op":"restore"}' | "$th" >/dev/null || echo "warning: tune restore failed; some knobs may stay changed until reboot" >&2
    else
        echo "warning: $th is not root-owned/safe — skipping restore" >&2
    fi
fi
rm -rf "$PREFIX/libexec/legion-power-manager"
rm -f "$PREFIX/bin/legion-power-manager" "$PREFIX/bin/nvcurve" "$PREFIX/bin/lpm-gamemode" /etc/init.d/lpm-tune \
      "$PREFIX/share/applications/legion-power-manager.desktop" \
      "$PREFIX/share/icons/hicolor/scalable/apps/legion-power-manager.svg" \
      "$PREFIX/share/polkit-1/actions/com.legion-power-manager.policy" \
      /etc/xdg/autostart/legion-power-manager.desktop \
      /etc/polkit-1/rules.d/49-legion-power-manager.rules /etc/init.d/nvcurve-autoload
rm -f "$UNITDIR/nvcurve-autoload.service" "$UNITDIR/lpm-tune.service"
[[ -d /run/systemd/system ]] && systemctl daemon-reload 2>/dev/null || true
[[ -L /run/legion-power-manager ]] && rm -f /run/legion-power-manager || rm -rf /run/legion-power-manager
echo "Removed. Kept: /etc/nvcurve, /etc/legion-power-manager (boot preset),"
echo "~/.config/ryzen-curve-optimizer and ~/.config/legion-power-manager (tuning presets)."
