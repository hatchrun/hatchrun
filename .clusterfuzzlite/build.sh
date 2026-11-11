#!/bin/bash
# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: AGPL-3.0-only
set -eu

cd "$SRC/hatchrun"

# RUSTUP_TOOLCHAIN from the Dockerfile (a dated nightly) overrides the repository's
# pinned stable toolchain, so cargo fuzz can pass the sanitizer flags.
cargo fuzz build -O --debug-assertions --fuzz-dir fuzz

targets=$(cargo fuzz list --fuzz-dir fuzz)
if [ -z "$targets" ]; then
    echo "cargo fuzz list named no target" >&2
    exit 1
fi

# cargo fuzz builds for the host triple unless told otherwise.
host=$(rustc -vV | sed -n 's/^host: //p')
target_dir=fuzz/target/$host/release
for name in $targets; do
    cp "$target_dir/$name" "$OUT/"
done

# cifuzz unpacks <target>_seed_corpus.zip before fuzzing. Start the
# config and convert targets from the fixtures the tests already use.
zip -j -q "$OUT/config_seed_corpus.zip" crates/workflow/tests/fixtures/spec_example.yml
zip -j -q "$OUT/convert_seed_corpus.zip" crates/workflow/tests/fixtures/github/*.yml

# The convert target splits its input on NUL into docker-build.yml and
# release.yml. The release fixture calls docker-build, so this seed
# reaches a resolved local call from the start.
pair=$(mktemp -d)
github=crates/workflow/tests/fixtures/github
{ cat "$github/docker-build.yml"; printf '\0'; cat "$github/release.yml"; } > "$pair/docker-build-release"
zip -j -q "$OUT/convert_seed_corpus.zip" "$pair/docker-build-release"
rm -rf "$pair"

for dir in fuzz/seeds/*/; do
    [ -d "$dir" ] || continue
    name=$(basename "$dir")
    (cd "$dir" && zip -q -r "$OUT/${name}_seed_corpus.zip" .)
done
