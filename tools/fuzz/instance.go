// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fuzz

import (
	"fmt"
	"io"
	"time"

	"github.com/golang/glog"
)

// An Instance is a specific combination of build, connector, and launcher,
// representable by a handle.  Most methods of this interface map directly to
// the ClusterFuchsia API.
type Instance interface {
	Start() error
	Stop() error
	ListFuzzers() []string
	Get(fuzzerName, targetSrc, hostDst string) error
	Put(fuzzerName, hostSrc, targetDst string) error
	RunFuzzer(out io.Writer, name, hostArtifactDir string, args ...string) error
	Handle() (Handle, error)
	Close()
}

// BaseInstance groups the core subobjects, to which most work will be delegated
type BaseInstance struct {
	Build     Build
	Connector Connector
	Launcher  Launcher

	// While we rewrite the handle data each time Handle() is called, we need to keep a
	// reference around to avoid recreating new Handles each time
	handle Handle
}

// NewInstance creates a fresh instance
func NewInstance() (Instance, error) {
	build, err := NewBuild()
	if err != nil {
		return nil, fmt.Errorf("Error configuring build: %s", err)
	}

	if err := build.Prepare(); err != nil {
		return nil, fmt.Errorf("Error preparing build: %s", err)
	}

	// TODO(fxbug.dev/47479): should the user be able to choose connector/launcher types?
	launcher := NewQemuLauncher(build)

	// Note: We can't get a Connector until the Launcher has started
	return &BaseInstance{build, nil, launcher, ""}, nil
}

func loadInstanceFromHandle(handle Handle) (Instance, error) {
	// TODO(fxbug.dev/47320): Store build info in the handle too
	build, err := NewBuild()
	if err != nil {
		return nil, fmt.Errorf("Error configuring build: %s", err)
	}

	connector, err := loadConnectorFromHandle(handle)
	if err != nil {
		return nil, err
	}

	launcher, err := loadLauncherFromHandle(build, handle)
	if err != nil {
		return nil, err
	}

	return &BaseInstance{build, connector, launcher, handle}, nil
}

// Close releases the Instance, but doesn't Stop it
func (i *BaseInstance) Close() {
	i.Connector.Close()
}

// Start boots up the instance and waits for connectivity to be established
//
// If Start succeeds, it is up to the caller to clean up by calling Stop later.
// However, if it fails, any resources will have been automatically released.
func (i *BaseInstance) Start() error {
	conn, err := i.Launcher.Start()
	if err != nil {
		return err
	}
	i.Connector = conn

	glog.Infof("Waiting for connectivity...")

	sleep := 3 * time.Second
	success := false
	for j := 0; j < 3; j++ {
		cmd := conn.Command("echo", "hello")
		cmd.SetTimeout(5 * time.Second)
		if err := cmd.Start(); err != nil {
			i.Launcher.Kill()
			return err
		}

		if err := cmd.Wait(); err != nil {
			// If you forgot to --with-base the devtools:
			// error: 2 (/boot/bin/sh: 1: Cannot create child process: -1 (ZX_ERR_INTERNAL):
			// failed to resolve fuchsia-pkg://fuchsia.com/ls#bin/ls
			glog.Warningf("Got error during attempt %d: %s", j, err)
			glog.Warningf("Retrying in %s...", sleep)
			time.Sleep(sleep)
		} else {
			glog.Info("Instance is now online.")
			success = true

			if sc, ok := conn.(*SSHConnector); ok {
				glog.Infof("Access via: ssh -i'%s' %s:%d", sc.Key, sc.Host, sc.Port)
			}
			break
		}
	}
	if !success {
		i.Launcher.Kill()
		return fmt.Errorf("error establishing connectivity to instance")
	}

	return nil
}

// RunFuzzer runs the named fuzzer on the Instance. If `hostArtifactDir` is
// specified and the run generated any output artifacts, they will be copied to
// that directory. `args` is an optional list of arguments in the form
// `-key=value` that will be passed to libFuzzer.
func (i *BaseInstance) RunFuzzer(out io.Writer, name, hostArtifactDir string, args ...string) error {
	fuzzer, err := i.Build.Fuzzer(name)
	if err != nil {
		return err
	}

	fuzzer.Parse(args)
	artifacts, err := fuzzer.Run(i.Connector, out, hostArtifactDir)
	if err != nil {
		return err
	}

	if hostArtifactDir != "" {
		for _, art := range artifacts {
			if err := i.Get(name, art, hostArtifactDir); err != nil {
				return err
			}
		}
	}

	return nil
}

// Get copies files from a fuzzer namespace on the Instance to the host
func (i *BaseInstance) Get(fuzzerName, targetSrc, hostDst string) error {
	fuzzer, err := i.Build.Fuzzer(fuzzerName)
	if err != nil {
		return err
	}
	return i.Connector.Get(fuzzer.AbsPath(targetSrc), hostDst)
}

// Put copies files from the host to a fuzzer namespace on the Instance
func (i *BaseInstance) Put(fuzzerName, hostSrc, targetDst string) error {
	fuzzer, err := i.Build.Fuzzer(fuzzerName)
	if err != nil {
		return err
	}
	return i.Connector.Put(hostSrc, fuzzer.AbsPath(targetDst))
}

// Stop shuts down the Instance
func (i *BaseInstance) Stop() error {
	if err := i.Launcher.Kill(); err != nil {
		return err
	}

	if i.handle != "" {
		i.handle.Release()
	}

	return nil
}

// Handle returns a Handle representing the Instance
func (i *BaseInstance) Handle() (Handle, error) {
	data := HandleData{i.Connector, i.Launcher}

	if i.handle == "" {
		// Create a new handle from scratch
		handle, err := NewHandleWithData(data)

		if err != nil {
			return "", fmt.Errorf("error constructing instance handle: %s", err)
		}

		i.handle = handle
		return handle, nil
	}

	// Update an existing handle, but don't release it even if we fail
	if err := i.handle.SetData(data); err != nil {
		return "", fmt.Errorf("error updating instance handle: %s", err)
	}

	return i.handle, nil
}

// ListFuzzers lists fuzzers available on the Instance
func (i *BaseInstance) ListFuzzers() []string {
	return i.Build.ListFuzzers()
}
