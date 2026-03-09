#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
DEB_ARCH="${DEB_ARCH:-armhf}"

if ! command -v apt-get >/dev/null 2>&1; then
    echo "This bootstrap script expects an apt-based Linux host." >&2
    exit 1
fi

APT_BIN="apt-get"
if [[ "$(id -u)" -ne 0 ]]; then
    if ! command -v sudo >/dev/null 2>&1; then
        echo "sudo is required to install host dependencies." >&2
        exit 1
    fi
    APT_BIN="sudo apt-get"
fi

packages=(
    build-essential
    ca-certificates
    curl
    dpkg-dev
    fakeroot
    file
    gcc-arm-linux-gnueabihf
    git
    libc6-dev-armhf-cross
    "libdbus-1-dev:${DEB_ARCH}"
    pkg-config
    xz-utils
)

need_apt_update=0

if ! dpkg --print-foreign-architectures | grep -qx "${DEB_ARCH}"; then
    echo "Adding foreign architecture: ${DEB_ARCH}"
    ${APT_BIN% apt-get} dpkg --add-architecture "${DEB_ARCH}"
    need_apt_update=1
fi

missing_packages=()
for package in "${packages[@]}"; do
    if ! dpkg-query -W -f='${Status}\n' "${package}" 2>/dev/null | grep -q "install ok installed"; then
        missing_packages+=("${package}")
    fi
done

if [[ "${#missing_packages[@]}" -gt 0 ]]; then
    echo "Installing host packages: ${missing_packages[*]}"
    export DEBIAN_FRONTEND=noninteractive
    need_apt_update=1
fi

if [[ "${need_apt_update}" -eq 1 ]]; then
    export DEBIAN_FRONTEND=noninteractive
    ${APT_BIN} update
fi

if [[ "${#missing_packages[@]}" -gt 0 ]]; then
    ${APT_BIN} install -y "${missing_packages[@]}"
fi

export RUSTUP_HOME="${RUSTUP_HOME:-${HOME}/.rustup}"
export CARGO_HOME="${CARGO_HOME:-${HOME}/.cargo}"

if [[ ! -x "${CARGO_HOME}/bin/rustup" ]]; then
    echo "Installing rustup into ${CARGO_HOME}"
    curl --proto '=https' --tlsv1.2 -fsSL https://sh.rustup.rs | sh -s -- -y --profile minimal
fi

# shellcheck source=/dev/null
source "${CARGO_HOME}/env"

"${CARGO_HOME}/bin/rustup" toolchain install stable --profile minimal
"${CARGO_HOME}/bin/rustup" target add armv7-unknown-linux-gnueabihf --toolchain stable

echo "Bootstrap complete for ${REPO_ROOT}"
