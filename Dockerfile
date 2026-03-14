# Build stage
FROM rust:1.83-bookworm AS builder

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY backend/ backend/
COPY common/ common/
# Create stub frontend/wasm so workspace resolves
RUN mkdir -p frontend/src wasm/src && \
    echo '[package]\nname = "frontend"\nversion = "0.1.0"\nedition = "2021"\n\n[lib]\ncrate-type = ["cdylib"]' > frontend/Cargo.toml && \
    echo '' > frontend/src/lib.rs && \
    echo '[package]\nname = "wasm"\nversion = "0.1.0"\nedition = "2021"\n\n[lib]\ncrate-type = ["cdylib"]' > wasm/Cargo.toml && \
    echo '' > wasm/src/lib.rs

RUN cargo build -p bitmap-render-backend --release

# Runtime stage
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*

COPY --from=builder /app/target/release/bitmap-render-backend /usr/local/bin/
COPY frontend/dist/ /app/frontend/dist/

WORKDIR /app
EXPOSE 3000

CMD ["bitmap-render-backend"]
