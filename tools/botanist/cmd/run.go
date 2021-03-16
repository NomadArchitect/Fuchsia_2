// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bufio"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"io/ioutil"
	"net"
	"os"
	"path/filepath"
	"sync"
	"time"

	"go.fuchsia.dev/fuchsia/tools/bootserver"
	"go.fuchsia.dev/fuchsia/tools/botanist"
	"go.fuchsia.dev/fuchsia/tools/botanist/constants"
	"go.fuchsia.dev/fuchsia/tools/botanist/target"
	"go.fuchsia.dev/fuchsia/tools/lib/environment"
	"go.fuchsia.dev/fuchsia/tools/lib/flagmisc"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/lib/runner"
	"go.fuchsia.dev/fuchsia/tools/lib/syslog"
	"go.fuchsia.dev/fuchsia/tools/net/sshutil"
	"go.fuchsia.dev/fuchsia/tools/serial"

	"github.com/google/subcommands"
	"golang.org/x/sync/errgroup"
)

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
}

func (r *RunCommand) execute(ctx context.Context, args []string) error {
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()

	opts := target.Options{
		Netboot: r.netboot,
		SSHKey:  r.sshKey,
	}

	data, err := ioutil.ReadFile(r.configFile)
	if err != nil {
		return fmt.Errorf("%s: %v", constants.ReadConfigFileErrorMsg, err)
	}
	var objs []json.RawMessage
	if err := json.Unmarshal(data, &objs); err != nil {
		return fmt.Errorf("could not unmarshal config file as a JSON list: %v", err)
	}

	var targets []target.Target
	for _, obj := range objs {
		t, err := deriveTarget(ctx, obj, opts)
		if err != nil {
			return err
		}
		targets = append(targets, t)
	}
	if len(targets) == 0 {
		return fmt.Errorf("no targets found")
	}

	// This is the primary target that a command will be run against and that
	// logs will be streamed from.
	t0 := targets[0]

	// Modify the zirconArgs passed to the kernel on boot to enable serial on x64.
	// arm64 devices should already be enabling kernel.serial at compile time.
	// We need to pass this in to all devices (even those without a serial line)
	// to prevent race conditions that only occur when the option isn't present.
	// TODO (fxbug.dev/10480): Move this back to being invoked in the if clause.
	r.zirconArgs = append(r.zirconArgs, "kernel.serial=legacy")

	// Disable usb mass storage to determine if it affects NUC stability.
	// TODO(rudymathu): Remove this once stability is achieved.
	r.zirconArgs = append(r.zirconArgs, "driver.usb_mass_storage.disable")

	eg, ctx := errgroup.WithContext(ctx)
	socketPath := os.Getenv(constants.SerialSocketEnvKey)
	var conn net.Conn
	if socketPath != "" && r.serialLogFile != "" {
		// If a serial server was created earlier in the stack, use
		// the socket to copy to the serial log file.
		serialLog, err := os.Create(r.serialLogFile)
		if err != nil {
			return err
		}
		defer serialLog.Close()
		conn, err = net.Dial("unix", socketPath)
		if err != nil {
			return err
		}
		eg.Go(func() error {
			logger.Debugf(ctx, "starting serial collection")
			// Copy each line from the serial mux to the log file.
			b := bufio.NewReader(conn)
			for {
				line, err := b.ReadString('\n')
				if err != nil {
					if !serial.IsErrNetClosing(err) {
						return fmt.Errorf("%s: %w", constants.SerialReadErrorMsg, err)
					}
					return nil
				}
				if _, err := io.WriteString(serialLog, line); err != nil {
					return fmt.Errorf("failed to write line to serial log: %w", err)
				}
			}
		})
	} else if t0.Serial() != nil {
		// Otherwise, spin up a serial server now.
		defer t0.Serial().Close()

		sOpts := serial.ServerOptions{
			Logger: logger.LoggerFromContext(ctx),
		}
		if r.serialLogFile != "" {
			serialLog, err := os.Create(r.serialLogFile)
			if err != nil {
				return err
			}
			defer serialLog.Close()
			sOpts.AuxiliaryOutput = serialLog
		}

		s := serial.NewServer(t0.Serial(), sOpts)
		socketPath = createSocketPath()
		addr := &net.UnixAddr{Name: socketPath, Net: "unix"}
		l, err := net.ListenUnix("unix", addr)
		if err != nil {
			return err
		}
		eg.Go(func() error {
			if err := s.Run(ctx, l); err != nil && ctx.Err() == nil {
				return fmt.Errorf("serial server error: %w", err)
			}
			return nil
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
		if conn != nil {
			defer conn.Close()
		}

		if err := r.startTargets(ctx, targets, socketPath); err != nil {
			return fmt.Errorf("%s: %w", constants.FailedToStartTargetMsg, err)
		}
		defer func() {
			ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
			defer cancel()
			r.stopTargets(ctx, targets)
		}()
		return r.runAgainstTarget(ctx, t0, args, socketPath)
	})

	return eg.Wait()
}

func (r *RunCommand) startTargets(ctx context.Context, targets []target.Target, serialSocketPath string) error {
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

			return t.Start(ctx, imgs, r.zirconArgs, serialSocketPath)
		})
	}
	return eg.Wait()
}

func (r *RunCommand) stopTargets(ctx context.Context, targets []target.Target) {
	// Stop the targets in parallel.
	eg, ctx := errgroup.WithContext(ctx)
	for _, t := range targets {
		t := t
		eg.Go(func() error {
			return t.Stop(ctx)
		})
	}
	_ = eg.Wait()
}

func (r *RunCommand) runAgainstTarget(ctx context.Context, t target.Target, args []string, socketPath string) error {
	subprocessEnv := map[string]string{
		constants.NodenameEnvKey:     t.Nodename(),
		constants.SerialSocketEnvKey: socketPath,
	}

	// If |netboot| is true, then we assume that fuchsia is not provisioned
	// with a netstack; in this case, do not try to establish a connection.
	if !r.netboot {
		p, err := ioutil.ReadFile(t.SSHKey())
		if err != nil {
			return err
		}
		config, err := sshutil.DefaultSSHConfig(p)
		if err != nil {
			return err
		}

		sshAddr := net.TCPAddr{
			Port: sshutil.SSHPort,
		}
		if t, ok := t.(target.ConfiguredTarget); ok {
			if addr := t.Address(); len(addr) != 0 {
				sshAddr.IP = addr
				subprocessEnv[constants.DeviceAddrEnvKey] = addr.String()
			}
		}
		if sshAddr.IP == nil {
			ipv4Addr, ipv6Addr, err := func() (net.IP, net.IPAddr, error) {
				ctx, cancel := context.WithTimeout(ctx, 90*time.Second)
				defer cancel()
				return botanist.ResolveIP(ctx, t.Nodename())
			}()
			if err != nil {
				return fmt.Errorf("could not resolve IP address of %s: %w", t.Nodename(), err)
			}
			if ipv4Addr != nil {
				sshAddr.IP = ipv4Addr

				logger.Infof(ctx, "IPv4 address of %s found: %s", t.Nodename(), ipv4Addr)
				subprocessEnv[constants.IPv4AddrEnvKey] = ipv4Addr.String()
				if _, ok := subprocessEnv[constants.DeviceAddrEnvKey]; !ok {
					subprocessEnv[constants.DeviceAddrEnvKey] = ipv4Addr.String()
				}
			} else {
				logger.Warningf(ctx, "could not resolve IPv4 address of %s", t.Nodename())
			}
			if ipv6Addr.IP != nil {
				sshAddr.IP = ipv6Addr.IP
				sshAddr.Zone = ipv6Addr.Zone

				logger.Infof(ctx, "IPv6 address of %s found: %s", t.Nodename(), &ipv6Addr)
				subprocessEnv[constants.IPv6AddrEnvKey] = ipv6Addr.String()
				if _, ok := subprocessEnv[constants.DeviceAddrEnvKey]; !ok {
					subprocessEnv[constants.DeviceAddrEnvKey] = ipv6Addr.String()
				}
			} else {
				logger.Warningf(ctx, "could not resolve IPv6 address of %s", t.Nodename())
			}
		}

		if sshAddr.IP == nil {
			// Reachable when ResolveIP times out because no error is returned.
			// Invoke `threads` over serial if possible to dump process state to logs.
			// Invokes the command twice to identify hanging processes.
			if conn, err := net.Dial("unix", socketPath); err != nil {
				logger.Errorf(ctx, "failed to open serial socket %s to invoke threads: %s", socketPath, err)
			} else {
				for i := 0; i < 2; i++ {
					if _, err := io.WriteString(conn, fmt.Sprintf("\r\nthreads --all-processes\r\n")); err != nil {
						logger.Errorf(ctx, "failed to send threads over serial socket: %s", err)
					}
					time.Sleep(5 * time.Second)
				}
			}
			return fmt.Errorf("%s for %s", constants.FailedToResolveIPErrorMsg, t.Nodename())
		}

		client, err := sshutil.NewClient(ctx, &sshAddr, config, sshutil.DefaultConnectBackoff())
		if err != nil {
			return err
		}
		defer client.Close()

		if r.repoURL != "" {
			if err := botanist.AddPackageRepository(ctx, client, r.repoURL, r.blobURL); err != nil {
				return fmt.Errorf("%s: %w", constants.PackageRepoSetupErrorMsg, err)
			}
		}

		if r.syslogFile != "" {
			stopStreaming, err := r.startSyslogStream(ctx, client)
			if err != nil {
				return err
			}
			// Stop streaming syslogs after we've finished running the command.
			defer stopStreaming()
		}

		subprocessEnv[constants.SSHKeyEnvKey] = t.SSHKey()
	}

	// Run the provided command against t0, adding |subprocessEnv| into
	// its environment.
	environ := os.Environ()
	for k, v := range subprocessEnv {
		environ = append(environ, fmt.Sprintf("%s=%s", k, v))
	}
	runner := runner.SubprocessRunner{
		Env: environ,
	}

	ctx, cancel := context.WithTimeout(ctx, r.timeout)
	defer cancel()

	if err := runner.Run(ctx, args, os.Stdout, os.Stderr); err != nil {
		return fmt.Errorf("command %s with timeout %s failed: %w", args, r.timeout, err)
	}
	return nil
}

// startSyslogStream uses the SSH client to start streaming syslogs from the
// fuchsia target to a file, in a background goroutine. It returns a function
// that cancels the streaming, which should be deferred by the caller.
func (r *RunCommand) startSyslogStream(ctx context.Context, client *sshutil.Client) (stopStreaming func(), err error) {
	syslogger := syslog.NewSyslogger(client)

	f, err := os.Create(r.syslogFile)
	if err != nil {
		return nil, err
	}

	ctx, cancel := context.WithCancel(ctx)

	var wg sync.WaitGroup
	wg.Add(1)
	go func() {
		defer f.Close()
		defer wg.Done()
		syslogger.Stream(ctx, f)
	}()

	// The caller should call this function when they want to stop streaming syslogs.
	return func() {
		// Signal syslogger.Stream to stop and wait for it to finish before
		// return. This makes sure syslogger.Stream finish necessary clean-up
		// (e.g. closing any open SSH sessions) before SSH client is closed.
		cancel()
		wg.Wait()
	}, nil
}

func (r *RunCommand) Execute(ctx context.Context, f *flag.FlagSet, _ ...interface{}) subcommands.ExitStatus {
	args := f.Args()
	if len(args) == 0 {
		return subcommands.ExitUsageError
	}

	cleanUp, err := environment.Ensure()
	if err != nil {
		logger.Errorf(ctx, "failed to setup environment: %v", err)
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

func createSocketPath() string {
	// We randomly construct a socket path that is highly improbable to collide with anything.
	randBytes := make([]byte, 16)
	rand.Read(randBytes)
	return filepath.Join(os.TempDir(), "serial"+hex.EncodeToString(randBytes)+".sock")
}

func deriveTarget(ctx context.Context, obj []byte, opts target.Options) (target.Target, error) {
	type typed struct {
		Type string `json:"type"`
	}
	var x typed

	if err := json.Unmarshal(obj, &x); err != nil {
		return nil, fmt.Errorf("object in list has no \"type\" field: %v", err)
	}
	switch x.Type {
	case "aemu":
		var cfg target.QEMUConfig
		if err := json.Unmarshal(obj, &cfg); err != nil {
			return nil, fmt.Errorf("invalid QEMU config found: %v", err)
		}
		return target.NewAEMUTarget(cfg, opts)
	case "qemu":
		var cfg target.QEMUConfig
		if err := json.Unmarshal(obj, &cfg); err != nil {
			return nil, fmt.Errorf("invalid QEMU config found: %v", err)
		}
		return target.NewQEMUTarget(cfg, opts)
	case "device":
		var cfg target.DeviceConfig
		if err := json.Unmarshal(obj, &cfg); err != nil {
			return nil, fmt.Errorf("invalid device config found: %v", err)
		}
		t, err := target.NewDeviceTarget(ctx, cfg, opts)
		return t, err
	case "gce":
		var cfg target.GCEConfig
		if err := json.Unmarshal(obj, &cfg); err != nil {
			return nil, fmt.Errorf("invalid GCE config found: %v", err)
		}
		return target.NewGCETarget(ctx, cfg, opts)
	default:
		return nil, fmt.Errorf("unknown type found: %q", x.Type)
	}
}
