# syntax=docker/dockerfile:1
FROM --platform=$BUILDPLATFORM rust:1.91.1-bookworm@sha256:c1e5f19e773b7878c3f7a805dd00a495e747acbdc76fb2337a4ebf0418896b33 AS build
ARG BUILDARCH
ARG TARGETARCH
ARG CARGO_BUILD_JOBS=2
# Keep rustc, build scripts, libclang and protoc native when building for another CPU.
RUN set -eu; \
    case "$BUILDARCH/$TARGETARCH" in \
        amd64/amd64|arm64/arm64) extra='' ;; \
        arm64/amd64) extra='g++-x86-64-linux-gnu libc6-dev-amd64-cross' ;; \
        amd64/arm64) extra='g++-aarch64-linux-gnu libc6-dev-arm64-cross' ;; \
        *) echo "unsupported platform $BUILDARCH/$TARGETARCH" >&2; exit 1 ;; \
    esac; \
    apt-get update; apt-get install -y --no-install-recommends \
        clang libclang-dev cmake pkg-config libssl-dev protobuf-compiler build-essential $extra; \
    rm -rf /var/lib/apt/lists/*
WORKDIR /src/host
COPY host/ ./
RUN --mount=type=cache,id=till-registry,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,id=till-git,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,id=till-target-${TARGETARCH},target=/src/host/target,sharing=locked \
    set -eu; \
    set --; bin=target/release/card-sidecar; strip=strip; \
    if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
        case "$TARGETARCH" in amd64) cpu=x86_64 ;; arm64) cpu=aarch64 ;; esac; \
        prefix=$cpu-linux-gnu; target=$cpu-unknown-linux-gnu; \
        target_env=$(echo "$target" | tr - _); upper=$(echo "$target_env" | tr '[:lower:]' '[:upper:]'); \
        rustup target add "$target"; \
        export "CARGO_TARGET_${upper}_LINKER=$prefix-gcc" "CC_$target_env=$prefix-gcc" \
            "CXX_$target_env=$prefix-g++" "AR_$target_env=$prefix-ar" \
            "BINDGEN_EXTRA_CLANG_ARGS_$target_env=--sysroot=/usr/$prefix -I/usr/$prefix/include"; \
        export CARGO_TARGET_DIR=target/cross-$BUILDARCH; \
        set -- --target "$target"; bin=$CARGO_TARGET_DIR/$target/release/card-sidecar; strip=$prefix-strip; \
    fi; \
    cargo build "$@" --jobs "$CARGO_BUILD_JOBS" --locked --release -p fedimint-card-sidecar --bin card-sidecar; \
    install -D -m 0755 "$bin" /out/card-sidecar; "$strip" /out/card-sidecar

FROM debian:bookworm-slim@sha256:99c0b25c46d0214d5a2091092e7908907dcaecd6a962cde0ac700acee222b6f3
LABEL org.opencontainers.image.source="https://github.com/fedimintcard/fedimintcard" \
      org.opencontainers.image.title="Fedimint Card 4.0 Till" \
      org.opencontainers.image.description="Merchant till for Fedimint Card 4.x cards only. Requires a Card 4.0 registry."
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl libssl3 \
    && rm -rf /var/lib/apt/lists/* && install -d -m 0700 -o 1000 -g 1000 /data
COPY --from=build /out/card-sidecar /usr/local/bin/card-sidecar
COPY LICENSE NOTICE /usr/share/doc/fedimintcard/
RUN card-sidecar till admin --help >/dev/null
ENV CARD_DATA_DIR=/data TILL_ADMIN_LISTEN=0.0.0.0:9878 TILL_LISTEN=0.0.0.0:9877 TILL_REQUIRE_POS_TOKEN=true
USER 1000:1000
VOLUME ["/data"]
EXPOSE 9877 9878
HEALTHCHECK --interval=30s --timeout=5s --start-period=20s CMD curl -fsS http://127.0.0.1:9878/healthz >/dev/null || exit 1
ENTRYPOINT ["card-sidecar"]
CMD ["till", "admin"]
