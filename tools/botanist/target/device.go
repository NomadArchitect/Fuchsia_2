// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package target

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"io/ioutil"
	"net"
	"os"
	"os/exec"
	"sync/atomic"

	"go.fuchsia.dev/fuchsia/tools/bootserver"
	"go.fuchsia.dev/fuchsia/tools/lib/iomisc"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/lib/serial"
	"go.fuchsia.dev/fuchsia/tools/net/netboot"
	"go.fuchsia.dev/fuchsia/tools/net/netutil"
	"go.fuchsia.dev/fuchsia/tools/net/tftp"

	"golang.org/x/crypto/ssh"
)

const (
	// Command to dump the zircon debug log over serial.
	dlogCmd = "\ndlog\n"

	// String to look for in serial log that indicates system booted. From
	// https://cs.opensource.google/fuchsia/fuchsia/+/main:zircon/kernel/top/main.cc;l=116;drc=6a0fd696cde68b7c65033da57ab911ee5db75064
	bootedLogSignature = "welcome to Zircon"
)

// DeviceConfig contains the static properties of a target device.
type DeviceConfig struct {
	// FastbootSernum is the fastboot serial number of the device.
	FastbootSernum string `json:"fastboot_sernum"`

	// Network is the network properties of the target.
	Network NetworkProperties `json:"network"`

	// SSHKeys are the default system keys to be used with the device.
	SSHKeys []string `json:"keys,omitempty"`

	// Serial is the path to the device file for serial i/o.
	Serial string `json:"serial,omitempty"`

	// SerialMux is the path to the device's serial multiplexer.
	SerialMux string `json:"serial_mux,omitempty"`
}

// NetworkProperties are the static network properties of a target.
type NetworkProperties struct {
	// Nodename is the hostname of the device that we want to boot on.
	Nodename string `json:"nodename"`

	// IPv4Addr is the IPv4 address, if statically given. If not provided, it may be
	// resolved via the netstack's MDNS server.
	IPv4Addr string `json:"ipv4"`
}

// LoadDeviceConfigs unmarshalls a slice of device configs from a given file.
func LoadDeviceConfigs(path string) ([]DeviceConfig, error) {
	data, err := ioutil.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read device properties file %q", path)
	}

	var configs []DeviceConfig
	if err := json.Unmarshal(data, &configs); err != nil {
		return nil, fmt.Errorf("failed to unmarshal configs: %w", err)
	}
	return configs, nil
}

var (
	_ Target           = (*DeviceTarget)(nil)
	_ ConfiguredTarget = (*DeviceTarget)(nil)
)

// DeviceTarget represents a target device.
type DeviceTarget struct {
	config   DeviceConfig
	opts     Options
	signers  []ssh.Signer
	serial   io.ReadWriteCloser
	tftp     tftp.Client
	stopping uint32
}

// NewDeviceTarget returns a new device target with a given configuration.
func NewDeviceTarget(ctx context.Context, config DeviceConfig, opts Options) (*DeviceTarget, error) {
	// If an SSH key is specified in the options, prepend it the configs list so that it
	// corresponds to the authorized key that would be paved.
	if opts.SSHKey != "" {
		config.SSHKeys = append([]string{opts.SSHKey}, config.SSHKeys...)
	}
	signers, err := parseOutSigners(config.SSHKeys)
	if err != nil {
		return nil, fmt.Errorf("could not parse out signers from private keys: %w", err)
	}
	var s io.ReadWriteCloser
	if config.SerialMux != "" {
		s, err = serial.NewSocket(ctx, config.SerialMux)
		if err != nil {
			return nil, fmt.Errorf("unable to open: %s: %w", config.SerialMux, err)
		}
		// Dump the existing serial debug log buffer.
		if _, err := io.WriteString(s, dlogCmd); err != nil {
			return nil, fmt.Errorf("failed to tail serial logs: %w", err)
		}
	} else if config.Serial != "" {
		s, err = serial.Open(config.Serial)
		if err != nil {
			return nil, fmt.Errorf("unable to open %s: %w", config.Serial, err)
		}
		// Dump the existing serial debug log buffer.
		if _, err := io.WriteString(s, dlogCmd); err != nil {
			return nil, fmt.Errorf("failed to tail serial logs: %w", err)
		}
	}
	return &DeviceTarget{
		config:  config,
		opts:    opts,
		signers: signers,
		serial:  s,
	}, nil
}

// Tftp returns a tftp client interface for the device.
func (t *DeviceTarget) Tftp() tftp.Client {
	return t.tftp
}

// Nodename returns the name of the node.
func (t *DeviceTarget) Nodename() string {
	return t.config.Network.Nodename
}

// Serial returns the serial device associated with the target for serial i/o.
func (t *DeviceTarget) Serial() io.ReadWriteCloser {
	return t.serial
}

// Address implements ConfiguredTarget.
func (t *DeviceTarget) Address() net.IP {
	return net.ParseIP(t.config.Network.IPv4Addr)
}

// SSHKey returns the private SSH key path associated with the authorized key to be paved.
func (t *DeviceTarget) SSHKey() string {
	return t.config.SSHKeys[0]
}

// Start starts the device target.
func (t *DeviceTarget) Start(ctx context.Context, images []bootserver.Image, args []string, serialSocketPath string) error {
	// Initialize the tftp client if:
	// 1. It is currently uninitialized.
	// 2. The device cannot be accessed via fastboot.
	if t.config.FastbootSernum == "" && t.tftp == nil {
		// Discover the node on the network and initialize a tftp client to
		// talk to it.
		addr, err := netutil.GetNodeAddress(ctx, t.Nodename())
		if err != nil {
			return err
		}
		tftpClient, err := tftp.NewClient(&net.UDPAddr{
			IP:   addr.IP,
			Port: tftp.ClientPort,
			Zone: addr.Zone,
		}, 0, 0)
		if err != nil {
			return err
		}
		t.tftp = tftpClient
	}

	// Set up log listener and dump kernel output to stdout.
	l, err := netboot.NewLogListener(t.Nodename())
	if err != nil {
		return fmt.Errorf("cannot listen: %w", err)
	}
	go func() {
		defer l.Close()
		for atomic.LoadUint32(&t.stopping) == 0 {
			data, err := l.Listen()
			if err != nil {
				continue
			}
			fmt.Print(data)
		}
	}()

	// Get authorized keys from the ssh signers.
	// We cannot have signers in netboot because there is no notion
	// of a hardware backed key when you are not booting from disk
	var authorizedKeys []byte
	if !t.opts.Netboot {
		if len(t.signers) > 0 {
			for _, s := range t.signers {
				authorizedKey := ssh.MarshalAuthorizedKey(s.PublicKey())
				authorizedKeys = append(authorizedKeys, authorizedKey...)
			}
		}
	}

	bootedLogChan := make(chan error)
	if serialSocketPath != "" {
		// Start searching for the string before we reboot, otherwise we can miss it.
		go func() {
			logger.Debugf(ctx, "watching serial for string that indicates device has booted: %q", bootedLogSignature)
			socket, err := net.Dial("unix", serialSocketPath)
			if err != nil {
				bootedLogChan <- fmt.Errorf("failed to open serial socket connection: %w", err)
				return
			}
			defer socket.Close()
			_, err = iomisc.ReadUntilMatchString(ctx, socket, bootedLogSignature)
			bootedLogChan <- err
		}()
	}

	// Boot Fuchsia.
	if t.config.FastbootSernum != "" {
		if err := t.flash(ctx, images); err != nil {
			return err
		}
	} else {
		if err := bootserver.Boot(ctx, t.Tftp(), images, args, authorizedKeys); err != nil {
			return err
		}
	}

	if serialSocketPath != "" {
		return <-bootedLogChan
	}

	return nil
}

func (t *DeviceTarget) flash(ctx context.Context, images []bootserver.Image) error {
	// Download the images to disk.
	// TODO(rudymathu): Transport these images via isolate for improved caching performance.
	wd, err := os.Getwd()
	if err != nil {
		return err
	}
	var imgs []*bootserver.Image
	for _, img := range images {
		img := img
		imgs = append(imgs, &img)
	}
	if err := copyImagesToDir(ctx, wd, true, imgs...); err != nil {
		return err
	}

	flashScript := ""
	for _, img := range imgs {
		if img.Name == "script_flash-script" {
			flashScript = img.Path
			break
		}
	}
	if flashScript == "" {
		return errors.New("flash script not found")
	}
	// Write the public SSH key to disk if one is needed.
	flashArgs := []string{"-s", t.config.FastbootSernum}
	if !t.opts.Netboot && len(t.signers) > 0 {
		pubkey, err := ioutil.TempFile("", "pubkey*")
		if err != nil {
			return err
		}
		defer os.Remove(pubkey.Name())
		defer pubkey.Close()

		if _, err := pubkey.Write(ssh.MarshalAuthorizedKey(t.signers[0].PublicKey())); err != nil {
			return err
		}
		flashArgs = append([]string{fmt.Sprintf("--ssh-key=%s", pubkey.Name())}, flashArgs...)
	}

	cmd := exec.CommandContext(ctx, flashScript, flashArgs...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	return cmd.Run()
}

// Stop stops the device.
func (t *DeviceTarget) Stop(context.Context) error {
	atomic.StoreUint32(&t.stopping, 1)
	return nil
}

// Wait waits for the device target to stop.
func (t *DeviceTarget) Wait(context.Context) error {
	return ErrUnimplemented
}

func parseOutSigners(keyPaths []string) ([]ssh.Signer, error) {
	if len(keyPaths) == 0 {
		return nil, errors.New("must supply SSH keys in the config")
	}
	var keys [][]byte
	for _, keyPath := range keyPaths {
		p, err := ioutil.ReadFile(keyPath)
		if err != nil {
			return nil, fmt.Errorf("could not read SSH key file %q: %w", keyPath, err)
		}
		keys = append(keys, p)
	}

	var signers []ssh.Signer
	for _, p := range keys {
		signer, err := ssh.ParsePrivateKey(p)
		if err != nil {
			return nil, err
		}
		signers = append(signers, signer)
	}
	return signers, nil
}
