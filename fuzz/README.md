# Fuzzing

The fuzzers cover the parts of `hatchrun-workflow` that read untrusted
input: workflow and config YAML, GitHub Actions conversion, `${{ }}`
expressions, cron schedules and container options.

They build with [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html),
which needs nightly. CI pins the nightly in
`.clusterfuzzlite/Dockerfile`; use the same one locally. The fuzzers are their own workspace so the rest of
the repository keeps building on the pinned stable toolchain.

```sh
cargo install cargo-fuzz
rustup toolchain install nightly-2026-11-01
cargo +nightly-2026-11-01 fuzz run --fuzz-dir fuzz config fuzz/corpus/config fuzz/seeds/config
```

Swap `config` for `convert`, `expr` or `cron`. libFuzzer writes new
inputs to the first directory, `fuzz/corpus/<target>/`, which git
ignores; `fuzz/seeds/<target>/` holds the tracked seeds and is only
read. Crashes land in `fuzz/artifacts/<target>/`; add a minimised input
to `fuzz/seeds/<target>/` so it stays covered.

`fuzz/Cargo.lock` must resolve the crates it shares with the root
`Cargo.lock` to the same versions, so the fuzzers test what ships.
`fuzz/check-lock.py` checks this in CI. To sync after a root update:

```sh
cp Cargo.lock fuzz/Cargo.lock
cargo metadata --manifest-path fuzz/Cargo.toml --format-version 1 >/dev/null
```

CI runs [ClusterFuzzLite](https://github.com/google/clusterfuzzlite) on
pull requests and on a daily schedule (`.github/workflows/fuzz.yml`).
