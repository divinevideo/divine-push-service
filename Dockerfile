# Stage 1: Build the application
FROM rust:1.85.1 as builder

WORKDIR /app

# Copy manifests and lock file
COPY Cargo.toml Cargo.lock ./

# Create dummy src/main.rs to cache dependencies
RUN mkdir src && echo "fn main(){}" > src/main.rs

# Build dependencies (this layer is cached if manifests don't change)
RUN cargo build --release --bin divine_push_service

# Copy the actual source code
COPY . .

# Build the application binary, leveraging cached dependencies
RUN touch src/main.rs && cargo build --release --bin divine_push_service

# Stage 2: Create the final lean image
FROM debian:bookworm-slim

# TLS is rustls throughout (reqwest, redis), so no OpenSSL runtime is needed.
# ca-certificates still supplies the trust roots.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*

# Set working directory
WORKDIR /app

# Copy the compiled binary from the builder stage
COPY --from=builder /app/target/release/divine_push_service .

# Copy the configuration directory
COPY config ./config

# Expose the health check port
EXPOSE 8000

# Set the user (optional, but good practice)
# RUN useradd -ms /bin/bash appuser
# USER appuser

# Define the entrypoint
ENTRYPOINT ["./divine_push_service"]

# Default command (can be overridden)
CMD []