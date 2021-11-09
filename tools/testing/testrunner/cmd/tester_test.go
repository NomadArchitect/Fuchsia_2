// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"context"
	"fmt"
	"io"
	"io/ioutil"
	"math/rand"
	"net"
	"os"
	"path/filepath"
	"reflect"
	"sync"
	"testing"
	"time"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder"
	"go.fuchsia.dev/fuchsia/tools/lib/iomisc"
	"go.fuchsia.dev/fuchsia/tools/lib/retry"
	"go.fuchsia.dev/fuchsia/tools/net/sshutil"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
	"golang.org/x/sync/errgroup"

	"github.com/google/go-cmp/cmp"
)

type fakeSSHClient struct {
	reconnectErrs  []error
	reconnectCalls int
	runErrs        []error
	runCalls       int
	lastCmd        []string
}

func (c *fakeSSHClient) Run(_ context.Context, command []string, _, _ io.Writer) error {
	c.runCalls++
	c.lastCmd = command
	if c.runErrs == nil {
		return nil
	}
	err, remainingErrs := c.runErrs[0], c.runErrs[1:]
	c.runErrs = remainingErrs
	return err
}

func (c *fakeSSHClient) Close() {}

func (c *fakeSSHClient) Reconnect(_ context.Context) error {
	c.reconnectCalls++
	if c.reconnectErrs == nil {
		return nil
	}
	err, remainingErrs := c.reconnectErrs[0], c.reconnectErrs[1:]
	c.reconnectErrs = remainingErrs
	return err
}

type fakeCmdRunner struct {
	runErrs  []error
	runCalls int
	lastCmd  []string
}

func (r *fakeCmdRunner) Run(_ context.Context, command []string, _, _ io.Writer) error {
	r.runCalls++
	r.lastCmd = command
	if r.runErrs == nil {
		return nil
	}
	err, remainingErrs := r.runErrs[0], r.runErrs[1:]
	r.runErrs = remainingErrs
	return err
}

func TestSubprocessTester(t *testing.T) {
	tmpDir := t.TempDir()
	tester := subprocessTester{
		localOutputDir: tmpDir,
		getModuleBuildIDs: func(test string) ([]string, error) {
			name := filepath.Base(test)
			return []string{name + "-BUILD-ID1", name + "-BUILD-ID2"}, nil
		},
	}

	passingTest := filepath.Join("host_x64", "passing")
	passingProfile := filepath.Join("llvm-profile", passingTest+".profraw")
	failingTest := filepath.Join("host_x64", "failing")
	failingProfile := filepath.Join("llvm-profile", failingTest+".profraw")
	for _, profile := range []string{passingProfile, failingProfile} {
		abs := filepath.Join(tmpDir, profile)
		os.MkdirAll(filepath.Dir(abs), 0o700)
		f, err := os.Create(abs)
		if err != nil {
			t.Fatalf("failed to create profile: %v", err)
		}
		f.Close()
	}

	cases := []struct {
		name          string
		test          build.Test
		runErrs       []error
		wantErr       bool
		wantCmd       []string
		wantDataSinks runtests.DataSinkMap
	}{
		{
			name:    "no path",
			test:    build.Test{},
			wantErr: true,
		},
		{
			name:    "test passes with profile",
			test:    build.Test{Path: passingTest},
			wantErr: false,
			wantCmd: []string{passingTest},
			wantDataSinks: runtests.DataSinkMap{
				"llvm-profile": []runtests.DataSink{
					{
						Name:     filepath.Base(passingProfile),
						File:     passingProfile,
						BuildIDs: []string{"passing-BUILD-ID1", "passing-BUILD-ID2"},
					},
				},
			},
		},
		{
			name:          "test passes without profile",
			test:          build.Test{Path: "uninstrumented_test"},
			wantErr:       false,
			wantCmd:       []string{"uninstrumented_test"},
			wantDataSinks: nil,
		},
		{
			name:    "test fails",
			test:    build.Test{Path: failingTest},
			runErrs: []error{fmt.Errorf("test failed")},
			wantErr: true,
			wantCmd: []string{failingTest},
			wantDataSinks: runtests.DataSinkMap{
				"llvm-profile": []runtests.DataSink{
					{
						Name:     filepath.Base(failingProfile),
						File:     failingProfile,
						BuildIDs: []string{"failing-BUILD-ID1", "failing-BUILD-ID2"},
					},
				},
			},
		},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			runner := &fakeCmdRunner{
				runErrs: c.runErrs,
			}
			newRunner = func(dir string, env []string) cmdRunner {
				return runner
			}
			outDir := filepath.Join(tmpDir, c.test.Path)
			ref, err := tester.Test(context.Background(), testsharder.Test{Test: c.test}, ioutil.Discard, ioutil.Discard, outDir)
			if gotErr := (err != nil); gotErr != c.wantErr {
				t.Errorf("tester.Test got error: %v, want error: %t", err, c.wantErr)
			}
			if err == nil {
				if _, statErr := os.Stat(outDir); statErr != nil {
					t.Error("tester.Test did not create a readable outDir:", statErr)
				}
			}
			if diff := cmp.Diff(c.wantCmd, runner.lastCmd); diff != "" {
				t.Errorf("Unexpected command run (-want +got):\n%s", diff)
			}

			sinks := runtests.DataSinkMap(ref)
			if !reflect.DeepEqual(sinks, c.wantDataSinks) {
				t.Fatalf("expected: %#v;\nactual: %#v", c.wantDataSinks, sinks)
			}
		})
	}
}

type fakeDataSinkCopier struct {
	reconnectCalls int
}

func (*fakeDataSinkCopier) GetReference() (runtests.DataSinkReference, error) {
	return runtests.DataSinkReference{}, nil
}

func (*fakeDataSinkCopier) Copy(_ []runtests.DataSinkReference, _ string) (runtests.DataSinkMap, error) {
	return runtests.DataSinkMap{}, nil
}

func (c *fakeDataSinkCopier) Reconnect() error {
	c.reconnectCalls++
	return nil
}

func (*fakeDataSinkCopier) Close() error {
	return nil
}

func TestSSHTester(t *testing.T) {
	cases := []struct {
		name            string
		runErrs         []error
		reconErrs       []error
		copierReconErrs []error
		wantErr         bool
		wantConnErr     bool
		runSnapshot     bool
	}{
		{
			name:    "success",
			runErrs: []error{nil},
			wantErr: false,
		},
		{
			name:    "test failure",
			runErrs: []error{fmt.Errorf("test failed")},
			wantErr: true,
		},
		{
			name:      "connection error retry and test failure",
			runErrs:   []error{sshutil.ConnectionError{}, fmt.Errorf("test failed")},
			reconErrs: []error{nil},
			wantErr:   true,
		},
		{
			name:      "reconnect succeeds then fails",
			runErrs:   []error{sshutil.ConnectionError{}, sshutil.ConnectionError{}},
			reconErrs: []error{nil, fmt.Errorf("reconnect failed")},
			wantErr:   true,
			// Make sure we return the original ConnectionError and not the error from the failed
			// reconnect attempt. This is important because the code that calls Test() in a loop
			// aborts the loop when it sees an ConnectionError.
			wantConnErr: true,
		},
		{
			name:      "reconnect succeeds thrice",
			runErrs:   []error{sshutil.ConnectionError{}, sshutil.ConnectionError{}, sshutil.ConnectionError{}},
			reconErrs: []error{nil, nil, nil},
			wantErr:   true,
			// Reconnection succeeds so we don't want the caller to see a ConnectionError.
			wantConnErr: false,
		},
		{
			name:        "reconnect before snapshot",
			runErrs:     []error{nil, sshutil.ConnectionError{}, nil},
			reconErrs:   []error{nil},
			wantErr:     false,
			wantConnErr: false,
			runSnapshot: true,
		},
	}
	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			client := &fakeSSHClient{
				reconnectErrs: c.reconErrs,
				runErrs:       c.runErrs,
			}
			copier := &fakeDataSinkCopier{}
			tester := fuchsiaSSHTester{
				client:                      client,
				copier:                      copier,
				connectionErrorRetryBackoff: &retry.ZeroBackoff{},
			}
			eg := errgroup.Group{}
			serialServer := fakeSerialServer{
				received:       make([]byte, 1024),
				shutdownString: "shutdown",
				socketPath:     fmt.Sprintf("%d.sock", rand.Uint32()),
				listeningChan:  make(chan bool),
			}
			if c.wantConnErr {
				serialDiagnosticCmds = []serialDiagnosticCmd{
					{
						cmd: []string{"foo"},
						// Ensure we don't waste time sleeping in this test.
						sleepDuration: time.Microsecond,
					},
					{
						// This is a hack to ensure the shutdown command gets sent to the serial server.
						// Rather than introduce a new synchronization mechanism, just use the code under test's
						// existing mechanism for sending commands.
						cmd: []string{serialServer.shutdownString},
						// Ensure we don't waste time sleeping in this test.
						sleepDuration: time.Microsecond,
					},
				}
				tester.serialSocketPath = serialServer.socketPath
				defer os.Remove(serialServer.socketPath)
				eg.Go(serialServer.Serve)
				if !<-serialServer.listeningChan {
					t.Fatalf("fakeSerialServer.Serve() returned: %v", eg.Wait())
				}
			}
			defer func() {
				if err := tester.Close(); err != nil {
					t.Errorf("Close returned error: %v", err)
				}
			}()
			wantReconnCalls := len(c.reconErrs)
			wantRunCalls := len(c.runErrs)
			test := testsharder.Test{
				Test:         build.Test{PackageURL: "fuchsia-pkg://foo"},
				Runs:         1,
				RunAlgorithm: testsharder.StopOnSuccess,
			}
			_, err := tester.Test(context.Background(), test, ioutil.Discard, ioutil.Discard, "unused-out-dir")
			if err == nil {
				if c.wantErr {
					t.Errorf("tester.Test got nil error, want non-nil error")
				}
			} else {
				if !c.wantErr {
					t.Errorf("tester.Test got error: %v, want nil", err)
				}
				if isConnErr := sshutil.IsConnectionError(err); isConnErr != c.wantConnErr {
					t.Errorf("got isConnErr: %t, want: %t", isConnErr, c.wantConnErr)
				}
			}

			if c.runSnapshot {
				p := filepath.Join(t.TempDir(), "testrunner-cmd-test")
				if err = tester.RunSnapshot(context.Background(), p); err != nil {
					t.Errorf("failed to run snapshot: %v", err)
				}
			}

			if wantReconnCalls != client.reconnectCalls {
				t.Errorf("Reconnect() called wrong number of times. Got: %d, Want: %d", client.reconnectCalls, wantReconnCalls)
			}

			reconnFailures := 0
			for _, err := range c.reconErrs {
				if err != nil {
					reconnFailures++
				}
			}
			// The copier shouldn't be reconnected if reconnecting the ssh
			// client fails.
			wantCopierReconnCalls := wantReconnCalls - reconnFailures
			if wantCopierReconnCalls != copier.reconnectCalls {
				t.Errorf("Reconnect() called wrong number of times. Got: %d, Want: %d", copier.reconnectCalls, wantReconnCalls)
			}

			if wantRunCalls != client.runCalls {
				t.Errorf("Run() called wrong number of times. Got: %d, Want: %d", client.runCalls, wantRunCalls)
			}

			if c.wantConnErr {
				if err = eg.Wait(); err != nil {
					t.Errorf("serialServer.Serve() failed: %v", err)
				}
				// Verify that each command was seen in the received data, and in the
				// proper order. Ignore the shutdown command we appended at the end
				searchPos := 0
				for _, cmd := range serialDiagnosticCmds[:len(serialDiagnosticCmds)-2] {
					index := bytes.Index(serialServer.received[searchPos:], []byte(asSerialCmd(cmd.cmd)))
					if index == -1 {
						t.Errorf("SSHTester did find the command \"%v\" in the proper order, all received: %s", cmd, string(serialServer.received))
					} else {
						searchPos = index + 1
					}
				}
			}
		})
	}
}

// test that v2 tests are skipped when runtests binary is used.
func TestRunTestsWithV2Tests(t *testing.T) {

	tester := fuchsiaSSHTester{
		client: &fakeSSHClient{
			runErrs: []error{nil},
		},
		copier:                      &fakeDataSinkCopier{},
		connectionErrorRetryBackoff: &retry.ZeroBackoff{},
		useRuntests:                 true,
	}
	defer func() {
		if err := tester.Close(); err != nil {
			t.Errorf("Close returned error: %v", err)
		}
	}()

	test := testsharder.Test{
		Test:         build.Test{PackageURL: "fuchsia-pkg://foo#meta/bar.cm"},
		Runs:         1,
		RunAlgorithm: testsharder.StopOnSuccess,
	}
	_, err := tester.Test(context.Background(), test, ioutil.Discard, ioutil.Discard, "unused-out-dir")
	if !isTestSkippedErr(err) {
		t.Errorf("expected test to be skipped, got %v", err)
	}
}

// Creates pair of ReadWriteClosers that mimics the relationship between serial
// and socket i/o. Implemented with in-memory pipes, the input of one can
// synchronously by read as the output of the other.
func serialAndSocket() (io.ReadWriteCloser, io.ReadWriteCloser) {
	rSerial, wSocket := io.Pipe()
	rSocket, wSerial := io.Pipe()
	serial := &joinedPipeEnds{rSerial, wSerial}
	socket := &joinedPipeEnds{rSocket, wSocket}
	return serial, socket
}

type fakeSerialServer struct {
	received       []byte
	shutdownString string
	socketPath     string
	listeningChan  chan bool
}

func (s *fakeSerialServer) Serve() error {
	listener, err := net.Listen("unix", s.socketPath)
	if err != nil {
		s.listeningChan <- false
		return fmt.Errorf("Listen(%s) failed: %v", s.socketPath, err)
	}
	defer listener.Close()
	s.listeningChan <- true
	conn, err := listener.Accept()
	if err != nil {
		return fmt.Errorf("Accept() failed: %v", err)
	}
	defer conn.Close()
	// Signal we're ready to accept input.
	if _, err := conn.Write([]byte(serialConsoleCursor)); err != nil {
		return fmt.Errorf("conn.Write() failed: %v", err)
	}
	reader := iomisc.NewMatchingReader(conn, [][]byte{[]byte(s.shutdownString)})
	for {
		buf := make([]byte, 1024)
		bytesRead, err := reader.Read(buf)
		s.received = append(s.received, buf[:bytesRead]...)
		if err != nil {
			if err == io.EOF {
				return nil
			}
			return fmt.Errorf("conn.Read() failed: %v", err)
		}
	}
}

func TestNewSerialSocket(t *testing.T) {
	socketPath := fmt.Sprintf("%d.sock", rand.Uint32())
	defer os.Remove(socketPath)
	server := fakeSerialServer{
		shutdownString: "dm shutdown",
		socketPath:     socketPath,
		listeningChan:  make(chan bool),
	}
	eg := errgroup.Group{}
	eg.Go(server.Serve)

	if !<-server.listeningChan {
		t.Fatalf("fakeSerialServer.Serve() returned: %v", eg.Wait())
	}

	clientSocket, err := newSerialSocket(context.Background(), socketPath)
	if err != nil {
		t.Fatalf("newSerialSocket() failed: %v", err)
	}
	bytesWritten, err := clientSocket.Write([]byte(server.shutdownString))
	if err != nil {
		t.Errorf("clientSocket.Write() failed: %v", err)
	}
	if bytesWritten != len(server.shutdownString) {
		t.Errorf("clientSocket.Write() wrote %d bytes, wanted %d", bytesWritten, len(server.shutdownString))
	}
	if err = eg.Wait(); err != nil {
		t.Errorf("server returned: %v", err)
	}
	if err = clientSocket.Close(); err != nil {
		t.Errorf("clientSocket.Close() returned: %v", err)
	}
	// First newline should be sent by newSerialSocket to trigger a cursor.
	if diff := cmp.Diff("\r\n\r\n"+server.shutdownString, string(server.received)); diff != "" {
		t.Errorf("Unexpected server.received (-want +got):\n%s", diff)
	}
}

// fakeContext conforms to context.Context but lets us control the return
// value of Err().
type fakeContext struct {
	sync.Mutex
	err error
}

func (ctx *fakeContext) Deadline() (time.Time, bool) {
	return time.Time{}, false
}

func (ctx *fakeContext) Done() <-chan struct{} {
	ch := make(chan struct{})
	close(ch)
	return ch
}

func (ctx *fakeContext) Err() error {
	ctx.Lock()
	defer ctx.Unlock()
	return ctx.err
}

func (ctx *fakeContext) SetErr(err error) {
	ctx.Lock()
	ctx.err = err
	ctx.Unlock()
}

func (ctx *fakeContext) Value(key interface{}) interface{} {
	return nil
}

func TestSerialTester(t *testing.T) {
	ctx := context.Background()
	serial, socket := serialAndSocket()
	defer socket.Close()
	defer serial.Close()

	tester := fuchsiaSerialTester{socket: socket}
	test := testsharder.Test{
		Test: build.Test{
			Name: "myfoo",
			Path: "foo",
		},
	}
	expectedCmd := "\r\nruntests --output /data/infra/testrunner foo\r\n"

	t.Run("test passes", func(t *testing.T) {
		errs := make(chan error)
		go func() {
			_, err := tester.Test(ctx, test, ioutil.Discard, ioutil.Discard, "unused-out-dir")
			errs <- err
		}()

		// The write to the socket will block until we read from serial.
		buff := make([]byte, len(expectedCmd))
		if _, err := io.ReadFull(serial, buff); err != nil {
			t.Errorf("error reading from serial: %v", err)
		} else if string(buff) != expectedCmd {
			t.Errorf("unexpected command: %s", buff)
		}

		// At this point, the tester will be blocked reading from the socket.
		// Ensure we handle the StartedSignature and SuccessSignature in a single write.
		startedAndSucceded := runtests.StartedSignature + test.Name + "\n" + runtests.SuccessSignature + test.Name
		if _, err := io.WriteString(serial, startedAndSucceded); err != nil {
			t.Errorf("failed to write %s to serial", startedAndSucceded)
		}

		select {
		case err := <-errs:
			if err != nil {
				t.Error("test unexpectedly failed")
			}
		}
	})
	t.Run("test fails", func(t *testing.T) {
		errs := make(chan error)
		go func() {
			_, err := tester.Test(ctx, test, ioutil.Discard, ioutil.Discard, "unused-out-dir")
			errs <- err
		}()
		// The write to the socket will block until we read from serial.
		buff := make([]byte, len(expectedCmd))
		if _, err := io.ReadFull(serial, buff); err != nil {
			t.Errorf("error reading from serial: %v", err)
		} else if string(buff) != expectedCmd {
			t.Errorf("unexpected command: %s", buff)
		}

		// At this point, the tester will be blocked reading from the socket.
		started := runtests.StartedSignature + test.Name
		if _, err := io.WriteString(serial, started); err != nil {
			t.Errorf("failed to write %s to serial", started)
		}
		failureReturn := runtests.FailureSignature + test.Name
		if _, err := io.WriteString(serial, failureReturn); err != nil {
			t.Errorf("failed to write %s to serial", failureReturn)
		}

		select {
		case err := <-errs:
			if err == nil {
				t.Error("test unexpectedly passed")
			}
		}
	})
	t.Run("test does not start on first try", func(t *testing.T) {
		oldNewTestStartedContext := newTestStartedContext
		defer func() {
			newTestStartedContext = oldNewTestStartedContext
		}()
		var fakeTestStartedContext fakeContext
		fakeTestStartedCancel := func() {}
		newTestStartedContext = func(ctx context.Context) (context.Context, context.CancelFunc) {
			return &fakeTestStartedContext, fakeTestStartedCancel
		}
		errs := make(chan error)
		go func() {
			_, err := tester.Test(ctx, test, ioutil.Discard, ioutil.Discard, "unused-out-dir")
			errs <- err
		}()

		// Ensure tester times out waiting for the test to start.
		fakeTestStartedContext.SetErr(context.DeadlineExceeded)

		// The tester's write to the socket will block until we read from serial.
		buff := make([]byte, len(expectedCmd))
		for i := 0; i < 2; i++ {
			if _, err := io.ReadFull(serial, buff); err != nil {
				t.Errorf("error reading from serial: %v", err)
			} else if string(buff) != expectedCmd {
				t.Errorf("unexpected command: %s", string(buff))
			}
		}

		fakeTestStartedContext.SetErr(nil)
		// At this point, the tester will be blocked reading from the socket.
		if _, err := io.WriteString(serial, runtests.StartedSignature+test.Name); err != nil {
			t.Errorf("failed to write to serial")
		}
		successReturn := runtests.SuccessSignature + test.Name
		if _, err := io.WriteString(serial, successReturn); err != nil {
			t.Errorf("failed to write %s to serial", successReturn)
		}

		select {
		case err := <-errs:
			if err != nil {
				t.Error("test unexpectedly failed")
			}
		}
	})

}

func TestCommandForTest(t *testing.T) {
	cases := []struct {
		name        string
		test        testsharder.Test
		useRuntests bool
		timeout     time.Duration
		expected    []string
		wantErr     bool
	}{
		{
			name:        "use runtests URL",
			useRuntests: true,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cmx",
				}},
			expected: []string{"runtests", "--output", "REMOTE_DIR", "fuchsia-pkg://example.com/test.cmx"},
		},
		{
			name:        "use runtests path",
			useRuntests: true,
			test: testsharder.Test{
				Test: build.Test{
					Path: "/path/to/test",
				}},
			expected: []string{"runtests", "--output", "REMOTE_DIR", "/path/to/test"},
		},
		{
			name:        "use runtests timeout",
			useRuntests: true,
			test: testsharder.Test{
				Test: build.Test{
					Path: "/path/to/test",
				}},
			timeout:  time.Second,
			expected: []string{"runtests", "--output", "REMOTE_DIR", "-i", "1", "/path/to/test"},
		},
		{
			name:        "use runtests realm-label",
			useRuntests: true,
			test: testsharder.Test{
				Test: build.Test{
					Path: "/path/to/test",
				},
				RealmLabel: "testrealm",
			},
			expected: []string{"runtests", "--output", "REMOTE_DIR", "--realm-label", "testrealm", "/path/to/test"},
		},
		{
			name:        "system path",
			useRuntests: false,
			test: testsharder.Test{
				Test: build.Test{
					Path: "/path/to/test",
				}},
			wantErr: true,
		},
		{
			name:        "components v1",
			useRuntests: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cmx",
				}},
			expected: []string{"run-test-component", "--wait-for-utc=10", "fuchsia-pkg://example.com/test.cmx"},
		},
		{
			name:        "components v1 timeout",
			useRuntests: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cmx",
				}},
			timeout:  time.Second,
			expected: []string{"run-test-component", "--timeout=1", "--wait-for-utc=10", "fuchsia-pkg://example.com/test.cmx"},
		},
		{
			name:        "components v1 max severity",
			useRuntests: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:        "/path/to/test",
					PackageURL:  "fuchsia-pkg://example.com/test.cmx",
					LogSettings: build.LogSettings{MaxSeverity: "ERROR"},
				}},
			timeout:  time.Second,
			expected: []string{"run-test-component", "--max-log-severity=ERROR", "--timeout=1", "--wait-for-utc=10", "fuchsia-pkg://example.com/test.cmx"},
		},
		{
			name:        "components v1 realm label",
			useRuntests: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cmx",
				},
				RealmLabel: "testrealm",
			},
			expected: []string{"run-test-component", "--realm-label", "testrealm", "--wait-for-utc=10", "fuchsia-pkg://example.com/test.cmx"},
		},
		{
			name:        "components v2",
			useRuntests: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cm",
				}},
			expected: []string{"run-test-suite", "--wait-for-utc", "10", "fuchsia-pkg://example.com/test.cm"},
		},
		{
			name:        "components v2 no parallel",
			useRuntests: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cm",
				}},
			expected: []string{"run-test-suite", "--wait-for-utc", "10", "fuchsia-pkg://example.com/test.cm"},
		},
		{
			name:        "components v2 parallel",
			useRuntests: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cm",
					Parallel:   2,
				}},
			expected: []string{"run-test-suite", "--parallel", "2", "--wait-for-utc", "10", "fuchsia-pkg://example.com/test.cm"},
		},
		{
			name:        "components v2 timeout",
			useRuntests: false,
			test: testsharder.Test{
				Test: build.Test{
					Path:       "/path/to/test",
					PackageURL: "fuchsia-pkg://example.com/test.cm",
				}},
			timeout:  time.Second,
			expected: []string{"run-test-suite", "--timeout", "1", "--wait-for-utc", "10", "fuchsia-pkg://example.com/test.cm"},
		},
	}

	for _, c := range cases {
		t.Run(c.name, func(t *testing.T) {
			command, err := commandForTest(&c.test, c.useRuntests, "REMOTE_DIR", c.timeout)
			if err == nil {
				if c.wantErr {
					t.Errorf("commandForTest returned nil error, want non-nil error")
				}
			} else if !c.wantErr {
				t.Errorf("commandForTest returned error: %v, want nil", err)
			}
			if !reflect.DeepEqual(command, c.expected) {
				t.Errorf("unexpected command:\nexpected: %q\nactual: %q\n", c.expected, command)
			}
		})

	}
}

type joinedPipeEnds struct {
	r *io.PipeReader
	w *io.PipeWriter
}

func (pe *joinedPipeEnds) Read(p []byte) (int, error) {
	return pe.r.Read(p)
}

func (pe *joinedPipeEnds) Write(p []byte) (int, error) {
	return pe.w.Write(p)
}

func (pe *joinedPipeEnds) Close() error {
	if err := pe.r.Close(); err != nil {
		pe.w.Close()
		return err
	}
	return pe.w.Close()
}
