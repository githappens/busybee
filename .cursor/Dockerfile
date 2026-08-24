# Cloud Agent base image for busybee.
#
# The nix dev shell (flake.nix) is the canonical local toolchain, but Cloud
# Agent VMs do not run nix, so this image installs the same tools directly —
# the exact versions CI pins in .github/workflows/ci.yml, for the same reasons:
#   * GNU make >= 4.4 for the fifo jobserver (Ubuntu ships 4.3) — built from
#     source, as CI does, because no distro package pins the version.
#   * ninja >= 1.13 for its jobserver client — release binary.
#   * pueued >= 4.0, the daemon pueue-lib 0.31 speaks — release binary, a test
#     fixture rather than a dependency we build.
#   * the Rust stable toolchain plus rustfmt and clippy, via rustup, so
#     rust-toolchain.toml resolves without a download on first use.
FROM ubuntu:24.04

ENV DEBIAN_FRONTEND=noninteractive

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        build-essential \
        ca-certificates \
        curl \
        git \
        pkg-config \
        unzip \
        xz-utils \
    && rm -rf /var/lib/apt/lists/*

# GNU make >= 4.4: the jobserver integration tests need the fifo jobserver that
# landed in 4.4, and Ubuntu 24.04 ships 4.3. Built from source into /usr/local,
# which precedes /usr/bin on PATH.
ARG MAKE_VERSION=4.4.1
RUN curl -fsSL "https://ftp.gnu.org/gnu/make/make-${MAKE_VERSION}.tar.gz" | tar xz -C /tmp \
    && cd "/tmp/make-${MAKE_VERSION}" \
    && ./configure --prefix=/usr/local --quiet \
    && make -j"$(nproc)" \
    && make install \
    && cd / && rm -rf "/tmp/make-${MAKE_VERSION}"

# ninja >= 1.13: its jobserver client is what the jobserver tests exercise.
ARG NINJA_VERSION=1.13.0
RUN curl -fsSL -o /tmp/ninja.zip \
        "https://github.com/ninja-build/ninja/releases/download/v${NINJA_VERSION}/ninja-linux.zip" \
    && unzip -o /tmp/ninja.zip -d /usr/local/bin \
    && rm /tmp/ninja.zip \
    && chmod +x /usr/local/bin/ninja

# pueued >= 4.0: the supervisor bzbd submits admitted tasks to. The client
# binary is handy too. Musl release builds, static and portable.
ARG PUEUE_VERSION=4.0.4
RUN curl -fsSL -o /usr/local/bin/pueued \
        "https://github.com/Nukesor/pueue/releases/download/v${PUEUE_VERSION}/pueued-x86_64-unknown-linux-musl" \
    && curl -fsSL -o /usr/local/bin/pueue \
        "https://github.com/Nukesor/pueue/releases/download/v${PUEUE_VERSION}/pueue-x86_64-unknown-linux-musl" \
    && chmod +x /usr/local/bin/pueued /usr/local/bin/pueue

# Rust toolchain via rustup. Installed into /usr/local so it is shared, then
# handed to the agent user (ubuntu) so cargo can write its registry cache and
# resolve rust-toolchain.toml without a permission error.
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/usr/local/bin:/usr/bin:/bin
RUN curl -fsSL https://sh.rustup.rs -o /tmp/rustup-init.sh \
    && sh /tmp/rustup-init.sh -y --no-modify-path --profile minimal \
        --default-toolchain stable -c rustfmt -c clippy \
    && rm /tmp/rustup-init.sh \
    && chmod -R a+w "$RUSTUP_HOME" "$CARGO_HOME"

# ubuntu:24.04 already ships a uid-1000 `ubuntu` user; Cursor runs commands as
# it (see environment.json `user`).
USER ubuntu
