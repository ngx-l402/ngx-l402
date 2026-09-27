# Building from Source

## Prerequisites

Install required system dependencies:

```bash
sudo apt-get install -y \
  build-essential \
  clang \
  libclang-dev \
  libc6-dev \
  zlib1g-dev \
  pkg-config \
  libssl-dev \
  protobuf-compiler \
  nginx
```

Install Rust and Cargo:

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

---

## Build Steps

1. Clone the repository:

```bash
git clone https://github.com/ngx-l402/ngx-l402.git
cd ngx-l402
```

2. Download Nginx source and build the module:

> ⚠️ **Important**: You must download the exact Nginx source code matching your target version and set the `NGINX_SOURCE_DIR` environment variable before building.

```bash
# Example for Nginx 1.28.0
curl -fsSL http://nginx.org/download/nginx-1.28.0.tar.gz -o nginx.tar.gz
tar -xzf nginx.tar.gz
cd nginx-1.28.0
./configure --with-compat
cd ..
export NGINX_SOURCE_DIR=$(pwd)/nginx-1.28.0

cargo build --release --features export-modules
```

The compiled module will be at `target/release/libngx_l402_lib.so`.

3. Copy to your Nginx modules directory:

```bash
sudo cp target/release/libngx_l402_lib.so /etc/nginx/modules/
```

4. Follow the remaining [manual installation steps](./install-manual.md).

---

## How Worker Processes Exit

When nginx starts, the module registers an exit handler that ends each exiting process with `_exit()`. This stops a worker crashing while OpenSSL frees its state at exit and the module's background threads are still using it ([#201](https://github.com/ngx-l402/ngx-l402/issues/201)).

- **Earlier exit handlers are skipped.** Exit handlers run in reverse order, so this one skips every handler registered before the module loaded, not only OpenSSL's: nginx core's, those of modules loaded earlier, and the stdio flush. nginx writes its logs with `write()`, so no log lines are lost. A module that needs an `atexit` handler to run should load after this one.
- **glibc only.** The handler uses glibc's `on_exit`. On musl builds, such as Alpine, it isn't installed: nginx logs `ngx_l402: fast exit unavailable` at startup, and an exiting worker can still crash in OpenSSL's cleanup. The crash happens only as the worker exits, so no requests are lost, but it shows up as a `SIGSEGV` in the error log. The Docker image is glibc-based and isn't affected.
