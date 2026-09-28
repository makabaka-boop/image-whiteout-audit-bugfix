# syntax=docker/dockerfile:1

# ---- build & test stage ----------------------------------------------------
FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY tests ./tests
COPY examples ./examples
# The image only builds if the whole test suite (including the differential
# tests against the explicit tree model) passes.
RUN cargo test --release --locked && cargo build --release --locked

# ---- runtime stage ---------------------------------------------------------
# The tool is a pure stdin->stdout filter: it needs no writable filesystem,
# no network and no privileges.
FROM debian:bookworm-slim
RUN groupadd --system app && useradd --system --gid app --uid 10001 app
COPY --from=build /src/target/release/layer-merge /usr/local/bin/layer-merge
USER 10001
ENTRYPOINT ["/usr/local/bin/layer-merge"]
