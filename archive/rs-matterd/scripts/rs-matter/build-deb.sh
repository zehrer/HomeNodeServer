#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

PACKAGE_NAME="${PACKAGE_NAME:-rs-matterd}"
TARGET_TRIPLE="${TARGET_TRIPLE:-armv7-unknown-linux-gnueabihf}"
DEB_ARCH="${DEB_ARCH:-armhf}"
UPSTREAM_DIR="${RS_MATTER_UPSTREAM_DIR:-${REPO_ROOT}/build/upstream/rs-matter}"
OVERLAY_SOURCE="${REPO_ROOT}/overlay/${PACKAGE_NAME}"
OVERLAY_DEST="${UPSTREAM_DIR}/packaging-overlay/${PACKAGE_NAME}"
STAGING_ROOT="${REPO_ROOT}/build/staging/${PACKAGE_NAME}"
DIST_DIR="${REPO_ROOT}/dist"
MAINTAINER="${DEB_MAINTAINER:-rs-matter packaging <noreply@example.invalid>}"

"${SCRIPT_DIR}/bootstrap-ubuntu.sh"
"${SCRIPT_DIR}/clone-upstream.sh" >/dev/null

export RUSTUP_HOME="${RUSTUP_HOME:-${HOME}/.rustup}"
export CARGO_HOME="${CARGO_HOME:-${HOME}/.cargo}"
# shellcheck source=/dev/null
source "${CARGO_HOME}/env"

export CARGO_BUILD_TARGET="${TARGET_TRIPLE}"
export CARGO_TARGET_DIR="${UPSTREAM_DIR}/target"
export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER="${CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER:-arm-linux-gnueabihf-gcc}"
export CC_armv7_unknown_linux_gnueabihf="${CC_armv7_unknown_linux_gnueabihf:-arm-linux-gnueabihf-gcc}"
export CXX_armv7_unknown_linux_gnueabihf="${CXX_armv7_unknown_linux_gnueabihf:-arm-linux-gnueabihf-g++}"
export PKG_CONFIG_ALLOW_CROSS=1
export PKG_CONFIG_DIR=
export PKG_CONFIG_PATH=
export PKG_CONFIG_LIBDIR="${PKG_CONFIG_LIBDIR:-/usr/lib/arm-linux-gnueabihf/pkgconfig:/usr/share/pkgconfig}"
export PKG_CONFIG_SYSROOT_DIR="${PKG_CONFIG_SYSROOT_DIR:-/}"

rm -rf "${OVERLAY_DEST}" "${STAGING_ROOT}"
mkdir -p "${OVERLAY_DEST}" "${DIST_DIR}"
cp -R "${OVERLAY_SOURCE}/." "${OVERLAY_DEST}/"

cargo build \
    --release \
    --target "${TARGET_TRIPLE}" \
    --manifest-path "${OVERLAY_DEST}/Cargo.toml"

binary_path="${UPSTREAM_DIR}/target/${TARGET_TRIPLE}/release/${PACKAGE_NAME}"
if [[ ! -x "${binary_path}" ]]; then
    echo "Expected binary not found: ${binary_path}" >&2
    exit 1
fi

upstream_version="$(
    sed -n 's/^version = "\(.*\)"/\1/p' "${UPSTREAM_DIR}/rs-matter/Cargo.toml" | head -n 1
)"
if [[ -z "${upstream_version}" ]]; then
    echo "Failed to determine upstream version from ${UPSTREAM_DIR}/rs-matter/Cargo.toml" >&2
    exit 1
fi

upstream_sha="$(git -C "${UPSTREAM_DIR}" rev-parse --short HEAD)"
package_version="${RS_MATTER_DEB_VERSION:-${upstream_version}+git${upstream_sha}}"
package_file="${DIST_DIR}/${PACKAGE_NAME}_${package_version}_${DEB_ARCH}.deb"

install -d "${STAGING_ROOT}/DEBIAN"
install -d "${STAGING_ROOT}/usr/bin"
install -d "${STAGING_ROOT}/lib/systemd/system"
install -d "${STAGING_ROOT}/etc/default"

install -m 0755 "${binary_path}" "${STAGING_ROOT}/usr/bin/${PACKAGE_NAME}"
install -m 0644 \
    "${REPO_ROOT}/packaging/debian/${PACKAGE_NAME}.service" \
    "${STAGING_ROOT}/lib/systemd/system/${PACKAGE_NAME}.service"
install -m 0644 \
    "${REPO_ROOT}/packaging/debian/${PACKAGE_NAME}.default" \
    "${STAGING_ROOT}/etc/default/${PACKAGE_NAME}"
install -m 0755 \
    "${REPO_ROOT}/packaging/debian/postinst" \
    "${STAGING_ROOT}/DEBIAN/postinst"
install -m 0755 \
    "${REPO_ROOT}/packaging/debian/postrm" \
    "${STAGING_ROOT}/DEBIAN/postrm"

control_template="${REPO_ROOT}/packaging/debian/control.in"
control_file="${STAGING_ROOT}/DEBIAN/control"
sed \
    -e "s|@PACKAGE_NAME@|${PACKAGE_NAME}|g" \
    -e "s|@VERSION@|${package_version}|g" \
    -e "s|@ARCH@|${DEB_ARCH}|g" \
    -e "s|@MAINTAINER@|${MAINTAINER}|g" \
    "${control_template}" > "${control_file}"

dpkg-deb --build --root-owner-group "${STAGING_ROOT}" "${package_file}"

echo "Built package: ${package_file}"
