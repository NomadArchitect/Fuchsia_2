#!/bin/bash
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# See usage() for description.

script="$0"
script_basename="$(basename "$script")"
script_dir="$(dirname "$script")"

function msg() {
  echo "[$script_basename]: $@"
}

remote_action_wrapper="$script_dir"/fuchsia-rbe-action.sh
remote_compiler_swapper="$script_dir"/cxx-swap-remote-compiler.sh

# The project_root must cover all inputs, prebuilt tools, and build outputs.
# This should point to $FUCHSIA_DIR for the Fuchsia project.
# ../../ because this script lives in build/rbe.
# The value is an absolute path.
default_project_root="$(readlink -f "$script_dir"/../..)"

function usage() {
cat <<EOF
$script [options] -- C++-command...

Options:
  --help|-h: print this help and exit
  --local: disable remote execution and run the original command locally.
    The --remote-disable fake compiler flag (passed after the -- ) has the same
    effect, and is removed from the executed command.
  --verbose|-v: print debug information, including details about uploads.
  --dry-run: print remote execution command without executing (remote only).
  --save-temps: preserve temporary files

  --project-root: location of source tree which also encompasses outputs
      and prebuilt tools, forwarded to --exec-root in the reclient tools.
      [default: $default_project_root]

  There are two ways to forward options to $remote_action_wrapper,
  most of which are forwarded to 'rewrapper':

    Before -- : all unhandled flags are forwarded to $remote_action_wrapper.

    After -- : --remote-flag=* will be forwarded to $remote_action_wrapper
      and removed from the remote command.

  See '$remote_action_wrapper --help' for additional debug features.

EOF
}

dry_run=0
local_only=0
verbose=0
save_temps=0
# C-preprocessing strategy is determined automatically.
#   integrated: built into the command (common case)
#   local: done separately to produce a .ii intermediate file.
cpreprocess_strategy=integrated
project_root="$default_project_root"
canonicalize_working_dir=true
rewrapper_options=()

# Extract script options before --
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
    --help|-h) usage ; exit ;;
    --dry-run) dry_run=1 ;;
    --local) local_only=1 ;;
    # --fsatrace) trace=1 ;;
    --verbose|-v) verbose=1 ;;
    --save-temps) save_temps=1 ;;
    # --compare) compare=1 ;;
    # --check-determinism) check_determinism=1 ;;
    --project-root=*) project_root="$optarg" ;;
    --project-root) prev_opt=project_root ;;
    # stop option processing
    --) shift; break ;;
    # Forward all other options to rewrapper
    *) rewrapper_options+=( "$opt" ) ;;
  esac
  shift
done
test -z "$prev_out" || { echo "Option is missing argument to set $prev_opt." ; exit 1;}

# realpath doesn't ship with Mac OS X (provided by coreutils package).
# We only want it for calculating relative paths.
# Work around this using Python.
if which realpath 2>&1 > /dev/null
then
  function relpath() {
    local -r from="$1"
    local -r to="$2"
    # Preserve symlinks.
    realpath -s --relative-to="$from" "$to"
  }
else
  # Point to our prebuilt python3.
  python="$(ls "$project_root"/prebuilt/third_party/python3/*/bin/python3)" || {
    echo "*** Python interpreter not found under $project_root/prebuilt/third_party/python3."
    exit 1
  }
  function relpath() {
    local -r from="$1"
    local -r to="$2"
    "$python" -c "import os; print(os.path.relpath('$to', start='$from'))"
  }
fi

build_subdir="$(relpath "$project_root" . )"
project_root_rel="$(relpath . "$project_root")"

detected_os="$(uname -s)"
case "$detected_os" in
  Darwin) readonly HOST_OS="mac" ;;
  Linux) readonly HOST_OS="linux" ;;
  *) echo >&2 "Unknown operating system: $detected_os" ; exit 1 ;;
esac

detected_arch="$(uname -m)"
case "$detected_arch" in
  x86_64) readonly HOST_ARCH="x64" ;;
  *) echo >&2 "Unknown machine architecture: $detected_arch" ; exit 1 ;;
esac

readonly remote_clang_subdir=prebuilt/third_party/clang/linux-x64

_required_remote_tools=(
  "$remote_clang_subdir"
)
_missing_remote_tools=()
test "$HOST_OS" = "linux" && test "$HOST_ARCH" = "x64" || {
  for path in "${_required_remote_tools[@]}"
  do [[ -d "$project_root_rel"/"$path" ]] || _missing_remote_tools+=( "$path" )
  done
}

[[ "${#_missing_remote_tools[@]}" == 0 ]] || {
  msg "Remote building C++ requires prebuilts for linux.  Missing:"
  for path in "${_missing_remote_tools[@]}"
  do echo "        $path"
  done
  msg "Add these prebuilt packages to integration/fuchsia/toolchain.  Example: tqr/563535"
  exit 1
}

cc=
cc_command=()

first_source=
output=
profile_list=

comma_remote_inputs=
comma_remote_outputs=

uses_macos_sdk=0

# Some compiles will need C-preprocessing to be done locally.
cpreprocess_command=()
# Conventionally, C-preprocessing is written to a .i or .ii file.
cpreprocess_output=

prev_opt=
for opt in "$@"
do
  # Copy most command tokens.
  # handle --option arg
  if test -n "$prev_opt"
  then
    eval "$prev_opt"=\$opt
    case "$prev_opt" in
      remote_flag) rewrapper_options+=( "$opt" ) ;;
      comma_remote_inputs) ;;  # Remove this optarg.
      comma_remote_outputs) ;;  # Remove this optarg.
      # Copy all others.
      output)
         cc_command+=( "$opt" )
         # Change C-preprocessing output to produce .ii for C++, .i for C
         case "$cc" in
           *clang++ | *g++ )
             cpreprocess_output="${opt/.o/.ii}" ;;
           *)
             cpreprocess_output="${opt/.o/.i}" ;;
         esac
         cpreprocess_command+=( "$cpreprocess_output" )
         ;;
      *) cc_command+=( "$opt" )
         cpreprocess_command+=( "$opt" )
         ;;
    esac
    prev_opt=
    shift
    continue
  fi

  # Extract optarg from --opt=optarg
  case "$opt" in
    *=?*) optarg=$(expr "X$opt" : '[^=]*=\(.*\)') ;;
    *=) optarg= ;;
  esac

  # Reject absolute paths, for the sake of build artifact portability,
  # and remote-action cache hit benefits.  Some exceptions:
  case "$opt" in
    -fdebug-prefix-map="$project_root"* | \
    -ffile-prefix-map="$project_root"* | \
    -fmacro-prefix-map="$project_root"* | \
    -fcoverage-prefix-map="$project_root"* )
      # -fdebug-prefix-map etc. (clang, gcc) takes an absolute path for
      # the sake of remapping debug paths to canonical prefixes, thus
      # making their outputs reproducible across different build environments.
      # It is up to RBE/reclient to handle these flags transparently.
      ;;
    *"$project_root"*)
      cat <<EOF
Absolute paths are not remote-portable.  Found:
  $opt
Please rewrite the command without absolute paths.
EOF
      exit 1
      ;;
  esac

  case "$opt" in
    # This is equivalent to --local, but passed as a compiler flag,
    # instead of wrapper script flag (before the -- ).
    --remote-disable)
      local_only=1
      shift
      continue
      ;;

    # --remote-inputs signals to the remote action wrapper,
    # and not the actual compiler command.
    --remote-inputs=*)
      comma_remote_inputs="$optarg"
      # Remove this from the actual command to be executed.
      shift
      continue
      ;;
    --remote-inputs)
      prev_opt=comma_remote_inputs
      # Remove this from the actual command to be executed.
      shift
      continue
      ;;

    # --remote-outputs signals to the remote action wrapper,
    # and not the actual compiler command.
    --remote-outputs=*)
      comma_remote_outputs="$optarg"
      # Remove this from the actual command to be executed.
      shift
      continue
      ;;
    --remote-outputs) prev_opt=comma_remote_outputs
      # Remove this from the actual command to be executed.
      shift
      continue
      ;;

    # Redirect these flags to rewrapper.
    --remote-flag=*)
      rewrapper_options+=( "$optarg" )
      # Remove this from the actual command to be executed.
      shift
      continue
      ;;
    --remote-flag) prev_opt=remote_flag
      # Remove this from the actual command to be executed.
      shift
      continue
      ;;

    # This is the (likely prebuilt) cc binary.
    */bin/clang* | */bin/gcc* | */bin/g++* ) cc="$opt" ;;

    -o) prev_opt=output ;;

    # TODO(b/220028444): integrate handling of this flag into reclient
    -fprofile-list=*) profile_list="$optarg" ;;
    -fprofile-list) prev_opt=profile_list ;;

    --sysroot=/Library/Developer/* ) uses_macos_sdk=1 ;;

    --*=* ) ;;  # forward

    # Forward other environment variables (or similar looking).
    *=*) ;;

    # Capture the first named source file as the source-root.
    *.c | *.cc | *.cpp | *.cxx | *.s | *.S)
        test -n "$first_source" || first_source="$opt"
        ;;

    *.a | *.o | *.so | *.so.debug)
        link_arg_files+=( "$build_subdir/$opt" )
        ;;

    # Preserve all other tokens.
    *) ;;
  esac

  # Copy tokens to craft a command for local and remote execution.
  cc_command+=( "$opt" )
  cpreprocess_command+=( "$opt" )
  shift
done

# -E tells the compiler to stop after C-preprocessing
cpreprocess_command+=( -E )
# -fno-blocks works around an issue where C-preprocessing includes
# blocks-featured code when it is not wanted.
case "$cc" in
  */bin/clang*) cpreprocess_command+=( -fno-blocks ) ;;
  # Not a feature of gcc/g++
esac

# Craft a command that consumes a C-preprocessed input.
# This must be constructed in a second pass because there is no ordering
# guarantee between options like -c and -o.
# Change the input to use the .ii file, and remove options that are
# related to preprocessing.
cc_using_ii_command=()
delete_optarg=0
prev_opt=
for opt in "${cc_command[@]}"
do
  cc_using_ii_token="$opt"

  if test -n "$prev_opt"
  then
    eval "$prev_opt"=\$opt
    case "$prev_opt" in
      depfile) ;;  # drop this
      *) cc_using_ii_command+=( "$cc_using_ii_token" ) ;;
    esac
    prev_opt=
    continue
  fi

  case "$opt" in
    -D* | -I* | -isystem* | --sysroot=*)
      cc_using_ii_token= ;;
    -MD )
      cc_using_ii_token= ;;
    -MF )
      prev_opt=depfile
      cc_using_ii_token= ;;
    -mmacosx-version-min=* )
      # Sometimes this is used, sometimes not.  Allow either case.
      cc_using_ii_command+=( -Wno-error=unused-command-line-argument ) ;;
    -stdlib=* )
      cc_using_ii_token= ;;
    *.c | *.cc | *.cpp )
      cc_using_ii_token="$cpreprocess_output"
      ;;
  esac
  test -z "$cc_using_ii_token" || cc_using_ii_command+=( "$cc_using_ii_token" )
done

case "$first_source" in
  # TODO(b/220030106): support remote preprocessing of assembly
  *.S) local_only=1 ;;
esac

# When compilation depends on the Mac OS SDK, C-preprocess locally,
# and remote compile using the intermediate .ii file.
test "$uses_macos_sdk" = 0 || cpreprocess_strategy=local

if test "$local_only" = 1
then
  # TODO: add local file access tracing
  "${cc_command[@]}"
  exit "$?"
fi

# Specify the compiler binary to be uploaded.
cc_relative="$(relpath "$project_root" "$cc")"

# Remove these temporary files on exit.
cleanup_files=()
function cleanup() {
  test "$save_temps" != 0 || rm -f "${cleanup_files[@]}"
}
trap cleanup EXIT

# Collect extra inputs to upload for remote execution.
# Note: these paths are relative to the current working dir ($build_subdir),
# so they need to be adjusted relative to $project_root below, before passing
# them to rewrapper.
extra_inputs=()
test -z "$comma_remote_inputs" ||
  IFS=, read -ra extra_inputs <<< "$comma_remote_inputs"

# Collect extra outputs to download after remote execution.
extra_outputs=()
test -z "$comma_remote_outputs" ||
  IFS=, read -ra extra_outputs <<< "$comma_remote_outputs"


test -z "$profile_list" || {
  extra_inputs+=( "$profile_list" )
}

# Workaround b/239101612: missing gcc support libexec binaries for remote build
case "$cc" in
  *clang* ) ;;
  *gcc* | *g++* )
    _gcc_install_root="$(dirname "$(dirname "$cc")")"
    # * contains a version number
    _gcc_libexec_dir="$(ls -d "$_gcc_install_root"/libexec/gcc/x86_64-elf/* )"
    extra_inputs+=(
      "$_gcc_libexec_dir"/cc1
      "$_gcc_libexec_dir"/cc1plus
      # Only need collect2 if we are linking remotely.
    )
    ;;
esac

extra_inputs_rel_project_root=()
for f in "${extra_inputs[@]}"
do
  extra_inputs_rel_project_root+=( "$(relpath "$project_root" "$f" )" )
done

remote_inputs=(
  "${extra_inputs_rel_project_root[@]}"
)

# RBE backend only has linux-x64 support for now.
compiler_swapper_prefix=()
# Substitute the platform portion of cc_relative with linux-x64 (for remote).
remote_cc_relative="${cc_relative/third_party\/clang\/*\/bin/third_party/clang/linux-x64/bin}"
test "$HOST_OS" = "linux" && test "$HOST_ARCH" = "x64" || {
  remote_inputs+=(
    "$(relpath "$project_root" "$remote_compiler_swapper")"
    "$remote_cc_relative"
  )
  compiler_swapper_prefix+=(
    --remote_wrapper="$remote_compiler_swapper"
  )
}

# List inputs in a file to avoid exceeding shell limit.
inputs_file_list="$output".inputs
mkdir -p "$(dirname "$inputs_file_list")"
(IFS=$'\n' ; echo "${remote_inputs[*]}") > "$inputs_file_list"
cleanup_files+=( "$inputs_file_list" )

# Outputs need to be adjusted relative to the exec_root.
# rewrapper already knows:
#   -o is followed by an output file.
#      We declare this output explicitly in first position, so
#      fuchsia-rbe-action.sh can write to a unique stderr file more easily.
#   -MF is followed by a depfile (an output).
extra_outputs=( "$output" "${extra_outputs[@]}" )
remote_outputs_joined=
test "${#extra_outputs[@]}" = 0 || {
  _remote_outputs_comma="$(printf "${build_subdir}/%s," "${extra_outputs[@]}")"
  remote_outputs_joined="${_remote_outputs_comma%,}"  # get rid of last trailing comma
}

exec_root_flag=()
[[ "$project_root" == "$default_project_root" ]] || \
  exec_root_flag=( "--exec_root=$project_root" )

# --canonicalize_working_dir: coerce the output dir to a constant.
#   This requires that the command be insensitive to output dir, and
#   that its outputs do not leak the remote output dir.
#   Ensuring that the results reproduce consistently across different
#   build directories helps with caching.
remote_cc_command=(
  "$remote_action_wrapper"
  --labels=type=compile,compiler=clang,lang=cpp
  "${exec_root_flag[@]}"
  --canonicalize_working_dir="$canonicalize_working_dir"
#  "${remote_trace_flags[@]}"
  --input_list_paths="$inputs_file_list"
  --output_files="$remote_outputs_joined"
  "${compiler_swapper_prefix[@]}"
  "${rewrapper_options[@]}"
  --
)

case "$cpreprocess_strategy" in
  integrated)
    remote_cc_command+=( "${cc_command[@]}" )
    # Workaround an issue where ZX_DEBUG_ASSERT triggers -Wconstant-logical-operand
    remote_cc_command+=( -Wno-constant-logical-operand )
    ;;
  local)
    cleanup_files+=( "$cpreprocess_output" )
    test "$verbose" = 0 || msg "Local C-preprocessing: ${cpreprocess_command[@]}"
    "${cpreprocess_command[@]}"
    cpp_status="$?"
    test "$cpp_status" = 0 || {
      msg "*** Local C-preprocessing failed (exit=$cpp_status): ${cpreprocess_command[@]}"
      exit "$cpp_status"
    }
    remote_cc_command+=( "${cc_using_ii_command[@]}" )
    ;;
  *) msg "*** Invalid C-preprocessing strategy: $cpreprocess_strategy"
    exit 1
    ;;
esac

if test "$dry_run" = 1
then
  msg "skipped: ${remote_cc_command[@]}"
  exit
fi

test "$verbose" = 0 || msg "remote command: ${remote_cc_command[@]}"
# Cannot `exec` because that would bypass the trap cleanup.
"${remote_cc_command[@]}"
status="$?"

test "$status" = 0 || test "$verbose" = 1 || test "$cpreprocess_strategy" = integrated || {
  msg "local C-preprocessing was: ${cpreprocess_command[@]}"
}

exit "$status"
