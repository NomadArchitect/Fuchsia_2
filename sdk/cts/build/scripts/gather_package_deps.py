#!/usr/bin/env python3.8
# Copyright 2020 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import json
import os
import re
import shutil
import sys
import tarfile


class GatherPackageDeps:
    """Helper class to take a `package_manifest.json` and copy all files referenced
    into an archive that will then be available at runtime.

    Args:
      package_json_path (string): An absolute path to the package's `package_manifest.json` file.
      meta_far_path (string): An absolute path to the package's `meta.far` file.
      output_dir (string): The absolute path to the directory that this should output into.

    Raises: ValueError if any parameter is empty.
    """

    # Selects everything that comes after '/' and/or any number of '../'.
    path_stripper = re.compile(r'(?:(?:\.\.\/)+)?\/?(.+)')

    def __init__(self, package_json_path, meta_far_path, output_dir, depfile):
        if package_json_path and os.path.exists(package_json_path):
            self.package_json_path = package_json_path
        else:
            raise ValueError('package_json_path must be to a valid file')

        if meta_far_path and os.path.exists(meta_far_path):
            self.meta_far_path = meta_far_path
        else:
            raise ValueError('meta_far_path must be to a valid file')

        if output_dir:
            self.output_dir = output_dir
        else:
            raise ValueError('output_dir cannot be empty')

        if depfile:
            self.depfile = depfile
        else:
            raise ValueError('depfile cannot be empty')

    def parse_package_json(self):
        manifest_dict = {}
        with open(self.package_json_path) as f:
            data = json.load(f)
            for file in data['blobs']:
                if file['path'].startswith('meta/'):
                    continue
                manifest_dict[file['path']] = file['source_path']
        return manifest_dict

    def copy_meta_far(self):
        shutil.copyfile(
            self.meta_far_path, os.path.join(self.output_dir, 'meta.far'))

    def copy_to_output_dir(self, manifest_dict):
        for archive_path, source_path in manifest_dict.items():
            # Some `source_path`s are prefixed with a couple of `../`'s or are absolute paths.
            # Examples:
            #    "../../prebuilt/third_party/clang/linux-x64/lib/aarch64-unknown-fuchsia/c++/libc++.so.2"
            #    "/root/to/fuchsia/out/core.x64-host_asan/obj/topaz/runtime/flutter_runner/flutter_aot_runner.meta/blobs/2ae9bee944d30eeec29608eb2f5e21df71f92bdb8f75f8c2ea1a2cd8d273915b"
            #
            # All of these files must end up in our `output_dir`, where `../`s are meaningless and
            # wrong, and absolute paths are definitely wrong. All of these must be cleaned up into
            # useable relative paths - relative to the `output_dir`.
            # The above examples should become:
            #    "prebuilt/third_party/clang/linux-x64/lib/aarch64-unknown-fuchsia/c++/libc++.so.2"
            #    "root/to/fuchsia/out/core.x64-host_asan/obj/topaz/runtime/flutter_runner/flutter_aot_runner.meta/blobs/2ae9bee944d30eeec29608eb2f5e21df71f92bdb8f75f8c2ea1a2cd8d273915b"
            #
            # These are the paths as they will appear within the output `tar` file and will be how
            # they are referenced within the manifest file.
            m = self.path_stripper.match(source_path)
            out_path = os.path.join(self.output_dir, m.group(1))
            os.makedirs(os.path.dirname(out_path), exist_ok=True)
            shutil.copyfile(source_path, out_path)
            manifest_dict[archive_path] = m.group(1)

    def write_new_manifest(self, manifest_dict):
        with open(os.path.join(self.output_dir, 'package.manifest'), 'w') as f:
            for archive_path, source_path in manifest_dict.items():
                f.write(archive_path + '=' + source_path + '\n')
            f.write('meta/package=meta.far\n')

    def archive_output(self, tar_path):
        # Explicitly use the GNU_FORMAT because the current dart library
        # (v.3.0.0) does not support parsing other tar formats that allow for
        # filenames longer than 100 characters.
        with tarfile.open(tar_path, 'w', format=tarfile.GNU_FORMAT) as tar:
            for root, _, files in os.walk(self.output_dir):
                for name in files:
                    relative_dir = os.path.relpath(root, self.output_dir)
                    input_path = os.path.join(root, name)
                    relative_path = os.path.join(relative_dir, name)
                    if input_path == tar_path:
                        continue
                    tar.add(input_path, arcname=relative_path)
                    # Removes files added to archive otherwise they'll be
                    # considered as unexpected outputs.
                    os.remove(input_path)

    def run(self):
        manifest_dict = self.parse_package_json()

        # Record deps before manifest_dict is updated.
        deps = ' '.join(manifest_dict.values())

        self.copy_meta_far()
        self.copy_to_output_dir(manifest_dict)
        self.write_new_manifest(manifest_dict)
        tar_path = os.path.join(self.output_dir, 'package.tar')
        self.archive_output(tar_path)

        with open(self.depfile, 'w') as f:
            f.write(f'{tar_path}: {deps}\n')


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument(
        '--package_json_path',
        required=True,
        help=
        'The path to the package_manifest.json generated by a `fuchsia_package`.'
    )
    parser.add_argument(
        '--meta_far_path',
        required=True,
        help='The path to the package\'s meta.far.')
    parser.add_argument(
        '--output_dir',
        required=True,
        help=
        'The path to where the new manifest and all required files will be copied to.'
    )
    parser.add_argument(
        '--depfile',
        required=True,
        help='The path to write a depfile, see depfile from GN.',
    )
    args = parser.parse_args()

    try:
        gatherer = GatherPackageDeps(
            args.package_json_path, args.meta_far_path, args.output_dir,
            args.depfile).run()
    except Exception as e:
        print('GatherPackageDeps errored during run: %s' % e)
        return 1

    return 0


if __name__ == '__main__':
    sys.exit(main())
