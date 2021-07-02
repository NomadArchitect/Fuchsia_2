# Download the Fuchsia source code

This guide provides instructions on how to set up the Fuchsia development
environment on your machine for building Fuchsia from source.

The steps are:

1. [Prerequisites](#prerequisites).
2. [Download the Fuchsia source code](#download-fuchsia-source).
3. [Set up environment variables](#set-up-environment-variables).

## Prerequisites

Before you start, complete the following tasks:

* [Run preflight](#run-preflight).
* [Install required packages](#install-required-packages).

### Run preflight {#run-preflight}

We recommend you run `ffx platform preflight`, which examines your machine
and informs you of issues that may affect building Fuchsia from source or
running the Fuchsia emulator.

At this time the `preflight` tool is only provided as an x64 prebuilt. Fuchsia
is currently not guaranteed to build successfully on other host architectures.

* {Linux}

  ```posix-terminal
  curl -sO https://storage.googleapis.com/fuchsia-ffx/ffx-linux-x64 && chmod +x ffx-linux-x64 && ./ffx-linux-x64 platform preflight
  ```

* {macOS}

  ```posix-terminal
  curl -sO https://storage.googleapis.com/fuchsia-ffx/ffx-macos-x64 && chmod +x ffx-macos-x64 && ./ffx-macos-x64 platform preflight
  ```

### Install required packages {#install-required-packages}

The Fuchsia project requires `curl`, `unzip`, and `git` to be up to date.

Note: Fuchsia requires the version of Git to be 2.28 or higher.

* {Linux}

  Install (or update) the following packages:

  ```posix-terminal
  sudo apt-get install curl git unzip
  ```

* {macOS}

  Install the Xcode command line tools:

  Note: Skip this step if `ffx platform preflight` discovers that Xcode tools
  are installed on your machine.

  ```posix-terminal
  xcode-select --install
  ```

## Download Fuchsia source {#download-fuchsia-source}

Fuchsia's [bootstrap script](/scripts/bootstrap) creates a `fuchsia` directory
and downloads the content of the Fuchsia source repository to this new
directory.

Note: Downloading Fuchsia source requires ~2 GiB of space on your machine. In
addition, you will need another 80-90 GiB of space when you build Fuchsia,
depending on your build configuration.

To download the Fuchsia source, do the following:

1.  Select a directory for downloading the Fuchsia source code, for example:

    Note: While you can set up Fuchsia in any directory, this guide uses the
    home directory.

    ```posix-terminal
    cd ~
    ```

1.  Run the bootstrap script:

    ```posix-terminal
    curl -s "https://fuchsia.googlesource.com/fuchsia/+/HEAD/scripts/bootstrap?format=TEXT" | base64 --decode | bash
    ```
    This script creates a `fuchsia` directory to download the source code.
    Downloading Fuchsia source can take up to 60 minutes.

    If you see the `Invalid authentication credentials` error during the
    bootstrapping process, see [Authentication error](#authentication-error) for
    help.

## Set up environment variables {#set-up-environment-variables}

Fuchsia recommends that you update your shell profile to include the following
actions:

*   Add the `.jiri_root/bin` directory to your `PATH`.

    The `.jiri_root/bin` directory in the Fuchsia source contains the
    <code>[jiri](https://fuchsia.googlesource.com/jiri){:.external}</code> and
    <code>[fx](/docs/development/build/fx.md)</code> tools are essential to
    Fuchsia workflows. Fuchsia uses the `jiri` tool to manage repositories in
    the Fuchsia project. The `fx` tool helps configure, build, run, and debug
    Fuchsia. The Fuchsia toolchain requires `jiri` to be available in your
    `PATH`.

*   Source the `scripts/fx-env.sh` file.

    Although it's not required, sourcing the
    <code>[fx-env.sh](/scripts/fx-env.sh)</code> script enables useful shell
    functions in your terminal. For instance, it creates a `FUCHSIA_DIR`
    environment variable and provides the `fd` command for navigating
    directories with auto-completion (see comments in `fx-env.sh` for more
    information).

To update your shell profile with the changes above, do the following:

Note: If you don't wish to update your environment variables, see
[Work on Fuchsia without updating your PATH](#work-on-fuchsia-without-updating-your-path).

1.  Use a text editor to open your `~/.bash_profile` file, for example:

    Note: This guide uses a `bash` terminal as an example. If you are
    using `zsh`, replace `~/.bash_profile` with `~/.zprofile` in the
    following steps:

    ```posix-terminal
    nano ~/.bash_profile
    ```

1.  Add the following lines to your `~/.bash_profile` file:

    Note: If your Fuchsia source code is not located in the `~/fuchsia`
    directory, replace `~/fuchsia` with your Fuchsia directory.

    ```sh
    export PATH=~/fuchsia/.jiri_root/bin:$PATH
    source ~/fuchsia/scripts/fx-env.sh
    ```

1.  Save the file and exit the text editor.

1.  To update your environment variables, run the following command:

    ```posix-terminal
    source ~/.bash_profile
    ```

1.  Verify that you can run the following commands inside your
    `fuchsia` directory without error:

    ```posix-terminal
    jiri help
    ```

    ```posix-terminal
    fx help
    ```

## Next steps

See
[Configure and build Fuchsia](/docs/get-started/build_fuchsia.md)
in the Getting started guide for the next steps.

## Appendices

### Authentication error {#authentication-error}

If you see the `Invalid authentication credentials` error during the bootstrap
process, your `~/.gitcookies` file may contain cookies from some repositories in
`googlesource.com` that the bootstrap script wants to check out anonymously.

To resolve this error, do one of the following:

*   Follow the onscreen directions to get passwords for the specified
    repositories.
*   Delete the offending cookies from the `.gitcookies` file.

### Work on Fuchsia without updating your PATH {#work-on-fuchsia-without-updating-your-path}

The following sections provide alternative approaches to the
[Update your shell script](#update-your-shell-script) section:

*   [Copy the tool to your binary directory](#copy-the-tool-to-your-binary-directory)
*   [Add a symlink to your binary directory](#add-a-symlink-to-your-binary-directory)

#### Copy the tool to your binary directory {#copy-the-tool-to-your-binary-directory}

If you don't wish to update your environment variables, but you want `jiri` to
work in any directory, copy the `jiri` tool to your `~/bin` directory, for
example:

Note: If your Fuchsia source code is not located in the `~/fuchsia` directory,
replace `~/fuchsia` with your Fuchsia directory.

```posix-terminal
cp ~/fuchsia/.jiri_root/bin/jiri ~/bin
```

However, you must have write access to the `~/bin` directory without `sudo`. If
you don't, `jiri` cannot keep itself up-to-date.

#### Add a symlink to your binary directory {#add-a-symlink-to-your-binary-directory}

Similarly, if you want to use the `fx` tool without updating your environment
variables, provide the `fx` tool's symlink in your `~/bin` directory, for
example:

Note: If your Fuchsia source code is not located in the `~/fuchsia` directory,
replace `~/fuchsia` with your Fuchsia directory.

```posix-terminal
ln -s ~/fuchsia/scripts/fx ~/bin
```

Alternatively, run the `fx` tool directly using its path, for example:

```posix-terminal
./scripts/fx help
```

In either case, you need `jiri` in your `PATH`.

