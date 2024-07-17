# Build stage
FROM rust:latest as builder

WORKDIR /usr/src/app

# Install protobuf compiler
RUN apt-get update && \
    apt-get install -y protobuf-compiler && \
    rm -rf /var/lib/apt/lists/*

# Copy the entire project
COPY . .

# Build the application
RUN cargo build --release

# Final stage
FROM debian:bookworm-slim

# Install OpenSSL - often needed for Rust programs
RUN apt-get update && \
    apt-get install -y openssl ca-certificates && \
    rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Copy the build artifact from the build stage
COPY --from=builder /usr/src/app/target/release/eva01 .

# Set the startup command
CMD ["./eva01", "run", "/config/config.toml"]