#!/bin/sh
# Linker flags required by lbug 0.18's prebuilt library (see
# crates/graph-store/SPIKE.md). Emitted into RUSTFLAGS by mise's [env].
# OS-aware: ld64 vs GNU ld spellings differ, and the Linux prebuilt's
# needs are probed by CI.
case "$(uname -s)" in
  Darwin)
    libdir=$(pkg-config --variable=libdir openssl)
    echo "-L$libdir -lssl -lcrypto -Clink-arg=-Wl,-export_dynamic"
    ;;
  *)
    # Export dynamic symbols so dlopen'd lbug extensions can bind.
    echo "-Clink-arg=-Wl,--export-dynamic"
    ;;
esac
