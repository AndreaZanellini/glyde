#!/usr/bin/env bash
# Regenerates THIRD-PARTY-LICENSES.txt, the notices every Glyde package ships
# (MIT, BSD, Apache-2.0, OFL and the other licenses of the code compiled into
# the app ask for their copyright and license text to travel with it).
#
# Run it from anywhere after Cargo.lock changes; the test
# `the_third_party_notices_list_every_crate_in_the_app` says when.
# Needs cargo-about: cargo install cargo-about --locked --version 0.9.3 --features cli
set -euo pipefail

here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
out="$root/THIRD-PARTY-LICENSES.txt"
fonts="$root/crates/glyde-app/assets/fonts"

tmp=$(mktemp)
trap 'rm -f "$tmp"' EXIT

# Only glyde-app's dependency tree: that is what the shipped binary contains.
# --frozen: the exact Cargo.lock, and license texts read from the crates'
# own sources only, so the same lock always gives the same file. --fail: a
# crate whose license cannot be read or is not accepted stops the script.
cargo about generate \
    --frozen \
    --fail \
    --config "$here/about.toml" \
    --manifest-path "$root/crates/glyde-app/Cargo.toml" \
    --output-file "$tmp" \
    "$here/about.hbs"

{
    cat "$tmp"
    # The interface fonts are files in this repository, not crates, so
    # cargo-about does not see them. Their license files carry the copyright.
    printf '%s\n' \
        "================================================================================" \
        "SIL Open Font License 1.1 (OFL-1.1): fonts embedded in Glyde's interface" \
        "" \
        "Used by:" \
        "  - Inter 4.1 (https://github.com/rsms/inter)" \
        "  - JetBrains Mono 2.304 (https://github.com/JetBrains/JetBrainsMono)" \
        ""
    cat "$fonts/Inter-OFL.txt"
    printf '\n'
    cat "$fonts/JetBrainsMono-OFL.txt"
    # Some crates' license files use Windows line endings: one style for
    # the whole file, so it reads the same, and regenerates byte for byte,
    # on every system.
} | tr -d '\r' > "$out"

echo "wrote $out"
