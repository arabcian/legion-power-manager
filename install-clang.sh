#!/bin/bash
# Legion Power Manager 2 — clang/LLVM build and install.
# Same as install.sh, but the GUI is built with clang++ (lld when present,
# PGO via llvm-profdata) and the Rust helpers are linked with clang.
# Every install.sh option works here too:  sudo ./install-clang.sh --no-pgo
exec "$(dirname "$0")/install.sh" --clang "$@"
