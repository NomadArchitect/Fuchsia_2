#!/usr/bin/env bash

# Copyright 2021 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# This script updates the list of external dependencies in imports.go, which
# allows `go mod` to operate in that directory to update and vendor those
# dependencies.

set -euo pipefail

cd "$(dirname "$0")"

source ../../tools/devshell/lib/vars.sh

# Create various symlinks needed for the `go list` call below.
fx-command-run setup-go

function cleanup() {
  rm "$COBALT_DST"
  find "$TMP" -maxdepth 1 -mindepth 1 -exec mv {} . \;
}
trap cleanup EXIT

# Escape third_party/go.mod.
readonly COBALT_DST=$FUCHSIA_DIR/cobalt
ln -s "$FUCHSIA_DIR"/third_party/cobalt "$COBALT_DST"

# Move jiri-managed repositories out of the module.
TMP=$(mktemp -d)
readonly ignored=$(git check-ignore ./*)
echo "$ignored" | xargs --no-run-if-empty -I % mv % "$TMP"/%

GOROOTBIN=$(fx-command-run go env GOROOT)/bin
GO=$GOROOTBIN/go
GOFMT=$GOROOTBIN/gofmt

IMPORTS=()
for dir in $FUCHSIA_DIR $FUCHSIA_DIR/cobalt $FUCHSIA_DIR/third_party/syzkaller/sys/syz-sysgen; do
  while IFS='' read -r line; do IMPORTS+=("$line"); done < <(cd "$dir" && git ls-files -- \
    '*.go' ':!third_party/golibs/vendor' |
    xargs dirname |
    sort | uniq |
    sed 's|^|./|' |
    xargs "$GO" list -mod=readonly -e -f \
      '{{join .Imports "\n"}}{{"\n"}}{{join .TestImports "\n"}}{{"\n"}}{{join .XTestImports "\n"}}' |
    grep -vF go.fuchsia.dev/fuchsia/ |
    # Apparently we generate these normally checked-in files?
    grep -vF 'go.chromium.org/luci' |
    grep -F . |
    sort | uniq |
    xargs "$GO" list -mod=readonly -e -f \
      '{{if not .Goroot}}_ "{{.ImportPath}}"{{end}}' |
    grep -vF github.com/google/syzkaller/ |
    sort | uniq)
done

IMPORTS_STR=$(
  IFS=$'\n'
  echo "${IMPORTS[*]}"
)

printf '// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package imports

import (\n%s\n)' "$IMPORTS_STR" | $GOFMT -s >imports.go

$GO get -u gvisor.dev/gvisor@go
$GO get -u
$GO mod tidy
$GO mod vendor

"${PREBUILT_PYTHON3_DIR}/bin/python3.8" update_sources.py \
  --build-file='BUILD.gn' \
  --golibs-dir='.' > "${TMP}/BUILD.gn"
mv "${TMP}/BUILD.gn" 'BUILD.gn'
"${PREBUILT_GN}" format 'BUILD.gn'
