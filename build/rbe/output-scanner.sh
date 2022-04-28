#!/bin/bash
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# See usage() for description.

script="$0"
script_dir="$(dirname "$script")"

# The project_root must cover all inputs, prebuilt tools, and build outputs.
# This should point to $FUCHSIA_DIR for the Fuchsia project.
# ../../ because this script lives in build/rbe.
# The value is an absolute path.
project_root="$(readlink -f "$script_dir"/../..)"

build_subdir="$(realpath --relative-to="$project_root" . )"
project_root_rel="$(realpath --relative-to=. "$project_root")"

function usage() {
  cat <<EOF
This wrapper script validates a command in the following manner:

Reject any occurrences of the output dir ($build_subdir):

  1) in the command's tokens
  2) in the output files' paths
  3) in the output files' contents

Usage: $script [options] output_files... -- command...
Options:
  --help | -h : print help and exit
  --label STRING : build system label for this action
  --[no-]execute : check the command without executing it.  [default: execute]
EOF
}

command_break=0
prev_opt=
label=
execute=1
# Collect names of output files.
outputs=()
for opt
do
  # handle --option arg
  if test -n "$prev_opt"
  then
    eval "$prev_opt"=\$opt
    prev_opt=
    shift
    continue
  fi
  # Extract optarg from --opt=optarg
  case "$opt" in
    *=?*) optarg=$(expr "X$opt" : '[^=]*=\(.*\)') ;;
    *=) optarg= ;;
  esac

  case "$opt" in
    -h | --help) usage; exit ;;
    --label) prev_opt=label ;;
    --label=*) label="$optarg" ;;
    --execute) execute=1 ;;
    --no-execute) execute=0 ;;
    --) command_break=1; shift; break ;;
    *) outputs+=( "$opt" ) ;;
  esac
  shift
done

label_diagnostic=
test -z "$label" || label_diagnostic=" [$label]"

function error_msg() {
  echo "[$script]$label_diagnostic Error: " "$@" "(See http://go/remotely-cacheable for more information.)"
}

test "$command_break" = 1 || {
  error_msg "Missing -- before command."
  usage
  exit 1
}

# Scan outputs' paths
err=0
for f in "${outputs[@]}"
do
  if printf %s "$f" | grep -qwF "$build_subdir"
  then
    err=1
    error_msg "Output path '$f' contains '$build_subdir'." \
      "Adding rebase_path(..., root_build_dir) may fix this to be relative." \
      "If this command requires an absolute path, mark this action in GN with 'no_output_dir_leaks = false'."
  fi
done

# Command is in "$@".  Scan its tokens for $build_dir.
for tok
do
  if printf %s "$tok" | grep -qwF "$build_subdir"
  then
    err=1
    error_msg "Command token '$tok' contains '$build_subdir'." \
      "Adding rebase_path(..., root_build_dir) may fix this to be relative." \
      "If this command requires an absolute path, mark this action in GN with 'no_output_dir_leaks = false'."
  fi
  # Do not shift, keep tokens for execution.
done

if [[ "$execute" == 1 ]]
then
  # Run the command.
  "$@"

  status="$?"

  # On success, check the outputs.
  test "$status" != 0 || {
    for f in "${outputs[@]}"
    do
      if grep -qwF "$build_subdir" "$f"
      then
        err=1
        error_msg "Output file $f contains '$build_subdir'." \
          "If this cannot be fixed in the tool, mark this action in GN with 'no_output_dir_leaks = false'."
      fi
      # TODO(http://fxbug.dev/92670) check for known remote paths, like "/b/f/w"
    done
  }
else
  # Skip execution.
  status=0
fi

test "$err" = 0 || exit 1

exit "$status"
