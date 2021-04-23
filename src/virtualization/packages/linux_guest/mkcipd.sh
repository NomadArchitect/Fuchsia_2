#!/usr/bin/env bash

# Copyright 2018 The Fuchsia Authors
#
# Use of this source code is governed by a MIT-style
# license that can be found in the LICENSE file or at
# https://opensource.org/licenses/MIT

set -eo pipefail

declare -r LINUX_GUEST_DIR="$( cd "$( dirname "${BASH_SOURCE[0]}" )" && pwd )"
declare -r FUCHSIA_DIR=$(git rev-parse --show-toplevel)
declare -r CIPD="${FUCHSIA_DIR}/.jiri_root/bin/cipd"
declare -r SOURCE_DIR="/tmp/linux_guest"
declare -r LINUX_VERSION="4.18"

case "${1}" in
arm64)
  ARCH=${1};;
x64)
  ARCH=${1};;
*)
  echo "usage: ${0} {arm64, x64}"
  exit 1;;
esac

if [[ -z "$(which cargo)" ]]; then
  if [[ -f "$HOME/.cargo/env" ]]; then
    source "$HOME/.cargo/env"
  else
    echo "you must have rust installed on your host - see https://www.rust-lang.org/tools/install"
    exit 2
  fi
fi

if [[ "$(${CIPD} acl-check fuchsia_internal/ -writer)" == *"doesn't"* ]]; then
  ${CIPD} auth-login
fi

# Clean the existing source checkout.
rm -rf "${SOURCE_DIR}"

# Clean the existing images.
declare -r IMAGE_DIR="${FUCHSIA_DIR}/prebuilt/virtualization/packages/linux_guest/images/${ARCH}"
rm -rf "${IMAGE_DIR}/Image"
rm -rf "${IMAGE_DIR}/tests.img"
rm -rf "${IMAGE_DIR}/disk.img"

${LINUX_GUEST_DIR}/mklinux.sh \
    -b "machina-${LINUX_VERSION}" \
    -d "machina_defconfig" \
    -l "${SOURCE_DIR}/linux" \
    -o "${IMAGE_DIR}/Image" \
    ${ARCH}
LINUX_GIT_HASH="$( cd ${SOURCE_DIR}/linux && git rev-parse --verify HEAD )"

${LINUX_GUEST_DIR}/mktests.sh \
    -d "${SOURCE_DIR}/linux-tests" \
    -o "${IMAGE_DIR}/tests.img" \
    -u \
    ${ARCH}
TESTS_GIT_HASH="$( cd ${SOURCE_DIR}/linux-tests && git rev-parse --verify HEAD )"

${LINUX_GUEST_DIR}/mksysroot.sh \
    -d "${SOURCE_DIR}/toybox-${ARCH}" \
    -s "${SOURCE_DIR}/dash" \
    -o "${IMAGE_DIR}/disk.img" \
    -u \
    ${ARCH}
TOYBOX_GIT_HASH="$( cd ${SOURCE_DIR}/toybox-${ARCH} && git rev-parse --verify HEAD )"
DASH_GIT_HASH="$( cd ${SOURCE_DIR}/dash && git rev-parse --verify HEAD )"

declare -r CIPD_PATH="fuchsia_internal/linux/linux_guest-${LINUX_VERSION}-${ARCH}"

${CIPD} create \
    -in "${IMAGE_DIR}" \
    -name "${CIPD_PATH}" \
    -install-mode copy \
    -tag "kernel_git_revision:${LINUX_GIT_HASH}" \
    -tag "tests_git_revision:${TESTS_GIT_HASH}" \
    -tag "toybox_git_revision:${TOYBOX_GIT_HASH}" \
    -tag "dash_git_revision:${DASH_GIT_HASH}"

# Fetch the instance ID of the just-created CIPD package. If more than one
# matches our tags, use the most recent one.
INSTANCE_ID=$(${CIPD} search \
    "${CIPD_PATH}" \
    -tag "kernel_git_revision:${LINUX_GIT_HASH}" \
    -tag "tests_git_revision:${TESTS_GIT_HASH}" \
    -tag "toybox_git_revision:${TOYBOX_GIT_HASH}" \
    -tag "dash_git_revision:${DASH_GIT_HASH}" \
    | grep -v 'Instances:' \
    | cut -d ':' -f 2 \
    | head -1)

echo "Kernel git revision: ${LINUX_GIT_HASH}"
echo "Tests git revision: ${TESTS_GIT_HASH}"
echo "Toybox git revision: ${TOYBOX_GIT_HASH}"
echo "Dash git revision: ${DASH_GIT_HASH}"
echo "Instance ID: ${INSTANCE_ID}"
