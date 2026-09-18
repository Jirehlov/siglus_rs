#!/bin/sh
# Switch build entry — scaffold only (PR1). See platform/switch/ROADMAP.md.
# PR3 replaces this with the real devkitA64 + build-std invocation and NRO packaging.
set -e
echo "switch: build scaffold (ROADMAP PR1) — devkitA64 toolchain required (see platform/switch/README.md)"
echo "planned command:"
echo "  cargo +nightly build -p siglus_engine --release \\"
echo "    --target platform/switch/rust/aarch64-switch.json \\"
echo "    -Zbuild-std=core,alloc,std,panic_abort"
exit 1
