ARG NGX_VERSION=1.28.0

# Build stage
FROM rust:1.93 AS builder
WORKDIR /app

RUN apt-get update && apt-get install -y \
    pkg-config libssl-dev libclang-dev protobuf-compiler make libpcre2-dev zlib1g-dev \
    && rm -rf /var/lib/apt/lists/* \
    && rm -f /usr/bin/gpg /usr/bin/gpg2

COPY . .
ARG NGX_VERSION
ENV NGX_VERSION=${NGX_VERSION}
RUN curl -fsSL https://nginx.org/download/nginx-${NGX_VERSION}.tar.gz -o nginx.tar.gz \
    && tar -xzf nginx.tar.gz \
    && rm nginx.tar.gz \
    && cd nginx-${NGX_VERSION} \
    && ./configure --with-compat
ENV NGINX_SOURCE_DIR=/app/nginx-${NGX_VERSION}
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release --features export-modules \
    && cp target/release/libngx_l402_lib.so /tmp/libngx_l402_lib.so

# Runtime stage
FROM nginx:${NGX_VERSION}
EXPOSE 8000

COPY --from=builder /tmp/libngx_l402_lib.so /etc/nginx/modules/libngx_l402_lib.so
COPY nginx.conf /etc/nginx/nginx.conf
COPY index.html /usr/share/nginx/html/protected/index.html
COPY index.html /usr/share/nginx/html/protected-timeout/index.html
COPY index.html /usr/share/nginx/html/protected-indefinite/index.html
COPY index.html /usr/share/nginx/html/rate-limited/index.html
COPY index.html /usr/share/nginx/html/realm-a/index.html
COPY index.html /usr/share/nginx/html/realm-b/index.html
COPY index.html /usr/share/nginx/html/api-only/index.html
COPY index.html /usr/share/nginx/html/shadow/index.html
COPY index.html /usr/share/nginx/html/tenant1/index.html
COPY index.html /usr/share/nginx/html/tenant2/index.html

# Cashu data dir. Root owns it and nginx writes through the group; the sticky
# bit lets nginx delete or rename only its own files, so the root-owned wallet
# phrase is out of its reach. A mounted volume hides ownership set at build
# time, so the entrypoint sets it on every start.
RUN printf '%s\n' \
    '#!/bin/sh' \
    'd=$(dirname "${CASHU_DB_PATH:-/app/data/cashu_tokens.db}")' \
    'mkdir -p "$d" || exit 1' \
    '# Earlier images gave nginx everything, wallet files included. Take those' \
    '# back once, while nginx still owns the directory: after that, a wallet' \
    '# file nginx creates stays nginx-owned and the module refuses it.' \
    'if [ "$(stat -c %U "$d")" != root ]; then' \
    '  chown -h root:root "$d/wallet.mnemonic" "$d/wallet.fingerprint" 2>/dev/null' \
    'fi' \
    '# Only the database files nginx writes, and no hard link: one can be another' \
    '# name for the root-owned phrase.' \
    'find "$d" -maxdepth 1 -name "$(basename "${CASHU_DB_PATH:-/app/data/cashu_tokens.db}")*" -links 1 -exec chown -h nginx:nginx {} + || exit 1' \
    'chown root:nginx "$d" && chmod 1770 "$d"' \
    > /docker-entrypoint.d/05-cashu-data-perms.sh \
    && chmod +x /docker-entrypoint.d/05-cashu-data-perms.sh

USER root

ENTRYPOINT ["/docker-entrypoint.sh"]
CMD ["nginx", "-g", "daemon off;"]
