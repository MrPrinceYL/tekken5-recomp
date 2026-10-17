#!/usr/bin/env bash
# Launch the recompiled Tekken 5 on Linux (builds first if needed; the first build takes a while).
# The disc image defaults to ../../Game/Tekken 5 (USA).iso; set T5_DISC to use another one.
# Memory cards are folders: memcard/port0 and memcard/port1 next to this file.
set -e
here="$(cd "$(dirname "$0")" && pwd)"
: "${T5_DISC:=$here/../../Game/Tekken 5 (USA).iso}"
if [ ! -f "$T5_DISC" ]; then
    echo "Disc image not found: \"$T5_DISC\""
    echo "Set T5_DISC to the path of the Tekken 5 .iso file."
    exit 1
fi
: "${T5_MEMCARD:=$here/memcard}"
# The last session's sound is recorded to last_session.wav (for checking sound bugs).
: "${T5_WAVDUMP:=$here/last_session.wav}"
export T5_DISC T5_MEMCARD T5_WAVDUMP
cd "$here"
if [ ! -f rust/Cargo.toml ]; then
    echo "Generating the Rust code from the sheets..."
    python tools/gen_rust.py
fi
cd rust
exec cargo run -p t5-bevy "$@"
