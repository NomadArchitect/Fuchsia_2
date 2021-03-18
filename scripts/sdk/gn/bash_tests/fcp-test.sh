#!/bin/bash
# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.
#
# Test that verifies that fcp correctly discovers and invokes sfp correctly.

set -e

# Sets up an sftp mock binary on the $PATH of any subshells.
setup_sftp() {
  PATH_DIR_FOR_TEST="${BT_TEMP_DIR}/_isolated_path_for"
  export PATH="${PATH_DIR_FOR_TEST}:${PATH}"

  # The side effect of sftp captures the stdin and saves it to sftp.captured_stdin
  cat > "${PATH_DIR_FOR_TEST}/sftp.mock_side_effects" <<INPUT
  read line
  echo "\${line}" >> "${PATH_DIR_FOR_TEST}/sftp.captured_stdin"
INPUT
}

# Sets up a ffx mock. The implemented mock aims to produce minimal
# output that parses correctly but is otherwise uninteresting.
setup_ffx() {
  cat >"${MOCKED_FFX}.mock_side_effects" <<"EOF"
  if [[ "$*" =~ "--format s" ]]; then
    echo fe80::c0ff:eec0:ffee%coffee coffee-coffee-coffee-coffee
  else
    echo fe80::c0ff:eec0:ffee%coffee
  fi
EOF
}

# Helpers.

# Verifies that the correct sftp command is run by fcp.
TEST_fcp() {
  setup_sftp
  setup_ffx

  # Run command.
  BT_EXPECT "${BT_TEMP_DIR}/scripts/sdk/gn/base/bin/fcp.sh"  version.txt /tmp/version.txt

  # Verify that sftp was run correctly.
  # shellcheck disable=SC1090
  source "${PATH_DIR_FOR_TEST}/sftp.mock_state"

  gn-test-check-mock-args _ANY_ -F "${FUCHSIA_WORK_DIR}/sshconfig"  _ANY_ _ANY_ \[fe80::c0ff:eec0:ffee%coffee\]

  # copy to host
    # Run command.
  BT_EXPECT "${BT_TEMP_DIR}/scripts/sdk/gn/base/bin/fcp.sh"  --to-host /config/build-info/version version.txt

  # Verify that sftp was run correctly.
  # shellcheck disable=SC1090
  source "${PATH_DIR_FOR_TEST}/sftp.mock_state.2"

  gn-test-check-mock-args _ANY_ -F "${FUCHSIA_WORK_DIR}/sshconfig"  _ANY_ _ANY_ \[fe80::c0ff:eec0:ffee%coffee\]

  expected_cmds="put \"version.txt\" \"/tmp/version.txt\"
get \"/config/build-info/version\" \"version.txt\""

  BT_EXPECT_FILE_CONTAINS "${PATH_DIR_FOR_TEST}/sftp.captured_stdin" "${expected_cmds}"

  # Check using explicit key
  BT_EXPECT "${BT_TEMP_DIR}/scripts/sdk/gn/base/bin/fcp.sh" --private-key other_key_file.txt  --to-host /config/build-info/version version.txt
  # shellcheck disable=SC1090
  source "${PATH_DIR_FOR_TEST}/sftp.mock_state.3"
  gn-test-check-mock-args _ANY_ -F "${FUCHSIA_WORK_DIR}/sshconfig" -i "other_key_file.txt"  _ANY_ _ANY_ \[fe80::c0ff:eec0:ffee%coffee\]
}

TEST_fcp_with_props() {
  setup_sftp
  setup_ffx

  cat >"${MOCKED_FCONFIG}.mock_side_effects" <<"EOF"

  if [[ "$1" == "get" ]]; then
    if [[ "${2}" == "device-ip" ]]; then
      echo "192.1.1.2"
      return 0
    fi
    echo ""
  fi
EOF

 # Run command.
  BT_EXPECT "${BT_TEMP_DIR}/scripts/sdk/gn/base/bin/fcp.sh"  version.txt /tmp/version.txt

  # Verify that sftp was run correctly.
  # shellcheck disable=SC1090
  source "${PATH_DIR_FOR_TEST}/sftp.mock_state"

  gn-test-check-mock-args _ANY_ -F "${FUCHSIA_WORK_DIR}/sshconfig"  _ANY_ _ANY_ 192.1.1.2

}

# Test initialization.
# shellcheck disable=SC2034
BT_FILE_DEPS=(
  scripts/sdk/gn/base/bin/fconfig.sh
  scripts/sdk/gn/base/bin/fcp.sh
  scripts/sdk/gn/base/bin/fuchsia-common.sh
  scripts/sdk/gn/bash_tests/gn-bash-test-lib.sh
)

# shellcheck disable=SC2034
BT_MOCKED_TOOLS=(
  "scripts/sdk/gn/base/tools/x64/fconfig"
  "scripts/sdk/gn/base/tools/arm64/fconfig"
  "scripts/sdk/gn/base/tools/x64/ffx"
  "scripts/sdk/gn/base/tools/arm64/ffx"
  _isolated_path_for/sftp
)

BT_SET_UP() {
  # shellcheck disable=SC1090
  source "${BT_TEMP_DIR}/scripts/sdk/gn/bash_tests/gn-bash-test-lib.sh"

  # Make "home" directory in the test dir so the paths are stable."
  mkdir -p "${BT_TEMP_DIR}/test-home"
  export HOME="${BT_TEMP_DIR}/test-home"
  FUCHSIA_WORK_DIR="${HOME}/.fuchsia"

  MOCKED_FCONFIG="${BT_TEMP_DIR}/scripts/sdk/gn/base/$(gn-test-tools-subdir)/fconfig"
  MOCKED_FFX="${BT_TEMP_DIR}/scripts/sdk/gn/base/$(gn-test-tools-subdir)/ffx"
}

BT_RUN_TESTS "$@"
