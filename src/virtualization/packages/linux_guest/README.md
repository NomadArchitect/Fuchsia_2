# Linux Guest

See `src/virtualization/README.md` for more general information.

## Updating Linux image

Repeat each of the following steps for ARCH=x64 and ARCH=arm64.

Run the script to fetch the remote branch `machina-4.18` and build Linux:
```
$ ./src/virtualization/packages/linux_guest/mklinux.sh \
  -l /tmp/linux/source \
  -o prebuilt/virtualization/packages/linux_guest/images/${ARCH}/Image \
  -b machina-4.18 \
  ${ARCH}
```

Note: `-b` specifies the branch of `zircon_guest` to use. You can modify
this value if you need a different version or omit it to use a local
version.

Build the sysroot:
```
$ ./src/virtualization/packages/linux_guest/mksysroot.sh \
  -u \
  -o prebuilt/virtualization/packages/linux_guest/images/${ARCH}/disk.img \
  -d /tmp/toybox \
  -s /tmp/dash \
  S{ARCH}
```

Build the tests image:
```
$ ./src/virtualization/packages/linux_guest/mktests.sh \
  -u \
  -o prebuilt/virtualization/packages/linux_guest/images/${ARCH}/tests.img \
  -d /tmp/linux-tests \
  S{ARCH}
```

Ensure that `linux_guest` is working correctly. Then upload the images
to CIPD using `fx cipd`, as described below. `fx cipd auth-login` must
be executed once before running the following commands.

Use the git revision hash from
`zircon-guest.googlesource.com/third_party/ linux` as the
`kernel_git_revision` tag and from
`zircon-guest.googlesource.com/linux-tests` as the `tests_git_revision`
tag.

```
$ fx cipd create \
  -in prebuilt/virtualization/packages/linux_guest/images/${ARCH} \
  -name fuchsia_internal/linux/linux_guest-<version>-${ARCH} \
  -install-mode copy \
  -tag "kernel_git_revision:<git revision>" \
  -tag "tests_git_revision:<git revision>"
```

Then update `integration/fuchsia/prebuilts` to point to the new version using
the instance ID of the package you created. The instance ID is printed in the
output of the `create` command, but if you missed it, you can find the instance
ID with CIPD like so:

```
$ fx cipd search \
  fuchsia_internal/linux/linux_guest-<version>-${ARCH} \
  -tag "kernel_git_revision:<git revision>" \
  -tag "tests_git_revision:<git revision>"
```

## Updating the Linux kernel version

Create a branch within `zircon-guest.googlesource.com/third_party/linux` with
naming scheme `machina-X.XX` where `X.XX` is the kernel version. Make sure to
import all the machina defconfig files from the latest branch. Make sure
`linux_guest` works correctly before updating the images as above. Please also
update the instructions above, and in bin/guest/README.md, to use the most
recent branch.
