# ═══════════════════════════════════════════════════════════════
# Stage 1: PLANNER — cargo-chef prepare (creates recipe.json)
# ═══════════════════════════════════════════════════════════════
FROM rust:bookworm AS planner

RUN cargo install cargo-chef --locked
WORKDIR /router
COPY . .
RUN cargo chef prepare --recipe-path recipe.json

# ═══════════════════════════════════════════════════════════════
# Stage 2: BUILDER — cargo-chef cook (caches deps) + build
# ═══════════════════════════════════════════════════════════════
FROM rust:bookworm AS builder

ARG EXTRA_FEATURES=""
ARG VERSION_FEATURE_SET="v1"

# Install system deps
RUN apt-get update \
    && apt-get install -y libpq-dev libssl-dev pkg-config protobuf-compiler

RUN cargo install cargo-chef --locked
WORKDIR /router

# Env vars for CI builds
ENV CARGO_INCREMENTAL=0
ENV CARGO_NET_RETRY=10
ENV RUSTUP_MAX_RETRIES=10
ENV RUST_BACKTRACE="short"
# Limit parallel codegen to save RAM on 2-CPU / 15GB runner
ENV CARGO_BUILD_JOBS=2

# Step 1: Cook dependencies (CACHED if Cargo.lock unchanged)
COPY --from=planner /router/recipe.json recipe.json
RUN cargo chef cook \
    --release \
    --no-default-features \
    --features release \
    --features ${VERSION_FEATURE_SET} \
    ${EXTRA_FEATURES} \
    --recipe-path recipe.json

# Step 2: Build application (only recompiles OUR code)
COPY . .
RUN cargo build \
    --release \
    --no-default-features \
    --features release \
    --features ${VERSION_FEATURE_SET} \
    ${EXTRA_FEATURES}

# ═══════════════════════════════════════════════════════════════
# Stage 3: RUNTIME — minimal Debian image
# ═══════════════════════════════════════════════════════════════
FROM debian:bookworm-slim

ARG CONFIG_DIR=/local/config
ARG BIN_DIR=/local/bin

COPY --from=builder /router/config/payment_required_fields_v2.toml ${CONFIG_DIR}/payment_required_fields_v2.toml

ARG RUN_ENV=sandbox
ARG BINARY=router
ARG SCHEDULER_FLOW=consumer

RUN apt-get update \
    && apt-get install -y ca-certificates tzdata libpq-dev curl procps \
    && rm -rf /var/lib/apt/lists/*

EXPOSE 8080

ENV TZ=Etc/UTC \
    RUN_ENV=${RUN_ENV} \
    CONFIG_DIR=${CONFIG_DIR} \
    SCHEDULER_FLOW=${SCHEDULER_FLOW} \
    BINARY=${BINARY} \
    RUST_MIN_STACK=6291456

RUN mkdir -p ${BIN_DIR}

COPY --from=builder /router/target/release/${BINARY} ${BIN_DIR}/${BINARY}

RUN useradd --user-group --system --no-create-home --no-log-init app
USER app:app

WORKDIR ${BIN_DIR}

CMD ./${BINARY}
