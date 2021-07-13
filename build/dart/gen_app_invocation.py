#!/usr/bin/env python3.8
# Copyright 2017 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import os
import stat
import string
import sys


def main():
    parser = argparse.ArgumentParser(
        description='Generate a script that invokes a Dart application')
    parser.add_argument(
        '--out', help='Path to the invocation file to generate', required=True)
    parser.add_argument('--dart', help='Path to the Dart binary', required=True)
    parser.add_argument(
        '--snapshot', help='Path to the app snapshot', required=True)
    args = parser.parse_args()

    app_file = args.out
    app_path = os.path.dirname(app_file)
    if not os.path.exists(app_path):
        os.makedirs(app_path)

    # `dart` and `snapshot` are used in the output app script, use absolute path
    # so the script would work regardless where it's invoked.
    abs_dart = os.path.abspath(args.dart)
    abs_snapshot = os.path.abspath(args.snapshot)

    script_content = f'''#!/bin/sh

{abs_dart} \\
  {abs_snapshot} \\
  "$@"
'''
    with open(app_file, 'w') as file:
        file.write(script_content)
    permissions = (
        stat.S_IRUSR | stat.S_IWUSR | stat.S_IXUSR | stat.S_IRGRP |
        stat.S_IWGRP | stat.S_IXGRP | stat.S_IROTH)
    os.chmod(app_file, permissions)


if __name__ == '__main__':
    sys.exit(main())
