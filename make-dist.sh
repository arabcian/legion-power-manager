#!/bin/bash
# Builds a self-contained release tarball (crates vendored, builds offline).
set -euo pipefail
cd "$(dirname "$0")"
V=$(sed -n 's/^project(legion-power-manager VERSION \([0-9.]*\).*/\1/p' gui/CMakeLists.txt)
P=legion-power-manager-$V
rm -rf "dist/$P" && mkdir -p "dist/$P"
git ls-files 2>/dev/null | grep -q . && FILES=$(git ls-files) || \
    FILES=$(find . -path ./dist -prune -o -path ./target -prune -o -path ./gui/build -prune -o -type f -print | sed 's|^\./||')
echo "$FILES" | while read -r f; do install -D -m "$(stat -c %a "$f")" "$f" "dist/$P/$f"; done
(cd "dist/$P" && mkdir -p .cargo && cargo vendor --locked vendor > .cargo/config.toml)
tar -C dist -cJf "dist/$P.tar.xz" "$P"
echo "dist/$P.tar.xz"
