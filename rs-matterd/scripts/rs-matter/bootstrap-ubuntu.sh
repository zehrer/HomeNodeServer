#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"
DEB_ARCH="${DEB_ARCH:-armhf}"

if ! command -v apt-get >/dev/null 2>&1; then
    echo "This bootstrap script expects an apt-based Linux host." >&2
    exit 1
fi

SUDO=""
if [[ "$(id -u)" -ne 0 ]]; then
    if ! command -v sudo >/dev/null 2>&1; then
        echo "sudo is required to install host dependencies." >&2
        exit 1
    fi
    SUDO="sudo"
fi

APT_GET="${SUDO} apt-get"
native_arch="$(dpkg --print-architecture)"
os_id="${ID:-}"
os_codename="${VERSION_CODENAME:-}"

# shellcheck source=/etc/os-release
if [[ -r /etc/os-release ]]; then
    . /etc/os-release
    os_id="${ID:-${os_id}}"
    os_codename="${VERSION_CODENAME:-${os_codename}}"
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
    ${SUDO} dpkg --add-architecture "${DEB_ARCH}"
    need_apt_update=1
fi

configure_ubuntu_armhf_sources() {
    local main_sources="/etc/apt/sources.list.d/ubuntu.sources"
    local armhf_sources="/etc/apt/sources.list.d/ubuntu-armhf.sources"

    if [[ "${os_id}" != "ubuntu" || -z "${os_codename}" || ! -f "${main_sources}" ]]; then
        return
    fi

    if ! grep -q "^Architectures: ${native_arch}\$" "${main_sources}"; then
        echo "Constraining native Ubuntu sources to ${native_arch}"
        ${SUDO} cp "${main_sources}" "${main_sources}.codex.bak"
        ${SUDO} python3 - "${main_sources}" "${native_arch}" <<'PY'
from pathlib import Path
import sys

path = Path(sys.argv[1])
native_arch = sys.argv[2]
paragraphs = path.read_text().strip().split("\n\n")
updated = []
for paragraph in paragraphs:
    lines = paragraph.splitlines()
    if not any(line.startswith("Architectures:") for line in lines):
        insert_at = len(lines)
        for idx, line in enumerate(lines):
            if line.startswith("Components:"):
                insert_at = idx + 1
                break
        lines.insert(insert_at, f"Architectures: {native_arch}")
    updated.append("\n".join(lines))
path.write_text("\n\n".join(updated) + "\n")
PY
        need_apt_update=1
    fi

    if [[ ! -f "${armhf_sources}" ]]; then
        echo "Adding Ubuntu ports sources for ${DEB_ARCH}"
        ${SUDO} tee "${armhf_sources}" >/dev/null <<EOF
Types: deb
URIs: http://ports.ubuntu.com/ubuntu-ports/
Suites: ${os_codename} ${os_codename}-updates ${os_codename}-backports
Components: main restricted universe multiverse
Architectures: ${DEB_ARCH}
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg

Types: deb
URIs: http://ports.ubuntu.com/ubuntu-ports/
Suites: ${os_codename}-security
Components: main restricted universe multiverse
Architectures: ${DEB_ARCH}
Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg
EOF
        need_apt_update=1
    fi
}

configure_ubuntu_armhf_sources

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
    ${APT_GET} update
fi

if [[ "${#missing_packages[@]}" -gt 0 ]]; then
    ${APT_GET} install -y "${missing_packages[@]}"
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
