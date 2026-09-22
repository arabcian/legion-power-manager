#!/bin/bash
# Builds a self-contained release tarball (crates vendored, builds offline).
set -euo pipefail
cd "$(dirname "$0")"
V=$(sed -n 's/^project(legion-power-manager VERSION \([0-9.]*\).*/\1/p' gui/CMakeLists.txt)
[[ -n $V ]] || { echo "cannot read version from gui/CMakeLists.txt" >&2; exit 1; }
P=legion-power-manager-$V
rm -rf "dist/$P" && mkdir -p "dist/$P"
list() {
    if git ls-files 2>/dev/null | grep -q .; then git ls-files -z
    else find . -path ./dist -prune -o -path ./target -prune -o -path ./gui/build -prune -o -type f -printf '%P\0'; fi
}
while IFS= read -r -d '' f; do
    [[ -L $f ]] && { echo "skipping symlink $f" >&2; continue; }
    install -D -m "$(printf '%o' $(( 0$(stat -c %a "$f") & 0777 )))" "$f" "dist/$P/$f"
done < <(list)
(cd "dist/$P" && mkdir -p .cargo && cargo vendor --locked vendor > .cargo/config.toml)
tar -C dist -cJf "dist/$P.tar.xz" "$P"
echo "dist/$P.tar.xz"
