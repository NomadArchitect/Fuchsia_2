// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package flasher

import (
	"context"
	"io"
	"io/ioutil"
	"os"
	"os/exec"

	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"golang.org/x/crypto/ssh"
)

type BuildFlasher struct {
	FfxToolPath   string
	FlashManifest string
	sshPublicKey  ssh.PublicKey
	stdout        io.Writer
}

type Flasher interface {
	Flash(ctx context.Context) error
}

// NewBuildFlasher constructs a new flasher that uses `ffxPath` as the path
// to the tool used to flash a device using flash.json located
// at `flashManifest`. Also accepts a number of optional parameters.
func NewBuildFlasher(ffxPath, flashManifest string, options ...BuildFlasherOption) (*BuildFlasher, error) {
	p := &BuildFlasher{
		FfxToolPath:   ffxPath,
		FlashManifest: flashManifest,
	}

	for _, opt := range options {
		if err := opt(p); err != nil {
			return nil, err
		}
	}

	return p, nil
}

type BuildFlasherOption func(p *BuildFlasher) error

// Sets the SSH public key that the Flasher will bake into the device as an
// authorized key.
func SSHPublicKey(publicKey ssh.PublicKey) BuildFlasherOption {
	return func(p *BuildFlasher) error {
		p.sshPublicKey = publicKey
		return nil
	}
}

// Send stdout from the ffx target flash scripts to `writer`. Defaults to the parent
// stdout.
func Stdout(writer io.Writer) BuildFlasherOption {
	return func(p *BuildFlasher) error {
		p.stdout = writer
		return nil
	}
}

// Flash a device with flash.json manifest.
func (p *BuildFlasher) Flash(ctx context.Context) error {
	flasherArgs := []string{}

	// Write out the public key's authorized keys.
	if p.sshPublicKey != nil {
		authorizedKeys, err := ioutil.TempFile("", "")
		if err != nil {
			return err
		}
		defer os.Remove(authorizedKeys.Name())

		if _, err := authorizedKeys.Write(ssh.MarshalAuthorizedKey(p.sshPublicKey)); err != nil {
			return err
		}

		if err := authorizedKeys.Close(); err != nil {
			return err
		}

		flasherArgs = append(flasherArgs, "--authorized-keys", authorizedKeys.Name())
	}
	return p.runFlash(ctx, flasherArgs...)
}

func (p *BuildFlasher) runFlash(ctx context.Context, args ...string) error {
	args = append([]string{"target", "flash", p.FlashManifest}, args...)

	path, err := exec.LookPath(p.FfxToolPath)
	if err != nil {
		return err
	}

	logger.Infof(ctx, "running: %s %q", path, args)
	cmd := exec.CommandContext(ctx, path, args...)
	if p.stdout != nil {
		cmd.Stdout = p.stdout
	} else {
		cmd.Stdout = os.Stdout
	}
	cmd.Stderr = os.Stderr
	cmdRet := cmd.Run()
	logger.Infof(ctx, "finished running %s %q: %q", path, args, cmdRet)
	return cmdRet
}
