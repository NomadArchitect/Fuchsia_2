// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"io/ioutil"
	"log"
	"net"
	"net/url"
	"os"
	"path/filepath"
	"runtime"
	"strconv"
	"strings"
	"sync"
	"time"

	"go.fuchsia.dev/fuchsia/tools/botanist/constants"
	"go.fuchsia.dev/fuchsia/tools/botanist/target"
	"go.fuchsia.dev/fuchsia/tools/integration/testsharder"
	"go.fuchsia.dev/fuchsia/tools/lib/clock"
	"go.fuchsia.dev/fuchsia/tools/lib/color"
	"go.fuchsia.dev/fuchsia/tools/lib/environment"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
	"go.fuchsia.dev/fuchsia/tools/testing/tap"
	"go.fuchsia.dev/fuchsia/tools/testing/testparser"
	"go.fuchsia.dev/fuchsia/tools/testing/testrunner"
)

// Fuchsia-specific environment variables possibly exposed to the testrunner.
const (
	// A directory that will be automatically archived on completion of a task.
	testOutDirEnvKey = "FUCHSIA_TEST_OUTDIR"
)

// Command-line flags
var (
	// Whether to show Usage and exit.
	help bool

	// The path where a directory containing test results should be created.
	outDir string

	// Working directory of the local testing subprocesses.
	localWD string

	// Whether to use runtests when executing tests on fuchsia. If false, the
	// default will be run_test_component.
	useRuntests bool

	// The output filename for the snapshot. This will be created in the outDir.
	snapshotFile string

	// Per-test timeout.
	perTestTimeout time.Duration

	// Logger level.
	level = logger.InfoLevel
)

func usage() {
	fmt.Printf(`testrunner [flags] tests-file

Executes all tests found in the JSON [tests-file]
Fuchsia tests require both the node address of the fuchsia instance and a private
SSH key corresponding to a authorized key to be set in the environment under
%s and %s respectively.
`, constants.DeviceAddrEnvKey, constants.SSHKeyEnvKey)
}

func main() {
	flag.BoolVar(&help, "help", false, "Whether to show Usage and exit.")
	flag.StringVar(&outDir, "out-dir", "", "Optional path where a directory containing test results should be created.")
	flag.StringVar(&localWD, "C", "", "Working directory of local testing subprocesses; if unset the current working directory will be used.")
	flag.BoolVar(&useRuntests, "use-runtests", false, "Whether to default to running fuchsia tests with runtests; if false, run_test_component will be used.")
	flag.StringVar(&snapshotFile, "snapshot-output", "", "The output filename for the snapshot. This will be created in the output directory.")
	// TODO(fxbug.dev/36480): Support different timeouts for different tests.
	flag.DurationVar(&perTestTimeout, "per-test-timeout", 0, "Per-test timeout, applied to all tests. Ignored if <= 0.")
	flag.Var(&level, "level", "Output verbosity, can be fatal, error, warning, info, debug or trace.")
	flag.Usage = usage
	flag.Parse()

	if help || flag.NArg() != 1 {
		flag.Usage()
		flag.PrintDefaults()
		return
	}

	const logFlags = log.Ltime | log.Lmicroseconds | log.Lshortfile

	// Our mDNS library doesn't use the logger library.
	log.SetFlags(logFlags)

	log := logger.NewLogger(level, color.NewColor(color.ColorAuto), os.Stdout, os.Stderr, "testrunner ")
	log.SetFlags(logFlags)
	ctx := logger.WithLogger(context.Background(), log)

	testsPath := flag.Arg(0)
	tests, err := loadTests(testsPath)
	if err != nil {
		logger.Fatalf(ctx, "failed to load tests from %q: %s", testsPath, err)
	}

	// Configure a test outputs object, responsible for producing TAP output,
	// recording data sinks, and archiving other test outputs.
	testOutDir := filepath.Join(os.Getenv(testOutDirEnvKey), outDir)
	if testOutDir == "" {
		var err error
		testOutDir, err = ioutil.TempDir("", "testrunner")
		if err != nil {
			logger.Fatalf(ctx, "failed to create a test output directory")
		}
	}
	logger.Debugf(ctx, "test output directory: %s", testOutDir)

	tapProducer := tap.NewProducer(os.Stdout)
	tapProducer.Plan(len(tests))
	outputs, err := createTestOutputs(tapProducer, testOutDir)
	if err != nil {
		logger.Fatalf(ctx, "failed to create test results object: %s", err)
	}
	defer outputs.Close()

	var addr net.IPAddr
	if deviceAddr, ok := os.LookupEnv(constants.DeviceAddrEnvKey); ok {
		addrPtr, err := net.ResolveIPAddr("ip", deviceAddr)
		if err != nil {
			logger.Fatalf(ctx, "failed to parse device address %s: %s", deviceAddr, err)
		}
		addr = *addrPtr
	}
	sshKeyFile := os.Getenv(constants.SSHKeyEnvKey)

	cleanUp, err := environment.Ensure()
	if err != nil {
		logger.Fatalf(ctx, "failed to setup environment: %s", err)
	}
	defer cleanUp()

	serialSocketPath := os.Getenv(constants.SerialSocketEnvKey)
	if err := execute(ctx, tests, outputs, addr, sshKeyFile, serialSocketPath, testOutDir); err != nil {
		logger.Fatalf(ctx, err.Error())
	}
}

func validateTest(test testsharder.Test) error {
	if test.Name == "" {
		return fmt.Errorf("one or more tests missing `name` field")
	}
	if test.OS == "" {
		return fmt.Errorf("one or more tests missing `os` field")
	}
	if test.Runs <= 0 {
		return fmt.Errorf("one or more tests with invalid `runs` field")
	}
	if test.Runs > 1 {
		if test.RunAlgorithm == "" {
			return fmt.Errorf("one or more tests with invalid `run_algorithm` field")
		}
	}
	if test.OS == "fuchsia" && test.PackageURL == "" && test.Path == "" {
		return fmt.Errorf("one or more fuchsia tests missing the `path` and `package_url` fields")
	}
	if test.OS != "fuchsia" {
		if test.PackageURL != "" {
			return fmt.Errorf("one or more host tests have a `package_url` field present")
		} else if test.Path == "" {
			return fmt.Errorf("one or more host tests missing the `path` field")
		}
	}
	return nil
}

func loadTests(path string) ([]testsharder.Test, error) {
	bytes, err := ioutil.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("failed to read %q: %w", path, err)
	}

	var tests []testsharder.Test
	if err := json.Unmarshal(bytes, &tests); err != nil {
		return nil, fmt.Errorf("failed to unmarshal %q: %w", path, err)
	}

	for _, test := range tests {
		if err := validateTest(test); err != nil {
			return nil, err
		}
	}

	return tests, nil
}

type tester interface {
	Test(context.Context, testsharder.Test, io.Writer, io.Writer, string) (runtests.DataSinkReference, error)
	Close() error
	EnsureSinks(context.Context, []runtests.DataSinkReference, *testOutputs) error
	RunSnapshot(context.Context, string) error
}

// TODO: write tests for this function. Tests were deleted in fxrev.dev/407968 as part of a refactoring.
func execute(ctx context.Context, tests []testsharder.Test, outputs *testOutputs, addr net.IPAddr, sshKeyFile, serialSocketPath, outDir string) error {
	var fuchsiaSinks, localSinks []runtests.DataSinkReference
	var fuchsiaTester, localTester tester
	var finalError error

	for _, test := range tests {
		var t tester
		var sinks *[]runtests.DataSinkReference
		switch test.OS {
		case "fuchsia":
			if fuchsiaTester == nil {
				var err error
				if sshKeyFile != "" {
					fuchsiaTester, err = newFuchsiaSSHTester(ctx, addr, sshKeyFile, outputs.outDir, serialSocketPath, useRuntests, perTestTimeout)
				} else {
					if serialSocketPath == "" {
						finalError = fmt.Errorf("%q must be set if %q is not set", constants.SerialSocketEnvKey, constants.SSHKeyEnvKey)
						break
					}
					fuchsiaTester, err = newFuchsiaSerialTester(ctx, serialSocketPath, perTestTimeout)
				}
				if err != nil {
					finalError = fmt.Errorf("failed to initialize fuchsia tester: %w", err)
					break
				}
			}
			t = fuchsiaTester
			sinks = &fuchsiaSinks
		case "linux", "mac":
			if test.OS == "linux" && runtime.GOOS != "linux" {
				finalError = fmt.Errorf("cannot run linux tests when GOOS = %q", runtime.GOOS)
				break
			}
			if test.OS == "mac" && runtime.GOOS != "darwin" {
				finalError = fmt.Errorf("cannot run mac tests when GOOS = %q", runtime.GOOS)
				break
			}
			// Initialize the fuchsia SSH tester to run the snapshot at the end in case
			// we ran any host-target interaction tests.
			if fuchsiaTester == nil && sshKeyFile != "" {
				var err error
				fuchsiaTester, err = newFuchsiaSSHTester(ctx, addr, sshKeyFile, outputs.outDir, serialSocketPath, useRuntests, perTestTimeout)
				if err != nil {
					logger.Errorf(ctx, "failed to initialize fuchsia tester: %s", err)
				}
			}
			if localTester == nil {
				localEnv := append(os.Environ(),
					// Tell tests written in Rust to print stack on failures.
					"RUST_BACKTRACE=1",
				)
				localTester = newSubprocessTester(localWD, localEnv, outputs.outDir, perTestTimeout)
			}
			t = localTester
			sinks = &localSinks
		default:
			finalError = fmt.Errorf("test %#v has unsupported OS: %q", test, test.OS)
			break
		}

		results, err := runAndOutputTest(ctx, test, t, outputs, os.Stdout, os.Stderr, outDir)
		if err != nil {
			finalError = err
			break
		}
		for _, result := range results {
			*sinks = append(*sinks, result.DataSinks)
		}
	}

	if fuchsiaTester != nil {
		defer fuchsiaTester.Close()
	}
	if localTester != nil {
		defer localTester.Close()
	}
	finalize := func(t tester, sinks []runtests.DataSinkReference) error {
		if t != nil {
			if err := t.RunSnapshot(ctx, snapshotFile); err != nil {
				// This error usually has a different root cause that gets masked when we
				// return this error. Log it so we can keep track of it, but don't fail.
				logger.Errorf(ctx, err.Error())
			}
			if err := t.EnsureSinks(ctx, sinks, outputs); err != nil {
				return err
			}
		}
		return nil
	}

	if err := finalize(localTester, localSinks); err != nil && finalError == nil {
		finalError = err
	}
	if err := finalize(fuchsiaTester, fuchsiaSinks); err != nil && finalError == nil {
		finalError = err
	}
	return finalError
}

// stdioBuffer is a simple thread-safe wrapper around bytes.Buffer. It
// implements the io.Writer interface.
type stdioBuffer struct {
	// Used to protect access to `buf`.
	mu sync.Mutex

	// The underlying buffer.
	buf bytes.Buffer
}

func (b *stdioBuffer) Write(p []byte) (n int, err error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.buf.Write(p)
}

func runAndOutputTest(ctx context.Context, test testsharder.Test, t tester, outputs *testOutputs, collectiveStdout, collectiveStderr io.Writer, outDir string) ([]*testrunner.TestResult, error) {
	var results []*testrunner.TestResult

	var stopRepeatingTime time.Time
	if test.StopRepeatingAfterSecs > 0 {
		timeLimit := time.Second * time.Duration(test.StopRepeatingAfterSecs)
		stopRepeatingTime = clock.Now(ctx).Add(timeLimit)
	}

	for i := 0; i < test.Runs; i++ {
		outDir := filepath.Join(outDir, url.PathEscape(strings.ReplaceAll(test.Name, ":", "")), strconv.Itoa(i))
		result, err := runTestOnce(ctx, test, t, collectiveStdout, collectiveStderr, outDir)
		if err != nil {
			return nil, err
		}
		result.RunIndex = i
		if err := outputs.record(*result); err != nil {
			return nil, err
		}
		results = append(results, result)

		// Stop rerunning the test if we've exceeded the time limit for reruns,
		// or if the most recent test run met one of the stop conditions.
		if (!stopRepeatingTime.IsZero() && !clock.Now(ctx).Before(stopRepeatingTime)) ||
			(test.RunAlgorithm == testsharder.StopOnSuccess && result.Result == runtests.TestSuccess) ||
			(test.RunAlgorithm == testsharder.StopOnFailure && result.Result == runtests.TestFailure) {
			break
		}
	}
	return results, nil
}

// runTestOnce runs the given test once. It will not return an error if the test
// fails, only if an unrecoverable error occurs or testing should otherwise stop.
func runTestOnce(
	ctx context.Context,
	test testsharder.Test,
	t tester,
	collectiveStdout io.Writer,
	collectiveStderr io.Writer,
	outDir string,
) (*testrunner.TestResult, error) {
	// The test case parser specifically uses stdout, so we need to have a
	// dedicated stdout buffer.
	stdout := new(bytes.Buffer)
	stdio := new(stdioBuffer)

	multistdout := io.MultiWriter(collectiveStdout, stdio, stdout)
	multistderr := io.MultiWriter(collectiveStderr, stdio)

	// In the case of running tests on QEMU over serial, we do not wish to
	// forward test output to stdout, as QEMU is already redirecting serial
	// output there: we do not want to double-print.
	//
	// This is a bit of a hack, but is a lesser evil than extending the
	// testrunner CLI just to sidecar the information of 'is QEMU'.
	againstQEMU := os.Getenv(constants.NodenameEnvKey) == target.DefaultQEMUNodename
	if _, ok := t.(*fuchsiaSerialTester); ok && againstQEMU {
		multistdout = io.MultiWriter(stdio, stdout)
	}

	result := runtests.TestSuccess
	startTime := clock.Now(ctx)
	dataSinks, err := t.Test(ctx, test, multistdout, multistderr, outDir)
	if err != nil {
		if ctx.Err() != nil {
			// testrunner is shutting down, give up running tests and don't
			// report this test result as it may have been impacted by the
			// context cancelation.
			return nil, err
		}
		var errFatal fatalError
		if errors.As(err, &errFatal) {
			// The tester encountered a fatal condition and cannot run any more
			// tests.
			return nil, err
		}
		result = runtests.TestFailure
		var timeoutErr *timeoutError
		if errors.As(err, &timeoutErr) {
			// TODO(fxbug.dev/49266): Emit a different "Timeout" result if the
			// test timed out.
			logger.Errorf(ctx, "Test %s timed out after %s", test.Name, timeoutErr.timeout)
		} else {
			logger.Errorf(ctx, "Error running test %s: %s", test.Name, err)
		}
	}

	endTime := clock.Now(ctx)

	// Record the test details in the summary.
	return &testrunner.TestResult{
		Name:      test.Name,
		GNLabel:   test.Label,
		Stdio:     stdio.buf.Bytes(),
		Result:    result,
		Cases:     testparser.Parse(stdout.Bytes()),
		StartTime: startTime,
		EndTime:   endTime,
		DataSinks: dataSinks,
	}, nil
}
