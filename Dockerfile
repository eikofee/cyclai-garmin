# Build from this directory, giving the SDK as a named build context:
#   docker build --build-context sdk=../cyclai/plugin-sdk -t cyclai-garmin .
# (Cargo.toml reaches the SDK at ../cyclai/plugin-sdk, hence the layout below.)
FROM rust:1-bookworm AS builder
WORKDIR /build/cyclai-garmin
COPY --from=sdk . /build/cyclai/plugin-sdk
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
RUN cargo build --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --no-create-home plugin
COPY --from=builder /build/cyclai-garmin/target/release/cyclai-garmin /usr/local/bin/cyclai-garmin
USER plugin
EXPOSE 8080
ENTRYPOINT ["cyclai-garmin"]
