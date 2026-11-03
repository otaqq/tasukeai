# tasukeai image. Build from the repository root (the context needs Cargo.toml, Cargo.lock and crates/):
#   docker build -f docker/Dockerfile -t tasukeai .
# or use docker/compose.yaml (see README.md, "Run with Docker").
FROM rust:1-alpine AS base
ARG UID=1000
ARG GID=1000
WORKDIR /app
RUN apk add --no-cache musl-dev && \
    addgroup -g ${GID} appuser && \
    adduser -D -u ${UID} -G appuser appuser && \
    chown ${UID}:${GID} /app

FROM base AS builder
COPY . .
RUN cargo build --release --locked -p tasukeai && cp target/release/tasukeai /bin/tasukeai

FROM gcr.io/distroless/static-debian13:nonroot AS production
COPY --from=builder /bin/tasukeai /tasukeai
# The nonroot user of the distroless image, stated so scanners see it
USER 65532:65532
EXPOSE 9660
# distroless has no shell or wget: the binary checks its own /health
HEALTHCHECK --interval=30s --timeout=5s --start-period=10s CMD ["/tasukeai", "health"]
ENTRYPOINT ["/tasukeai", "--config", "/tasukeai.toml"]
CMD ["serve"]
