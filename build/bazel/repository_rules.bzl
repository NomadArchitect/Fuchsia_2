# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

"""
A set of repository rules used by the Bazel workspace for the Fuchsia
platform build.
"""

def _ninja_target_from_gn_label(gn_label):
    """Convert a GN label into an equivalent Ninja target name"""

    #
    # E.g.:
    #  //build/bazel:something(//build/toolchain/fuchsia:x64)
    #       --> build/bazel:something
    #
    #  //build/bazel/something:something(//....)
    #       --> build/bazel:something
    #
    # This assumes that all labels are in the default toolchain (since
    # otherwise the corresponding Ninja label is far too complex to compute).
    #
    ninja_target = gn_label.split("(")[0].removeprefix("//")
    dir_name, _, target_name = ninja_target.partition(":")
    if dir_name.endswith("/" + target_name):
        ninja_target = dir_name.removesuffix(target_name).removesuffix("/") + ":" + target_name
    return ninja_target

def _bazel_inputs_repository_impl(repo_ctx):
    # Set this constant to True to force Ninja invocation to ensure that
    # all Ninja outputs are properly generated, or False if you want to
    # create empty files or directories if needed.
    #
    # The latter allows performing some Bazel queries before building
    # anything, which might be crucial to later switch from Ninja to Bazel
    # as the main driver for the platform build system.
    INVOKE_NINJA = False

    build_bazel_content = '''# Auto-generated - do not edit

package(
    default_visibility = ["//visibility:public"]
)

exports_files(
    glob(
      ["**"],
      exclude=["ninja_output"],
      exclude_directories=1,
    )
)

'''

    # The Ninja output directory is passed by the launcher script at
    # gen/build/bazel/bazel as an environment variable.
    #
    # This is the root directory for all source entries in the manifest.
    # Create a //:ninja_output symlink in the repository to point to it.
    ninja_output_dir = repo_ctx.os.environ["BAZEL_FUCHSIA_NINJA_OUTPUT_DIR"]
    ninja_executable = repo_ctx.os.environ["BAZEL_FUCHSIA_NINJA_PREBUILT"]
    source_prefix = ninja_output_dir + "/"

    ninja_targets = []

    # //build/bazel/bazel_inputs.gni for the schema definition.
    for entry in json.decode(repo_ctx.read(repo_ctx.attr.inputs_manifest)):
        gn_label = entry["gn_label"]
        content = '''# From GN target: {label}
filegroup(
    name = "{name}",
'''.format(label = gn_label, name = entry["name"])
        if "sources" in entry:
            # A regular filegroup that list sources explicitly.
            content += "    srcs = [\n"
            for src, dst in zip(entry["sources"], entry["destinations"]):
                content += '       "{dst}",\n'.format(dst = dst)
                src_file = source_prefix + src
                repo_ctx.symlink(src_file, dst)

                # If the file doesn't exist because Ninja has not run yet,
                # create an empty file now. This allows Bazel queries to work
                # while the file itself will be overwritten by Ninja later.
                if not INVOKE_NINJA and not repo_ctx.path(src_file).exists:
                    repo_ctx.file(src_file, "")

            content += "    ],\n"
        elif "source_dir" in entry:
            # A directory filegroup which uses glob() to group input files.
            src_dir = source_prefix + entry["source_dir"]
            dst_dir = entry["dest_dir"]
            content += '    srcs = glob(["{dst_dir}**"])\n'.format(dst_dir = dst_dir)
            repo_ctx.symlink(src_dir, dst_dir)

            # If the directory does not exist, create it to allow basic Bazel
            # queries to work before a Ninja invocation.
            if not INVOKE_NINJA and not repo_ctx.path(src_dir).exists:
                repo_ctx.execute(["mkdir", "-p", src_dir])
        else:
            fail("Invalid inputs manifest entry: %s" % entry)

        content += ")\n\n"
        build_bazel_content += content

        # Convert GN label into the corresponding Ninja target.
        ninja_targets.append(_ninja_target_from_gn_label(gn_label))

    repo_ctx.file("BUILD.bazel", build_bazel_content)
    repo_ctx.file("WORKSPACE.bazel", "")
    repo_ctx.file("MODULE.bazel", 'module(name = "{name}", version = "1"),\n'.format(name = repo_ctx.attr.name))

    if INVOKE_NINJA:
        # Invoke Ninja to ensure that the dependencies of all bazel_input_xxx()
        # targets are properly generated.
        repo_ctx.execute(
            [ninja_executable, "-C", ninja_output_dir] + ninja_targets,
            quiet = False,
        )

bazel_inputs_repository = repository_rule(
    implementation = _bazel_inputs_repository_impl,
    attrs = {
        "inputs_manifest": attr.label(
            allow_files = True,
            mandatory = True,
            doc = "Label to the inputs manifest file describing the repository's content",
        ),
    },
    doc = "A repository rule used to populate a workspace with filegroup() entries " +
          "exposing Ninja build outputs as Bazel inputs. Its content is described by " +
          "a Ninja-generated input manifest, a JSON array of objects describing each " +
          "filegroup().",
)
