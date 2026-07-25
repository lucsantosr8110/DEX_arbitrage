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
# Override .cargo/config.toml (target-cpu=native) para imagem portavel
COPY . .
RUN RUSTFLAGS="-C target-cpu=x86-64" cargo build --release

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

# Config: bot procura config/config.toml (main.rs:125), preservar estrutura de diretorios
COPY config/ /app/config/

# ABIs necessarias em runtime (utils/abi_loader.rs le de disco)
COPY abi/ /app/abi/

# CORRIGIDO: Removidas as linhas de entrypoint.sh e config.template.toml
# COPY config.template.toml /app/config.template.toml
# COPY entrypoint.sh /app/entrypoint.sh
# RUN chmod +x /app/entrypoint.sh

# Copia binário do bot
COPY --from=builder /usr/src/app/target/release/flashloan-bot /usr/local/bin/

# Porta de metricas Prometheus ([prometheus].port = 9100)
EXPOSE 9100

# CORRIGIDO: Executa o binário do bot diretamente
CMD ["/usr/local/bin/flashloan-bot"]