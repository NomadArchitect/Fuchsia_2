// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"io/ioutil"
	"net"
	"os"
	"strconv"
	"time"

	"go.fuchsia.dev/fuchsia/tools/bootserver"
	"go.fuchsia.dev/fuchsia/tools/botanist"
	"go.fuchsia.dev/fuchsia/tools/botanist/constants"
	"go.fuchsia.dev/fuchsia/tools/botanist/target"
	"go.fuchsia.dev/fuchsia/tools/lib/environment"
	"go.fuchsia.dev/fuchsia/tools/lib/flagmisc"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/lib/serial"
	"go.fuchsia.dev/fuchsia/tools/lib/subprocess"
	"go.fuchsia.dev/fuchsia/tools/lib/syslog"
	"go.fuchsia.dev/fuchsia/tools/net/sshutil"

	"github.com/google/subcommands"
	"golang.org/x/sync/errgroup"
)

// Target represents a generic fuchsia instance.
type Target interface {
	// AddPackageRepository adds a given package repository to the target.
	AddPackageRepository(client *sshutil.Client, repoURL, blobURL string) error

	// CaptureSerialLog starts capturing serial logs to the given file.
	// This is only valid once the target has a serial multiplexer running.
	CaptureSerialLog(filename string) error

	// CaptureSyslog starts capturing the syslog to the given file.
	// This is only valid when the target has SSH running.
	CaptureSyslog(client *sshutil.Client, filename, repoURL, blobURL string) error

	// IPv4 returns the IPv4 of the target; this is nil unless explicitly
	// configured.
	IPv4() (net.IP, error)

	// IPv6 returns the IPv6 of the target.
	IPv6() (*net.IPAddr, error)

	// Nodename returns the name of the target node.
	Nodename() string

	// Serial returns the serial device associated with the target for serial i/o.
	Serial() io.ReadWriteCloser

	// SerialSocketPath returns the path to the target's serial socket.
	SerialSocketPath() string

	// SSHClient returns an SSH client to the device (if the device has SSH running).
	SSHClient() (*sshutil.Client, error)

	// SSHKey returns the private key corresponding an authorized SSH key of the target.
	SSHKey() string

	// Start starts the target.
	Start(ctx context.Context, images []bootserver.Image, args []string) error

	// StartSerialServer starts the serial server for the target iff one
	// does not exist.
	StartSerialServer() error

	// Stop stops the target.
	Stop() error

	// Wait waits for the target to finish running.
	Wait(context.Context) error
}

// RunCommand is a Command implementation for booting a device and running a
// given command locally.
type RunCommand struct {
	// ConfigFile is the path to the target configurations.
	configFile string

	// ImageManifest is a path to an image manifest.
	imageManifest string

	// Netboot tells botanist to netboot (and not to pave).
	netboot bool

	// ZirconArgs are kernel command-line arguments to pass on boot.
	zirconArgs flagmisc.StringsValue

	// Timeout is the duration allowed for the command to finish execution.
	timeout time.Duration

	// SysloggerFile, if nonempty, is the file to where the system's logs will be written.
	syslogFile string

	// SshKey is the path to a private SSH user key.
	sshKey string

	// SerialLogFile, if nonempty, is the file where the system's serial logs will be written.
	serialLogFile string

	// RepoURL specifies the URL of a package repository.
	repoURL string

	// BlobURL optionally specifies the URL of where a package repository's blobs may be served from.
	// Defaults to $repoURL/blobs.
	blobURL string

	// localRepo specifies the path to a local package repository. If set,
	// botanist will spin up a package server to serve packages from this
	// repository.
	localRepo string
}

func (*RunCommand) Name() string {
	return "run"
}

func (*RunCommand) Usage() string {
	return `
botanist run [flags...] [command...]

flags:
`
}

func (*RunCommand) Synopsis() string {
	return "boots a device and runs a local command"
}

func (r *RunCommand) SetFlags(f *flag.FlagSet) {
	f.StringVar(&r.configFile, "config", "", "path to file of device config")
	f.StringVar(&r.imageManifest, "images", "", "path to an image manifest")
	f.BoolVar(&r.netboot, "netboot", false, "if set, botanist will not pave; but will netboot instead")
	f.Var(&r.zirconArgs, "zircon-args", "kernel command-line arguments")
	f.DurationVar(&r.timeout, "timeout", 10*time.Minute, "duration allowed for the command to finish execution.")
	f.StringVar(&r.syslogFile, "syslog", "", "file to write the systems logs to")
	f.StringVar(&r.sshKey, "ssh", "", "file containing a private SSH user key; if not provided, a private key will be generated.")
	f.StringVar(&r.serialLogFile, "serial-log", "", "file to write the serial logs to.")
	f.StringVar(&r.repoURL, "repo", "", "URL at which to configure a package repository; if the placeholder of \"localhost\" will be resolved and scoped as appropriate")
	f.StringVar(&r.blobURL, "blobs", "", "URL at which to serve a package repository's blobs; if the placeholder of \"localhost\" will be resolved and scoped as appropriate")
	f.StringVar(&r.localRepo, "local-repo", "", "path to a local package repository; the repo and blobs flags are ignored when this is set")
}

func (r *RunCommand) execute(ctx context.Context, args []string) error {
	ctx, cancel := context.WithTimeout(ctx, r.timeout)
	defer cancel()

	// Start up a local package server if one was requested.
	if r.localRepo != "" {
		var port int
		pkgSrvPort := os.Getenv(constants.PkgSrvPortKey)
		if pkgSrvPort == "" {
			logger.Warningf(ctx, "%s is empty, using default port %d", constants.PkgSrvPortKey, botanist.DefaultPkgSrvPort)
			port = botanist.DefaultPkgSrvPort
		} else {
			var err error
			port, err = strconv.Atoi(pkgSrvPort)
			if err != nil {
				return err
			}
		}
		repoURL, blobURL, err := botanist.NewPackageServer(ctx, r.localRepo, port)
		if err != nil {
			return err
		}
		// TODO(rudymathu): Once gcsproxy and remote package serving are deprecated, remove
		// the repoURL and blobURL from the command line flags.
		r.repoURL = repoURL
		r.blobURL = blobURL
	}
	// Disable usb mass storage to determine if it affects NUC stability.
	// TODO(rudymathu): Remove this once stability is achieved.
	r.zirconArgs = append(r.zirconArgs, "driver.usb_mass_storage.disable")

	// Parse targets out from the target configuration file.
	targets, err := r.deriveTargetsFromFile(ctx)
	if err != nil {
		return err
	}
	// This is the primary target that a command will be run against and that
	// logs will be streamed from.
	t0 := targets[0]

	// Start serial servers for all targets. Will no-op for targets that
	// already have serial servers.
	for _, t := range targets {
		if err := t.StartSerialServer(); err != nil {
			return err
		}
	}

	eg, ctx := errgroup.WithContext(ctx)
	if r.serialLogFile != "" {
		eg.Go(func() error {
			logger.Debugf(ctx, "starting serial collection")
			return t0.CaptureSerialLog(r.serialLogFile)
		})
	}

	for _, t := range targets {
		t := t
		eg.Go(func() error {
			if err := t.Wait(ctx); err != nil && err != target.ErrUnimplemented && ctx.Err() == nil {
				return fmt.Errorf("target %s failed: %w", t.Nodename(), err)
			}
			return nil
		})
	}

	eg.Go(func() error {
		// Signal other goroutines to exit.
		defer cancel()
		if err := r.startTargets(ctx, targets); err != nil {
			return fmt.Errorf("%s: %w", constants.FailedToStartTargetMsg, err)
		}
		logger.Debugf(ctx, "successfully started all targets")
		if !r.netboot {
			for i, t := range targets {
				client, err := t.SSHClient()
				if err != nil {
					if err := r.dumpSyslogOverSerial(ctx, t.SerialSocketPath()); err != nil {
						logger.Errorf(ctx, err.Error())
					}
					return err
				}
				if r.repoURL != "" {
					if err := t.AddPackageRepository(client, r.repoURL, r.blobURL); err != nil {
						return err
					}
					logger.Debugf(ctx, "added package repo to target %s", t.Nodename())
				}
				if i == 0 && r.syslogFile != "" {
					go func() {
						t0.CaptureSyslog(client, r.syslogFile, r.repoURL, r.blobURL)
					}()
				}
			}
		}
		defer func() {
			ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
			defer cancel()
			r.stopTargets(ctx, targets)
		}()
		return r.runAgainstTarget(ctx, t0, args)
	})

	return eg.Wait()
}

func (r *RunCommand) startTargets(ctx context.Context, targets []Target) error {
	bootMode := bootserver.ModePave
	if r.netboot {
		bootMode = bootserver.ModeNetboot
	}

	// We wait until targets have started before running the subcommand against the zeroth one.
	eg, ctx := errgroup.WithContext(ctx)
	for _, t := range targets {
		t := t
		eg.Go(func() error {
			// TODO(fxbug.dev/47910): Move outside gofunc once we get rid of downloading or ensure that it only happens once.
			imgs, closeFunc, err := bootserver.GetImages(ctx, r.imageManifest, bootMode)
			if err != nil {
				return err
			}
			defer closeFunc()

			return t.Start(ctx, imgs, r.zirconArgs)
		})
	}
	return eg.Wait()
}

func (r *RunCommand) stopTargets(ctx context.Context, targets []Target) {
	// Stop the targets in parallel.
	var eg errgroup.Group
	for _, t := range targets {
		t := t
		eg.Go(func() error {
			return t.Stop()
		})
	}
	_ = eg.Wait()
}

// dumpSyslogOverSerial runs log_listener over serial to collect logs that may
// help with debugging. This is intended to be used when SSH connection fails to
// get some information about the failure mode prior to exiting.
func (r *RunCommand) dumpSyslogOverSerial(ctx context.Context, socketPath string) error {
	socket, err := serial.NewSocket(ctx, socketPath)
	if err != nil {
		return fmt.Errorf("newSerialSocket failed: %w", err)
	}
	defer socket.Close()
	if err := serial.RunDiagnostics(ctx, socket); err != nil {
		return fmt.Errorf("failed to run serial diagnostics: %w", err)
	}
	// Dump the existing syslog buffer. This may not work if pkg-resolver is not
	// up yet, in which case it will just print nothing.
	cmds := []serial.Command{
		{Cmd: []string{syslog.LogListener, "--dump_logs", "yes"}, SleepDuration: 5 * time.Second},
	}
	if err := serial.RunCommands(ctx, socket, cmds); err != nil {
		return fmt.Errorf("failed to dump syslog over serial: %w", err)
	}
	return nil
}

func (r *RunCommand) runAgainstTarget(ctx context.Context, t Target, args []string) error {
	subprocessEnv := map[string]string{
		constants.NodenameEnvKey:     t.Nodename(),
		constants.SerialSocketEnvKey: t.SerialSocketPath(),
	}

	// If |netboot| is true, then we assume that fuchsia is not provisioned
	// with a netstack; in this case, do not try to establish a connection.
	if !r.netboot {
		var addr net.IPAddr
		ipv6, err := t.IPv6()
		if err != nil {
			return err
		}
		if ipv6 != nil {
			addr = *ipv6
		}
		ipv4, err := t.IPv4()
		if err != nil {
			return err
		}
		if ipv4 != nil {
			addr.IP = ipv4
			addr.Zone = ""
		}
		env := map[string]string{
			constants.DeviceAddrEnvKey: addr.String(),
			constants.IPv4AddrEnvKey:   ipv4.String(),
			constants.IPv6AddrEnvKey:   ipv6.String(),
			constants.SSHKeyEnvKey:     t.SSHKey(),
		}
		for k, v := range env {
			subprocessEnv[k] = v
		}
	}

	// Run the provided command against t0, adding |subprocessEnv| into
	// its environment.
	environ := os.Environ()
	for k, v := range subprocessEnv {
		environ = append(environ, fmt.Sprintf("%s=%s", k, v))
	}
	runner := subprocess.Runner{
		Env: environ,
	}

	if err := runner.RunWithStdin(ctx, args, os.Stdout, os.Stderr, nil); err != nil {
		return fmt.Errorf("command %s with timeout %s failed: %w", args, r.timeout, err)
	}
	return nil
}

func (r *RunCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	args := f.Args()
	if len(args) == 0 {
		return subcommands.ExitUsageError
	}

	cleanUp, err := environment.Ensure()
	if err != nil {
		logger.Errorf(ctx, "failed to setup environment: %s", err)
		return subcommands.ExitFailure
	}
	defer cleanUp()

	var expandedArgs []string
	for _, arg := range args {
		expandedArgs = append(expandedArgs, os.ExpandEnv(arg))
	}
	r.blobURL = os.ExpandEnv(r.blobURL)
	r.repoURL = os.ExpandEnv(r.repoURL)
	if err := r.execute(ctx, expandedArgs); err != nil {
		logger.Errorf(ctx, "%s", err)
		return subcommands.ExitFailure
	}
	return subcommands.ExitSuccess
}

func deriveTarget(ctx context.Context, obj []byte, opts target.Options) (Target, error) {
	type typed struct {
		Type string `json:"type"`
	}
	var x typed

	if err := json.Unmarshal(obj, &x); err != nil {
		return nil, fmt.Errorf("object in list has no \"type\" field: %w", err)
	}
	switch x.Type {
	case "aemu":
		var cfg target.QEMUConfig
		if err := json.Unmarshal(obj, &cfg); err != nil {
			return nil, fmt.Errorf("invalid QEMU config found: %w", err)
		}
		return target.NewAEMUTarget(ctx, cfg, opts)
	case "qemu":
		var cfg target.QEMUConfig
		if err := json.Unmarshal(obj, &cfg); err != nil {
			return nil, fmt.Errorf("invalid QEMU config found: %w", err)
		}
		return target.NewQEMUTarget(ctx, cfg, opts)
	case "device":
		var cfg target.DeviceConfig
		if err := json.Unmarshal(obj, &cfg); err != nil {
			return nil, fmt.Errorf("invalid device config found: %w", err)
		}
		t, err := target.NewDeviceTarget(ctx, cfg, opts)
		return t, err
	case "gce":
		var cfg target.GCEConfig
		if err := json.Unmarshal(obj, &cfg); err != nil {
			return nil, fmt.Errorf("invalid GCE config found: %w", err)
		}
		return target.NewGCETarget(ctx, cfg, opts)
	default:
		return nil, fmt.Errorf("unknown type found: %q", x.Type)
	}
}

func (r *RunCommand) deriveTargetsFromFile(ctx context.Context) ([]Target, error) {
	opts := target.Options{
		Netboot: r.netboot,
		SSHKey:  r.sshKey,
	}

	data, err := ioutil.ReadFile(r.configFile)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", constants.ReadConfigFileErrorMsg, err)
	}
	var objs []json.RawMessage
	if err := json.Unmarshal(data, &objs); err != nil {
		return nil, fmt.Errorf("could not unmarshal config file as a JSON list: %w", err)
	}

	var targets []Target
	for _, obj := range objs {
		t, err := deriveTarget(ctx, obj, opts)
		if err != nil {
			return nil, err
		}
		targets = append(targets, t)
	}
	if len(targets) == 0 {
		return nil, fmt.Errorf("no targets found")
	}
	return targets, nil
}
