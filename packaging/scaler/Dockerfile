# SPDX-FileCopyrightText: Copyright 2026 Gamunu Balagalla <gamunu.balagalla@gmail.com>
#
# SPDX-License-Identifier: AGPL-3.0-only

# The scaler image. The release workflow builds the static binaries first
# and passes a context holding amd64/hatchrun-scaler and
# arm64/hatchrun-scaler next to LICENSE and NOTICE, so nothing compiles
# here and the image holds the same binary as the release tarball.
#
# distroless/static has CA certificates and a nonroot user and nothing
# else. The scaler writes no files, so it runs with a read-only root
# filesystem.
FROM gcr.io/distroless/static-debian13:nonroot@sha256:d62067879de890a1def9da0e7766b7d20810a5e5555cef8506b752437d7a3e88

ARG TARGETARCH

LABEL org.opencontainers.image.title="hatchrun-scaler" \
      org.opencontainers.image.description="Launches an EC2 instance for every GitHub Actions job of a runner scale set" \
      org.opencontainers.image.source="https://github.com/hatchrun/hatchrun" \
      org.opencontainers.image.licenses="AGPL-3.0-only"

COPY --chmod=0444 LICENSE NOTICE /usr/share/doc/hatchrun-scaler/
COPY --chmod=0555 ${TARGETARCH}/hatchrun-scaler /usr/local/bin/hatchrun-scaler

# Numeric, so Kubernetes runAsNonRoot can check it.
USER 65532:65532
ENTRYPOINT ["/usr/local/bin/hatchrun-scaler"]
