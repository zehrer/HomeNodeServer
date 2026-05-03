#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/../.." && pwd)"

UPSTREAM_URL="${RS_MATTER_UPSTREAM_URL:-https://github.com/project-chip/rs-matter.git}"
UPSTREAM_REF="${RS_MATTER_UPSTREAM_REF:-main}"
UPSTREAM_DIR="${RS_MATTER_UPSTREAM_DIR:-${REPO_ROOT}/build/upstream/rs-matter}"

mkdir -p "$(dirname "${UPSTREAM_DIR}")"

if [[ ! -d "${UPSTREAM_DIR}/.git" ]]; then
    git clone "${UPSTREAM_URL}" "${UPSTREAM_DIR}"
else
    current_origin="$(git -C "${UPSTREAM_DIR}" remote get-url origin)"
    if [[ "${current_origin}" != "${UPSTREAM_URL}" ]]; then
        git -C "${UPSTREAM_DIR}" remote set-url origin "${UPSTREAM_URL}"
    fi
fi

git -C "${UPSTREAM_DIR}" fetch --tags --prune origin

if git -C "${UPSTREAM_DIR}" rev-parse --verify --quiet "${UPSTREAM_REF}^{commit}" >/dev/null; then
    resolved_ref="${UPSTREAM_REF}"
else
    resolved_ref="origin/${UPSTREAM_REF}"
fi

git -C "${UPSTREAM_DIR}" checkout --detach "${resolved_ref}"
git -C "${UPSTREAM_DIR}" submodule update --init --recursive

echo "${UPSTREAM_DIR}"
