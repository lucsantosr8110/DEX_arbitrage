# ----------------------
# 📦 Builder Stage
# ----------------------
FROM rust:1.82 as builder

# Instala dependências para build
RUN apt-get update && apt-get install -y \
    pkg-config \
    libssl-dev \
    ca-certificates \
    git \
    gettext-base \
    nodejs \
    npm \
&& rm -rf /var/lib/apt/lists/*

WORKDIR /usr/src/app

# Cache de dependências do Rust
COPY Cargo.toml Cargo.lock ./
RUN mkdir src && echo 'fn main() {}' > src/main.rs && \
    cargo build --release && rm -rf src

# Copia o projeto inteiro e compila em release
COPY . .
RUN cargo build --release

# Instala dependências do Node.js (para Hardhat)
COPY package.json package-lock.json ./
RUN npm install

# ----------------------
# 🚀 Runtime Stage
# ----------------------
FROM debian:bookworm-slim

RUN apt-get update && apt-get install -y \
    libssl3 \
    ca-certificates \
    gettext-base \
&& rm -rf /var/lib/apt/lists/*

WORKDIR /app

# CORRIGIDO: Copia o config.toml diretamente
COPY config/config.toml /app/config.toml

# CORRIGIDO: Removidas as linhas de entrypoint.sh e config.template.toml
# COPY config.template.toml /app/config.template.toml
# COPY entrypoint.sh /app/entrypoint.sh
# RUN chmod +x /app/entrypoint.sh

# Copia binário do bot
COPY --from=builder /usr/src/app/target/release/flashloan-bot /usr/local/bin/

# Expõe porta de métricas
EXPOSE 9090

# CORRIGIDO: Executa o binário do bot diretamente
CMD ["/usr/local/bin/flashloan-bot"]