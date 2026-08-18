#!/usr/bin/env bash
#
# Build a flasheable mujina image for the FutureBit Apollo III
# (Radxa ROCK 5B+ / RK3588).
#
# What this produces
#   An Armbian-based .img (same base family as the vendor's armbi_root,
#   see docs/apollo-iii-boot-contract.md §1) carrying mujina-minerd
#   (aarch64), a systemd unit, boot-time GPIO/PWM exports and pool
#   config. NO bitcoin node, NO ckpool, NO apolloapi/UI — the miner
#   connects straight to a public pool via mujina's Stratum v1 client.
#
# How it works
#   1. Builds mujina-minerd for aarch64 in the pinned Rust container
#      (the repo's own toolchain image family; linux/arm64 runs natively
#      on Apple Silicon, via qemu on x86_64 hosts).
#   2. Clones the Armbian build framework at a pinned commit.
#   3. Stages the rootfs overlay (./overlay + the built binary) into
#      Armbian's userpatches/overlay, plus a customize-image.sh that
#      enables the two units.
#   4. Runs Armbian's build inside its Docker container (docker, or a
#      podman shim — see below) and copies the finished image to
#      ./out/ with a SHA-256 checksum.
#
# This is a heavy, multi-GB build (kernel + rootfs). Run it on a host
# with ~30 GB free and a few hours. It is intentionally NOT run during
# development of the recipe.
#
# Reproducibility pins
#   - Armbian build framework: ARMBIAN_COMMIT (see below; verified at
#     clone time — the script refuses to build on a mismatch).
#   - Rust toolchain image: rust:1.94-bookworm (same version family as
#     the repo's build.Containerfile), Cargo.lock is used --locked.
#   - BOARD/BRANCH/RELEASE below; the vendor image is Armbian-based
#     (armbi_root), so the DTB-provided GPIO/PWM/UART4/i2c-3 bases come
#     from the Armbian RK3588 kernel.
#
# Container engine
#   Armbian's build wrapper calls `docker`. On podman-only hosts this
#   script installs a `docker` shim that execs podman, which is the
#   standard workaround. podman on macOS additionally needs the VM
#   running: `podman machine start` (this project uses podman 6 via
#   Homebrew — see memory).
#
# Usage
#   tools/apollo-iii-image/build-image.sh
#   BOARD=rock-5b-plus RELEASE=bookworm tools/apollo-iii-image/build-image.sh
#
# Output
#   tools/apollo-iii-image/out/Armbian_<...>_rock-5b-plus_*.img  (+ .sha)
#   dd the .img to a microSD card — see README.md in this directory.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ---- Configuration (override via environment) -----------------------------

# Armbian build framework pin: tag v26.11.0-trunk.11.
ARMBIAN_REPO="${ARMBIAN_REPO:-https://github.com/armbian/build.git}"
ARMBIAN_TAG="${ARMBIAN_TAG:-v26.11.0-trunk.11}"
ARMBIAN_COMMIT="${ARMBIAN_COMMIT:-2db577f84818765eedb75a84eed39591b7849755}"

# Board / OS target. rock-5b-plus is the Armbian board name for the
# ROCK 5B+ (RK3588); the vendor image is Armbian with volume armbi_root.
BOARD="${BOARD:-rock-5b-plus}"
BRANCH="${BRANCH:-current}"
RELEASE="${RELEASE:-bookworm}"
BUILD_MINIMAL="${BUILD_MINIMAL:-yes}"
BUILD_DESKTOP="${BUILD_DESKTOP:-no}"
KERNEL_CONFIGURE="${KERNEL_CONFIGURE:-no}"
COMPRESS_IMAGE="${COMPRESS_IMAGE:-sha}"

# Mujina binary build.
RUST_IMAGE="${RUST_IMAGE:-docker.io/library/rust:1.94-bookworm}"
CARGO_TARGET="${CARGO_TARGET:-aarch64-unknown-linux-gnu}"

# Where the finished image goes.
OUT_DIR="${OUT_DIR:-$SCRIPT_DIR/out}"

# ---- Resolve the container engine -----------------------------------------

if command -v docker >/dev/null 2>&1; then
    ENGINE="docker"
elif command -v podman >/dev/null 2>&1; then
    ENGINE="podman"
else
    echo "error: neither docker nor podman found; install one (see README.md)" >&2
    exit 1
fi
echo "== container engine: $ENGINE"

WORK_DIR="${WORK_DIR:-$SCRIPT_DIR/.work}"
ARMBIAN_DIR="$WORK_DIR/armbian-build"
CARGO_CACHE_DIR="$WORK_DIR/cargo-cache"
mkdir -p "$WORK_DIR" "$CARGO_CACHE_DIR" "$OUT_DIR"

# ---- 1. Build mujina-minerd for aarch64 ------------------------------------
echo "== building mujina-minerd for $CARGO_TARGET"

# On Apple Silicon podman runs linux/arm64 natively; on x86_64 hosts this
# needs qemu-user emulation (podman handles it if qemu is installed).
$ENGINE run --rm --platform linux/arm64 \
    -v "$REPO_ROOT":/src:ro \
    -v "$CARGO_CACHE_DIR":/usr/local/cargo/registry \
    -w /src \
    "$RUST_IMAGE" \
    bash -euxo pipefail -c '
        rustup target add '"$CARGO_TARGET"'
        apt-get update && apt-get install -y --no-install-recommends \
            pkg-config libudev-dev libssl-dev
        cargo build --release --locked -p mujina-miner --bin mujina-minerd \
            --target '"$CARGO_TARGET"'
    '

BINARY="$REPO_ROOT/target/$CARGO_TARGET/release/mujina-minerd"
[ -x "$BINARY" ] || { echo "error: $BINARY not produced" >&2; exit 1; }

# ---- 2. Clone the Armbian build framework at the pin ------------------------

echo "== fetching Armbian build framework $ARMBIAN_TAG"
if [ ! -d "$ARMBIAN_DIR/.git" ]; then
    git clone --depth 1 --branch "$ARMBIAN_TAG" "$ARMBIAN_REPO" "$ARMBIAN_DIR"
fi
cd "$ARMBIAN_DIR"
git fetch --depth 1 origin tag "$ARMBIAN_TAG" 2>/dev/null || true
git checkout --detach "$ARMBIAN_TAG" 2>/dev/null || git checkout --detach "$ARMBIAN_COMMIT"
ACTUAL_COMMIT="$(git rev-parse HEAD)"
if [ "$ACTUAL_COMMIT" != "$ARMBIAN_COMMIT" ]; then
    echo "error: Armbian pin mismatch: expected $ARMBIAN_COMMIT, got $ACTUAL_COMMIT" >&2
    echo "       update ARMBIAN_COMMIT in $0 (or rebuild after a deliberate pin bump)" >&2
    exit 1
fi
echo "== Armbian build framework pinned at $ACTUAL_COMMIT"

# ---- 3. Stage the rootfs overlay --------------------------------------------

echo "== staging mujina overlay"
mkdir -p "$ARMBIAN_DIR/userpatches/overlay"
# Fresh copy every build so stale files never leak into the image.
rm -rf "$ARMBIAN_DIR/userpatches/overlay"/*
cp -a "$SCRIPT_DIR/overlay/." "$ARMBIAN_DIR/userpatches/overlay/"
mkdir -p "$ARMBIAN_DIR/userpatches/overlay/usr/local/bin"
cp "$BINARY" "$ARMBIAN_DIR/userpatches/overlay/usr/local/bin/mujina-minerd"
chmod 0755 "$ARMBIAN_DIR/userpatches/overlay/usr/local/bin/mujina-minerd"

# lib.config: build-time variables for the Armbian framework.
cat > "$ARMBIAN_DIR/userpatches/lib.config" <<'EOF'
# Generated by tools/apollo-iii-image/build-image.sh — do not edit.
KERNEL_CONFIGURE="no"
BUILD_MINIMAL="yes"
BUILD_DESKTOP="no"
COMPRESS_IMAGE="sha"
EXTRAWIFI="no"
INSTALL_HEADERS="no"
INSTALL_KERNEL_SOURCE="no"
EOF

# customize-image.sh runs inside the chroot at the end of the build.
cat > "$ARMBIAN_DIR/userpatches/customize-image.sh" <<'EOF'
#!/bin/bash
# Generated by tools/apollo-iii-image/build-image.sh — do not edit.
set -e
chmod 0755 /usr/local/sbin/apollo-iii-exports.sh
chmod 0755 /usr/local/bin/mujina-minerd
systemctl enable apollo-iii-exports.service
systemctl enable apollo-iii-mujina.service
EOF
chmod 0755 "$ARMBIAN_DIR/userpatches/customize-image.sh"

# ---- 4. Run the build ---------------------------------------------------------
#
# Armbian's ./compile.sh docker wraps the whole build in its container.
# On podman-only hosts we shim `docker` -> podman. BRANCH defaults to
# "current" (mainline kernel); rock-5b-plus also ships a "vendor"
# (Radxa BSP) branch — switch BRANCH if the mainline DTB is missing a
# controller the board needs (see README "Open questions").
echo "== starting Armbian build (heavy) — BOARD=$BOARD BRANCH=$BRANCH RELEASE=$RELEASE"

if [ "$ENGINE" = "podman" ]; then
    SHIM_DIR="$WORK_DIR/docker-shim"
    mkdir -p "$SHIM_DIR"
    cat > "$SHIM_DIR/docker" <<'EOF'
#!/bin/sh
exec podman "$@"
EOF
    chmod 0755 "$SHIM_DIR/docker"
    export PATH="$SHIM_DIR:$PATH"
fi

export BOARD BRANCH RELEASE BUILD_MINIMAL BUILD_DESKTOP KERNEL_CONFIGURE COMPRESS_IMAGE
./compile.sh docker

# ---- 5. Collect the image ------------------------------------------------------
echo "== collecting image"
IMG="$(ls -1 output/images/Armbian_*"$BOARD"*.img 2>/dev/null | head -n1 || true)"
if [ -z "$IMG" ]; then
    echo "error: no .img under output/images — see the Armbian build log above" >&2
    exit 1
fi
cp "$IMG" "$OUT_DIR/"
if [ -f "${IMG}.sha" ]; then
    cp "${IMG}.sha" "$OUT_DIR/"
fi
SHA="$(sha256sum "$OUT_DIR/$(basename "$IMG")" | cut -d' ' -f1)"
echo
echo "== done"
echo "   image:  $OUT_DIR/$(basename "$IMG")"
echo "   sha256: $SHA"
echo "   flash:  dd if=$OUT_DIR/$(basename "$IMG") of=/dev/<sdX> bs=4M status=progress conv=fsync"
echo "   (identify /dev/<sdX> carefully — see README.md flashing guide)"
