# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: AGPL-3.0-only

FROM gcr.io/oss-fuzz-base/base-builder-rust:v1@sha256:8d2315aeda612bc4176bf20057a72db2a7f61cdf4a8699829c147ab5af02d234

# The image's bundled nightly predates the crate's minimum Rust version
# (1.99), so install a dated nightly and use it for the fuzz build. A
# dated one keeps a broken nightly from failing every pull request.
# RUSTUP_TOOLCHAIN overrides the repository's pinned stable toolchain,
# which cannot pass the sanitizer flags.
ARG NIGHTLY=nightly-2026-11-01
RUN rustup toolchain install $NIGHTLY --component rust-src

ENV RUSTUP_TOOLCHAIN=$NIGHTLY

COPY . $SRC/hatchrun
WORKDIR $SRC/hatchrun
COPY ./.clusterfuzzlite/build.sh $SRC/
