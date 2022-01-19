// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fuzz

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"fmt"
	"io"
	"io/ioutil"
	"net"
	"os"
	"os/exec"
	"path"
	"path/filepath"
	"strconv"
	"syscall"
	"time"

	"github.com/golang/glog"
	"go.fuchsia.dev/fuchsia/tools/qemu"
)

// A Launcher manages the lifecycle of an instance; e.g. starting and stopping
type Launcher interface {
	// Do any preparation necessary before starting. This is idempotent, and
	// does not need to be explicitly called by the client, as it will be
	// automatically called by Start as necessary.
	Prepare() error

	// Starts the instance, returning a Connector that can be used to communicate with it
	Start() (Connector, error)

	// Returns true iff the instance is running.
	IsRunning() (bool, error)

	// Stops the instance. This is allowed to take up to 3 seconds to return.
	Kill() error

	// Dump any available system or debug logs to `out`
	GetLogs(out io.Writer) error
}

const successfulBootMarker = "{{{reset}}}"
const qemuBootTimeout = 10 * time.Second

// A QemuLauncher starts Fuchsia on QEMU
type QemuLauncher struct {
	Pid    int
	TmpDir string

	build Build

	// Paths to files that are created by Prepare():
	initrd       string
	extendedBlk  string
	sshKey       string
	sshPublicKey string

	// Overridable for testing
	timeout time.Duration
}

// qemuConfig contains all the configuration parameters that need to be passed
// from the Launcher to the qemu invocation
type qemuConfig struct {
	binary  string
	kernel  string
	initrd  string
	blk     string
	logFile string
	port    int
}

// NewQemuLauncher constructs a new QemuLauncher
func NewQemuLauncher(build Build) *QemuLauncher {
	return &QemuLauncher{build: build, timeout: qemuBootTimeout}
}

// Find an unused TCP port on the host
func getFreePort() (int, error) {
	listener, err := net.Listen("tcp", "localhost:0")
	if err != nil {
		return 0, err
	}

	port := listener.Addr().(*net.TCPAddr).Port

	// Technically this might get used before QEMU starts...
	listener.Close()
	return port, nil
}

// Configure QEMU appropriately
func getQemuInvocation(config qemuConfig) ([]string, error) {
	qemuCmd := &qemu.QEMUCommandBuilder{}

	qemuCmd.SetBinary(config.binary)
	qemuCmd.SetTarget(qemu.TargetEnum.X86_64, true /* KVM */)
	qemuCmd.SetKernel(config.kernel)
	qemuCmd.SetInitrd(config.initrd)

	qemuCmd.SetCPUCount(4)
	qemuCmd.SetMemory(3072 /* MiB */)

	qemuCmd.AddVirtioBlkPciDrive(qemu.Drive{
		ID:   "d0",
		File: config.blk,
	})

	network := qemu.Netdev{
		ID:     "net0",
		Device: qemu.Device{Model: qemu.DeviceModelVirtioNetPCI},
		User: &qemu.NetdevUser{
			Network:   "192.168.3.0/24",
			DHCPStart: "192.168.3.9",
			Host:      "192.168.3.2",
			Forwards:  []qemu.Forward{{HostPort: config.port, GuestPort: 22}},
		},
	}
	network.Device.AddOption("mac", "52:54:00:63:5e:7b")
	qemuCmd.AddNetwork(network)

	// The system will halt on a kernel panic instead of rebooting.
	qemuCmd.AddKernelArg("kernel.halt-on-panic=true")
	// Do not print colors.
	qemuCmd.AddKernelArg("TERM=dumb")
	// Necessary to redirect serial to stdout for x86.
	qemuCmd.AddKernelArg("kernel.serial=legacy")

	qemuCmd.SetFlag("-nographic")
	qemuCmd.SetFlag("-monitor", "none")

	// Override the SeaBIOS serial port to keep it from outputting a terminal
	// reset on start.
	qemuCmd.SetFlag("-fw_cfg", "name=etc/sercon-port,string=0")

	// Send serial to a log file. We don't want to attach directly via stdout,
	// because the QEMU process needs to be able outlive this one.
	qemuCmd.SetFlag("-serial", "file:"+config.logFile)

	entropy := make([]byte, 32)
	if _, err := rand.Read(entropy); err == nil {
		qemuCmd.AddKernelArg("kernel.entropy-mixin=" + hex.EncodeToString(entropy))
	}

	return qemuCmd.Build()
}

func fileExists(path string) bool {
	_, err := os.Stat(path)
	return !os.IsNotExist(err)
}

// Prepare files that are needed by QEMU, if they haven't already been prepared.
//
// If Prepare succeeds, it is up to the caller to clean up by calling Kill later.
// However, if it fails, any resources will have been automatically released.
func (q *QemuLauncher) Prepare() (returnErr error) {
	paths, err := q.build.Path("zbi", "fvm", "blk", "zbitool")
	if err != nil {
		return fmt.Errorf("Error resolving qemu dependencies: %s", err)
	}
	zbi, fvm, blk, zbitool := paths[0], paths[1], paths[2], paths[3]

	// Create a tmpdir to store files we need
	if q.TmpDir == "" {
		tmpDir, err := ioutil.TempDir("", "clusterfuchsia-qemu-")
		if err != nil {
			return fmt.Errorf("Error creating tempdir: %s", err)
		}
		q.TmpDir = tmpDir
	}

	// If we fail after this point, we need to make sure to clean up
	defer func() {
		if returnErr != nil {
			q.cleanup()
		}
	}()

	sshKey := filepath.Join(q.TmpDir, "sshid")
	sshPublicKey := filepath.Join(q.TmpDir, "sshid.pub")
	initrd := filepath.Join(q.TmpDir, "ssh-"+path.Base(zbi))
	if !fileExists(sshKey) || !fileExists(sshPublicKey) {
		// Generate SSH key pair
		key, err := createSSHKey()
		if err != nil {
			return fmt.Errorf("error generating ssh key: %s", err)
		}
		if err := writeSSHPrivateKeyFile(key, sshKey); err != nil {
			return fmt.Errorf("error writing ssh key: %s", err)
		}
		if err := writeSSHPublicKeyFile(key, sshPublicKey); err != nil {
			return fmt.Errorf("error writing ssh public key: %s", err)
		}
		// Force rebuild of any existing ZBI in the next step, to reflect the new key
		if err := os.RemoveAll(initrd); err != nil {
			return fmt.Errorf("error removing zbi: %s", err)
		}
	}
	q.sshKey = sshKey
	q.sshPublicKey = sshPublicKey

	// Stick our SSH key into the authorized_keys files
	if !fileExists(initrd) {
		entry := "data/ssh/authorized_keys=" + q.sshPublicKey
		if err := CreateProcessForeground(zbitool, "-o", initrd, zbi, "-e", entry); err != nil {
			return fmt.Errorf("adding ssh key failed: %s", err)
		}
	}
	q.initrd = initrd

	// Make an expanded copy of the disk image
	extendedBlk := path.Join(q.TmpDir, "extended-"+path.Base(blk))
	if !fileExists(extendedBlk) {
		if err := CreateProcessForeground("cp", blk, extendedBlk); err != nil {
			return fmt.Errorf("cp failed: %s", err)
		}
		if err := CreateProcessForeground(fvm, extendedBlk, "extend",
			"--length", "3G"); err != nil {
			return fmt.Errorf("fvm failed: %s", err)
		}
	}
	q.extendedBlk = extendedBlk

	return nil
}

// Start launches QEMU and waits for it to get through the basic boot sequence.
// Note that networking will not necessarily be fully up by the time Start() returns
//
// If Start succeeds, it is up to the caller to clean up by calling Kill later.
// However, if it fails, any resources will have been automatically released.
func (q *QemuLauncher) Start() (conn Connector, returnErr error) {
	running, err := q.IsRunning()
	if err != nil {
		return nil, fmt.Errorf("Error checking run state: %s", err)
	}

	if running {
		return nil, fmt.Errorf("Start called but already running")
	}

	paths, err := q.build.Path("qemu", "kernel")
	if err != nil {
		return nil, fmt.Errorf("Error resolving qemu dependencies: %s", err)
	}
	binary, kernel := paths[0], paths[1]

	port, err := getFreePort()
	if err != nil {
		return nil, fmt.Errorf("couldn't get free port: %s", err)
	}

	if err := q.Prepare(); err != nil {
		return nil, fmt.Errorf("error while preparing to start: %s", err)
	}

	// If we fail after this point, we need to make sure to clean up
	defer func() {
		if returnErr != nil {
			q.cleanup()
		}
	}()

	invocation, err := getQemuInvocation(qemuConfig{
		binary:  binary,
		kernel:  kernel,
		initrd:  q.initrd,
		blk:     q.extendedBlk,
		logFile: q.logPath(),
		port:    port})
	if err != nil {
		return nil, fmt.Errorf("qemu configuration error: %s", err)
	}

	cmd := NewCommand(invocation[0], invocation[1:]...)

	logFile, err := os.Create(q.logPath())
	if err != nil {
		return nil, fmt.Errorf("error creating qemu log: %s", err)
	}
	defer logFile.Close()

	// Save output from QEMU itself in case of error. This should be very
	// minimal in size if present at all.
	outPipe, err := cmd.StdoutPipe()
	if err != nil {
		return nil, fmt.Errorf("error attaching stdout: %s", err)
	}
	cmd.Stderr = cmd.Stdout // capture stderr too

	// Storage for system boot log
	var logBuf bytes.Buffer
	logBuf.Grow(64 * 1024) // 64KB should be plenty

	errCh := make(chan error)
	go func() {
		logReader := bufio.NewReader(logFile)

		// Tail the QEMU log file
		for {
			// Wait for input to be available
			for readAttempts := 0; ; readAttempts++ {
				// There is an overall timeout at a higher level, but if we are
				// not getting any input coming in at all let's check on the
				// process in case we can bail sooner (and to ensure that the
				// goroutine eventually gets cleaned up).
				if q.Pid != 0 && readAttempts > 2 {
					alive, err := q.IsRunning()
					if err != nil {
						errCh <- fmt.Errorf("error checking process status: %s", err)
						return
					}
					if !alive {
						errCh <- fmt.Errorf("qemu exited early")
						return
					}
				}

				_, err := logReader.Peek(1)
				if err == nil {
					break
				} else if err != io.EOF {
					errCh <- fmt.Errorf("log peek failed: %s", err)
					return
				}
				time.Sleep(100 * time.Millisecond)
			}

			if _, err := logBuf.ReadFrom(logReader); err != nil {
				errCh <- fmt.Errorf("log read failed: %s", err)
				return
			}

			// This is not as performant as something like a Scanner, but those
			// can't recover from EOFs and it doesn't seem worth reimplementing.
			if bytes.Contains(logBuf.Bytes(), []byte(successfulBootMarker)) {
				// Early boot has finished
				errCh <- nil
				return
			}
		}
	}()

	if err := cmd.Start(); err != nil {
		return nil, fmt.Errorf("failed to start qemu: %s", err)
	}

	q.Pid = cmd.Process.Pid

	glog.Info("Waiting for boot to complete...")

	select {
	case err := <-errCh:
		if err != nil {
			// Kill the process, just in case this was an error other than EOF
			cmd.Process.Kill()

			dumpLogs(outPipe, logBuf)
			return nil, fmt.Errorf("error during boot: %s", err)
		}
	case <-time.After(q.timeout):
		// Kill the process, and wait for the goroutine above to terminate
		cmd.Process.Kill()
		<-errCh

		dumpLogs(outPipe, logBuf)
		return nil, fmt.Errorf("timeout waiting for boot")
	}

	glog.Info("Instance started.")

	// Detach from the child, since we will never wait on it
	outPipe.Close()
	cmd.Process.Release()

	return NewSSHConnector("localhost", port, q.sshKey), nil
}

// IsRunning checks if the qemu process is alive
func (q *QemuLauncher) IsRunning() (bool, error) {
	if q.Pid == 0 {
		return false, nil
	}

	if err := CreateProcess("ps", "-p", strconv.Itoa(q.Pid)); err != nil {
		if cmderr, ok := err.(*exec.ExitError); ok && cmderr.ExitCode() == 1 {
			return false, nil
		}
		return false, fmt.Errorf("error running ps: %s", err)
	}

	return true, nil
}

// Cleans up any temporary files used by the launcher
func (q *QemuLauncher) cleanup() {
	if q.TmpDir != "" {
		if err := os.RemoveAll(q.TmpDir); err != nil {
			glog.Warningf("failed to remove temp dir: %s", err)
		}
		q.TmpDir = ""
	}
}

// Kill tells the QEMU process to terminate, and cleans up the TmpDir
func (q *QemuLauncher) Kill() error {
	if q.Pid != 0 {
		glog.Infof("Killing PID %d", q.Pid)

		// TODO(fxbug.dev/45431): More gracefully, with timeout
		if err := syscall.Kill(q.Pid, syscall.SIGKILL); err != nil {
			glog.Warningf("failed to kill instance: %s", err)
		}

		q.Pid = 0
	}

	q.cleanup()

	return nil
}

func (q *QemuLauncher) logPath() string {
	return filepath.Join(q.TmpDir, "qemu.log")
}

// GetLogs writes any system logs from QEMU to `out`
func (q *QemuLauncher) GetLogs(out io.Writer) error {
	f, err := os.Open(q.logPath())
	if err != nil {
		return fmt.Errorf("error opening file: %s", err)
	}
	defer f.Close()

	if _, err := io.Copy(out, f); err != nil {
		return fmt.Errorf("error while dumping log file %q: %s", q.logPath(), err)
	}

	return nil
}

// Dump the qemu and boot logs
func dumpLogs(outPipe io.Reader, logBuf bytes.Buffer) {
	if errLog, err := ioutil.ReadAll(outPipe); err != nil {
		glog.Errorf("error reading QEMU output: %s", err)
	} else {
		glog.Errorf("QEMU output: %s", errLog)
	}
	glog.Errorf("Boot log: %s", logBuf.String())
}

func loadLauncherFromHandle(build Build, handle Handle) (Launcher, error) {
	handleData, err := handle.GetData()
	if err != nil {
		return nil, err
	}

	// Check that the Launcher is in a valid state
	switch launcher := handleData.launcher.(type) {
	case *QemuLauncher:
		if launcher.Pid == 0 {
			return nil, fmt.Errorf("pid not found in handle")
		}

		if launcher.TmpDir == "" {
			return nil, fmt.Errorf("tmpdir not found in handle")
		}

		launcher.build = build
		return launcher, nil
	default:
		return nil, fmt.Errorf("unknown launcher type: %T", handleData.launcher)
	}
}
