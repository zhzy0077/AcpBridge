# Stage 1: Build the Rust application
FROM rust:1.85-bookworm AS builder

WORKDIR /usr/src/acpbridge
COPY . .
# Install build dependencies
RUN apt-get update && apt-get install -y pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
RUN cargo build --release

# Stage 2: Create the runtime image
FROM debian:bookworm-slim

# Install system dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    curl \
    gnupg \
    libssl-dev \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Install Node.js 24.x (LTS)
RUN curl -fsSL https://deb.nodesource.com/setup_24.x | bash - \
    && apt-get install -y nodejs \
    && rm -rf /var/lib/apt/lists/*

# Verify Node.js installation
RUN node --version && npm --version

# Configure npm global directory for root user
ENV NPM_CONFIG_PREFIX=/usr/local
ENV PATH=$NPM_CONFIG_PREFIX/bin:$PATH

# Install AI Agent CLI tools globally
RUN npm install -g @anthropic-ai/claude-code
RUN npm install -g opencode-ai
RUN npm install -g @github/copilot
RUN npm install -g @google/gemini-cli

# Install uv and Kimi CLI
RUN curl -LsSf https://astral.sh/uv/install.sh | env UV_INSTALL_DIR=/usr/local/bin INSTALLER_NO_MODIFY_PATH=1 sh -s
ENV PATH=/root/.local/bin:$PATH
ENV UV_TOOL_DIR=/usr/local/share/uv/tools
ENV UV_TOOL_BIN_DIR=/usr/local/bin
RUN uv tool install --python 3.13 kimi-cli

# Set working directory
WORKDIR /app

# Copy the binary from the builder stage
COPY --from=builder /usr/src/acpbridge/target/release/acpbridge .

# Copy example config as a default
COPY config.example.yaml ./config.yaml

# Run acpbridge
# Users can override the config path via the ACP_CONFIG environment variable
ENTRYPOINT ["./acpbridge"]
