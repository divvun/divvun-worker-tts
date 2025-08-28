# Build stage
FROM debian:trixie-slim AS builder

# Install build dependencies and Rust
RUN apt-get update && apt-get install -y \
    build-essential \
    curl \
    libsqlite3-dev \
    libboost-all-dev \
    cmake \
    flex \
    bison \
    wget \
    unzip \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Install Rust via rustup
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
ENV PATH="/root/.cargo/bin:${PATH}"

# Download and install libtorch
RUN wget -O libtorch.zip https://download.pytorch.org/libtorch/cpu/libtorch-shared-with-deps-2.8.0%2Bcpu.zip \
    && unzip libtorch.zip \
    && mkdir -p /opt/libtorch \
    && cp -ar libtorch/* /opt/libtorch \
    && rm -rf libtorch.zip libtorch

# Set working directory
WORKDIR /app

# Copy source code
COPY Cargo.toml Cargo.lock build.rs index.html .cargo ./
COPY src/ ./src/

# Build the application
ENV LIBTORCH=/opt/libtorch
ENV LIBTORCH_BYPASS_VERSION_CHECK=1
ENV LZMA_API_STATIC=1
RUN cargo build --release

# Runtime stage
FROM debian:trixie-slim

# Install runtime dependencies
RUN apt-get update && apt-get install -y \
    libgomp1 \
    libicu76 \
    libsqlite3-0 \
    libboost-system1.83.0 \
    libboost-filesystem1.83.0 \
    libboost-program-options1.83.0 \
    && rm -rf /var/lib/apt/lists/*

# Copy ALL libtorch libraries including bundled dependencies
COPY --from=builder /opt/libtorch /opt

# Copy the binary
COPY --from=builder /app/target/release/divvun-worker-tts /usr/local/bin/

# Set working directory
WORKDIR /app

# Expose port
EXPOSE 4000

# Default environment variables
ENV ADDRESS="tcp://0.0.0.0:4000"

# Health check
HEALTHCHECK --interval=30s --timeout=3s --start-period=5s --retries=3 \
    CMD curl -f http://localhost:4000/health || exit 1

ENV LD_LIBRARY_PATH "/opt/libtorch/lib"

# Entry point
ENTRYPOINT ["divvun-worker-tts"]