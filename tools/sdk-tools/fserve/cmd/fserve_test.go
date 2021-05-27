// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"log"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"runtime"
	"strings"
	"syscall"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/lib/color"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/sdk-tools/sdkcommon"
)

// test argument pointing to the directory containing the testdata directory.
// This is configured in the BUILD.gn file.
var testrootFlag = flag.String("testroot", "", "Root directory of the files needed to execute the test.")

const hostaddr = "fe80::c0ff:eeee:fefe:c000%eth1"

const validDir = "/some/package/repo/dir"

type testSDKProperties struct {
	dataPath              string
	expectCustomSSHConfig bool
	expectPrivateKey      bool
	expectSSHPort         bool
	expectedSSHArgs       [][]string
}

func (testSDK testSDKProperties) GetToolsDir() (string, error) {
	return "fake-tools", nil
}

func (testSDK testSDKProperties) GetSDKDataPath() string {
	return testSDK.dataPath
}

func (testSDK testSDKProperties) GetAvailableImages(version string, bucket string) ([]sdkcommon.GCSImage, error) {
	return []sdkcommon.GCSImage{}, nil
}
func (testSDK testSDKProperties) GetAddressByName(deviceName string) (string, error) {
	return "::1", nil
}
func (testSDK testSDKProperties) GetDefaultPackageRepoDir() (string, error) {
	return filepath.Join(testSDK.dataPath, "default-target-name", "packages", "amber-files"), nil
}
func (testSDK testSDKProperties) RunSSHCommand(targetAddress string, sshConfig string,
	privateKey string, sshPort string, verbose bool, sshArgs []string) (string, error) {

	if testSDK.expectCustomSSHConfig && sshConfig == "" {
		return "", errors.New("Expected custom ssh config file")
	}
	if testSDK.expectPrivateKey && privateKey == "" {
		return "", errors.New("Expected private key file")
	}

	if testSDK.expectSSHPort && sshPort == "" {
		return "", errors.New("Expected custom ssh port")
	}
	expectedArgs := []string{}

	for _, args := range testSDK.expectedSSHArgs {
		if sshArgs[0] == args[0] {
			expectedArgs = args
			break
		}
	}

	ok := len(expectedArgs) == len(sshArgs)
	if ok {
		for i, expected := range expectedArgs {
			if !ok {
				return "", fmt.Errorf("unexpected ssh args[%v]  %v expected[%v] %v",
					len(sshArgs), sshArgs, len(expectedArgs), expectedArgs)
			}
			if strings.Contains(expected, "*") {
				expectedPattern := regexp.MustCompile(expected)
				ok = expectedPattern.MatchString(sshArgs[i])
			} else {
				ok = expected == sshArgs[i]
			}
		}
	}
	if sshArgs[0] == "echo" {
		return fmt.Sprintf("%v 54545 fe80::c00f:f0f0:eeee:cccc 22\n", hostaddr), nil
	}
	return "", nil
}

// Context with a logger used to test.
func testingContext() context.Context {
	flags := log.Ltime | log.Lshortfile
	log := logger.NewLogger(logger.DebugLevel, color.NewColor(color.ColorAuto), os.Stdout, os.Stderr, "fserve_test ")
	log.SetFlags(flags)
	return logger.WithLogger(context.Background(), log)
}

// See exec_test.go for details, but effectively this runs the function called TestHelperProcess passing
// the args.
func helperCommandForFServe(command string, s ...string) (cmd *exec.Cmd) {
	cs := []string{"-test.run=TestFakeFServe", "--"}
	cs = append(cs, command)
	cs = append(cs, s...)

	cmd = exec.Command(os.Args[0], cs...)
	// Set this in the enviroment, so we can control the result.
	cmd.Env = append(os.Environ(), "GO_WANT_HELPER_PROCESS=1")
	return cmd
}

func TestCleanPmRepo(t *testing.T) {
	sdkcommon.ExecCommand = helperCommandForFServe
	ExecCommand = helperCommandForFServe
	ctx := testingContext()
	defer func() {
		ExecCommand = exec.Command
	}()

	var tests = []struct {
		path          string
		expectedError string
		doesPathExist bool
	}{
		{
			path:          validDir,
			doesPathExist: true,
		},
		{
			path:          "/some/dir/rm/error",
			expectedError: "exit status 1",
			doesPathExist: true,
		}, {
			path:          "/dir/does/not/exist",
			doesPathExist: false,
		},
	}

	for _, test := range tests {
		OsStat = func(name string) (fi os.FileInfo, err error) {
			var cmd os.FileInfo
			if test.doesPathExist {
				return cmd, nil
			}
			return nil, os.ErrNotExist
		}
		defer func() {
			OsStat = os.Stat
		}()

		err := cleanPmRepo(ctx, test.path)
		if err != nil && err.Error() != test.expectedError {
			t.Errorf("Expected error '%v', but got '%v'", test.expectedError, err)
		}
	}
}

func TestKillServers(t *testing.T) {
	ctx := testingContext()
	ExecCommand = helperCommandForFServe
	findProcess = mockedFindProcess
	defer func() {
		ExecCommand = exec.Command
		findProcess = defaultFindProcess
	}()

	// Test no existing servers
	os.Setenv("FSERVE_TEST_NO_SERVERS", "1")
	if err := killServers(ctx, ""); err != nil {
		t.Fatal(err)
	}
	// Test existing servers
	os.Setenv("FSERVE_TEST_NO_SERVERS", "0")
	if err := killServers(ctx, ""); err != nil {
		t.Fatal(err)
	}
	if err := killServers(ctx, "8083"); err != nil {
		t.Fatal(err)
	}
	os.Setenv("FSERVE_TEST_PGREP_ERROR", "1")
	err := killServers(ctx, "")
	if err == nil {
		t.Fatal("Expected error running pgrep, got no error.")
	}
	expected := "Error running pgrep: Expected error\n"
	actual := fmt.Sprintf("%v", err)
	if expected != actual {
		t.Fatalf("[%v], got [%v]", expected, actual)
	}

	os.Setenv("FSERVE_TEST_PGREP_ERROR", "0")
	os.Setenv("FSERVE_TEST_PS_ERROR", "1")
	err = killServers(ctx, "")
	if err == nil {
		t.Fatal("Expected error running ps, got no error.")
	}
	expected = "Error running ps: Expected error\n"
	actual = fmt.Sprintf("%v", err)
	if expected != actual {
		t.Fatalf("[%v], got [%v]", expected, actual)
	}
}

func TestStartServer(t *testing.T) {
	testSDK := testSDKProperties{
		dataPath: "/fake",
	}
	repoPath := "/fake/repo/path"
	repoPort := "8083"
	ExecCommand = helperCommandForFServe
	defer func() {
		ExecCommand = exec.Command
		syscallWait4 = defaultsyscallWait4
	}()

	tests := []struct {
		syscallWait4  func(pid int, wstatus *syscall.WaitStatus, flags int, usage *syscall.Rusage) (int, error)
		expectedError string
		logLevel      logger.LogLevel
		expectedArgs  []string
	}{

		{syscallWait4: mockWait4NoError,
			expectedError: "",
			logLevel:      logger.WarningLevel,
			expectedArgs:  []string{"serve", "-q", "-repo", "/fake/repo/path", "-l", ":8083"},
		},

		{syscallWait4: mockWait4NoError,
			expectedError: "",
			logLevel:      logger.DebugLevel,
			expectedArgs:  []string{"serve", "-repo", "/fake/repo/path", "-l", ":8083"},
		},

		{syscallWait4: mockWait4WithError,
			expectedError: "Server started then exited with code 1",
			logLevel:      logger.WarningLevel,
		},
	}

	for i, test := range tests {
		t.Run(fmt.Sprintf("TestStartServer case %d", i), func(t *testing.T) {
			syscallWait4 = test.syscallWait4
			level = test.logLevel
			os.Setenv("TEST_LOGLEVEL", level.String())
			cmd, err := startServer(testSDK, repoPath, repoPort)
			if err != nil {
				actual := fmt.Sprintf("%v", err)
				if test.expectedError != actual {
					t.Errorf("Actual error [%v] did not match expected [%v]", actual, test.expectedError)
				}
			} else if test.expectedError != "" {
				t.Errorf("Expected error %v, but got no error", test.expectedError)
			} else {
				actual := cmd.Args[4:]
				ok := len(actual) == len(test.expectedArgs)
				if ok {
					for i, arg := range test.expectedArgs {
						if arg != actual[i] {
							ok = false
							break
						}
					}
				}
				if !ok {
					t.Errorf("pm args %v do not match expected %v", actual, test.expectedArgs)
				}
			}
		})
		syscallWait4 = defaultsyscallWait4
	}
}

func TestDownloadImageIfNeeded(t *testing.T) {
	testSDK := testSDKProperties{
		dataPath: t.TempDir(),
	}
	ctx := testingContext()
	ExecCommand = helperCommandForFServe
	sdkcommon.ExecCommand = helperCommandForFServe
	sdkcommon.ExecLookPath = func(cmd string) (string, error) { return filepath.Join("mocked", cmd), nil }
	defer func() {
		ExecCommand = exec.Command
		sdkcommon.ExecCommand = exec.Command
		sdkcommon.ExecLookPath = exec.LookPath
	}()
	version := "any-version"
	bucket := "test-bucket"
	srcPath := "gs://test-bucket/path/on/GCS/theImage.tgz"
	imageFilename := "theImage.tgz"
	repoPath, err := testSDK.GetDefaultPackageRepoDir()
	if err != nil {
		t.Fatal(err)
	}

	executable, _ := os.Executable()
	fmt.Fprintf(os.Stderr, "Running test executable %v\n", executable)
	fmt.Fprintf(os.Stderr, "testrootFlag value   is %v\n", *testrootFlag)
	testrootPath := filepath.Join(filepath.Dir(executable), *testrootFlag)
	os.Setenv("FSERVE_TEST_TESTROOT", testrootPath)

	if err := downloadImageIfNeeded(ctx, testSDK, version, bucket, srcPath, imageFilename, repoPath); err != nil {
		t.Fatal(err)
	}
	// Run the test again, and it should skip the download
	os.Setenv("FSERVE_TEST_ASSERT_NO_DOWNLOAD", "1")
	if err := downloadImageIfNeeded(ctx, testSDK, version, bucket, srcPath, imageFilename, repoPath); err != nil {
		t.Fatal(err)
	}
}

func TestDownloadImageIfNeededCopiedFails(t *testing.T) {
	testSDK := testSDKProperties{
		dataPath: "/fake",
	}
	ctx := testingContext()
	ExecCommand = helperCommandForFServe
	sdkcommon.ExecCommand = helperCommandForFServe
	sdkcommon.ExecLookPath = func(cmd string) (string, error) { return filepath.Join("mocked", cmd), nil }
	defer func() {
		ExecCommand = exec.Command
		sdkcommon.ExecCommand = exec.Command
		sdkcommon.ExecLookPath = exec.LookPath
	}()
	version := "any-version"
	bucket := "test-bucket"
	srcPath := "gs://test-bucket/path/on/GCS/theImage.tgz"
	imageFilename := "theImage.tgz"
	repoPath, err := testSDK.GetDefaultPackageRepoDir()
	if err != nil {
		t.Fatal(err)
	}

	// Run the test again, and it should skip the download
	os.Setenv("FSERVE_TEST_ASSERT_NO_DOWNLOAD", "")
	os.Setenv("FSERVE_TEST_COPY_FAILS", "1")
	if err := downloadImageIfNeeded(ctx, testSDK, version, bucket, srcPath, imageFilename, repoPath); err != nil {
		destPath := filepath.Join(testSDK.GetSDKDataPath(), imageFilename)
		expected := fmt.Sprintf("Could not copy image from %v to %v: BucketNotFoundException: 404 %v bucket does not exist.: exit status 2",
			srcPath, destPath, srcPath)
		actual := fmt.Sprintf("%v", err)
		if expected != actual {
			t.Fatalf("[%v], got [%v]", expected, actual)
		}
	} else {
		t.Fatal("Expected error downloading, got no error.")
	}
}

const resolvedAddr = "fe80::c0ff:eee:fe00:4444%en0"

func TestSetPackageSource(t *testing.T) {
	testSDK := testSDKProperties{
		dataPath: t.TempDir(),
	}
	homeDir := filepath.Join(testSDK.GetSDKDataPath(), "_TEMP_HOME")
	if err := os.MkdirAll(homeDir, 0o700); err != nil {
		t.Fatal(err)
	}
	ctx := testingContext()
	ExecCommand = helperCommandForFServe
	sdkcommon.ExecCommand = helperCommandForFServe
	sdkcommon.GetUserHomeDir = func() (string, error) { return homeDir, nil }
	sdkcommon.GetUsername = func() (string, error) { return "testuser", nil }
	sdkcommon.GetHostname = func() (string, error) { return "testhost", nil }
	defer func() {
		ExecCommand = exec.Command
		sdkcommon.ExecCommand = exec.Command
		sdkcommon.GetUserHomeDir = sdkcommon.DefaultGetUserHomeDir
		sdkcommon.GetUsername = sdkcommon.DefaultGetUsername
		sdkcommon.GetHostname = sdkcommon.DefaultGetHostname
	}()

	tests := []struct {
		repoPort        string
		targetAddress   string
		sshConfig       string
		name            string
		privateKey      string
		sshPort         string
		expectedSSHArgs [][]string
	}{
		{
			repoPort:      "8083",
			targetAddress: resolvedAddr,
			sshConfig:     "",
			privateKey:    "",
			name:          "devhost",
			expectedSSHArgs: [][]string{
				{"echo", "$SSH_CONNECTION"},
				{"amber_ctl", "add_src", "-n", "devhost", "-f", "http://[fe80::c0ff:eeee:fefe:c000%25eth1]:8083/config.json"},
			},
		},
		{
			repoPort:      "8083",
			targetAddress: resolvedAddr,
			sshConfig:     "custom-sshconfig",
			privateKey:    "",
			name:          "devhost",
			expectedSSHArgs: [][]string{
				{"echo", "$SSH_CONNECTION"},
				{"amber_ctl", "add_src", "-n", "devhost", "-f", "http://[fe80::c0ff:eeee:fefe:c000%25eth1]:8083/config.json"},
			},
		},
		{
			repoPort:      "8083",
			targetAddress: resolvedAddr,
			sshConfig:     "",
			privateKey:    "private-key",
			name:          "devhost",
			expectedSSHArgs: [][]string{
				{"echo", "$SSH_CONNECTION"},
				{"amber_ctl", "add_src", "-n", "devhost", "-f", "http://[fe80::c0ff:eeee:fefe:c000%25eth1]:8083/config.json"},
			},
		},
		{
			repoPort:      "8083",
			targetAddress: resolvedAddr,
			sshConfig:     "",
			privateKey:    "",
			name:          "devhost",
			sshPort:       "1022",
			expectedSSHArgs: [][]string{
				{"echo", "$SSH_CONNECTION"},
				{"amber_ctl", "add_src", "-n", "devhost", "-f", "http://[fe80::c0ff:eeee:fefe:c000%25eth1]:8083/config.json"},
			},
		},
	}

	for _, test := range tests {
		testSDK := testSDKProperties{expectedSSHArgs: test.expectedSSHArgs,
			expectCustomSSHConfig: test.sshConfig != "",
			expectPrivateKey:      test.privateKey != "",
			expectSSHPort:         test.sshPort != ""}

		if err := setPackageSource(ctx, testSDK, test.repoPort, test.name, test.targetAddress, test.sshConfig, test.privateKey, test.sshPort); err != nil {
			t.Fatal(err)
		}
	}
}

/*
This "test" is used to mock the command line tools invoked by fserve.
The method "helperCommandForFServe" replaces exec.Command and runs
this test inplace of the command.

This approach to mocking out executables is  based on  exec_test.go.

*/
func TestFakeFServe(t *testing.T) {
	t.Helper()
	if os.Getenv("GO_WANT_HELPER_PROCESS") != "1" {
		return
	}
	defer os.Exit(0)

	args := os.Args
	for len(args) > 0 {
		if args[0] == "--" {
			args = args[1:]
			break
		}
		args = args[1:]
	}
	if len(args) == 0 {
		fmt.Fprintf(os.Stderr, "No command\n")
		os.Exit(2)
	}
	// Check the command line
	cmd, args := args[0], args[1:]
	switch filepath.Base(cmd) {
	case "pgrep":
		fakePgrep(args)
	case "ps":
		fakePS(args)
	case "rm":
		fakeRmRf(args)
	case "pm":
		fakePM(args)
	case "gsutil":
		fakeGSUtil(args)
	default:
		fmt.Fprintf(os.Stderr, "Unexpected command %v", cmd)
		os.Exit(1)
	}
}

func fakeGSUtil(args []string) {
	expected := []string{}
	expectedLS := []string{"ls", "gs://test-bucket/path/on/GCS/theImage.tgz"}
	expectedCP := []string{"cp", "gs://test-bucket/path/on/GCS/theImage.tgz", "/.*/theImage.tgz"}

	if len(args) == 0 {
		fmt.Fprintf(os.Stderr, "Expected arguments to gsutil\n")
		os.Exit(1)
	}
	switch args[0] {
	case "ls":
		expected = expectedLS
	case "cp":
		if os.Getenv("FSERVE_TEST_ASSERT_NO_DOWNLOAD") != "" {
			fmt.Fprintf(os.Stderr, "Unexpected call to gsutil cp: %v\n", args)
			os.Exit(1)
		}
		if os.Getenv("FSERVE_TEST_COPY_FAILS") != "" {
			fmt.Fprintf(os.Stderr, "BucketNotFoundException: 404 %v bucket does not exist.", args[1])
			os.Exit(2)
		}
		expected = expectedCP
		// Copy the test data to the expected path.
		testRoot := os.Getenv("FSERVE_TEST_TESTROOT")
		testdata := filepath.Join(testRoot, "testdata", "testdata.tgz")
		if !sdkcommon.FileExists(testdata) {
			testdata = filepath.Join("..", "testdata", "testdata.tgz")
		}
		if err := os.MkdirAll(filepath.Dir(args[2]), 0755); err != nil {
			fmt.Fprintf(os.Stderr, "Error getting mkdir temp dir: %v\n", err)
			os.Exit(1)
		}
		if err := copyFile(testdata, args[2]); err != nil {
			fmt.Fprintf(os.Stderr, "Error linking testdata: %v\n", err)
			os.Exit(1)
		}

	}

	ok := len(args) == len(expected)
	if ok {
		for i := range args {
			if strings.Contains(expected[i], "*") {
				expectedPattern := regexp.MustCompile(expected[i])
				ok = ok && expectedPattern.MatchString(args[i])
			} else {
				ok = ok && args[i] == expected[i]
			}
		}
	}
	if !ok {
		fmt.Fprintf(os.Stderr, "unexpected gsutil args  %v. Expected %v", args, expected)
		os.Exit(1)
	}
}

func fakePM(args []string) {
	expected := []string{"serve"}
	logLevel := os.Getenv("TEST_LOGLEVEL")
	// only debug and trace have non-quiet mode.
	if logLevel != "debug" && logLevel != "trace" {
		expected = append(expected, "-q")
	}
	expected = append(expected, "-repo", "/fake/repo/path", "-l", ":8083")
	ok := len(args) == len(expected)
	if ok {
		for i := range args {
			ok = ok && args[i] == expected[i]
		}
	}
	if !ok {
		fmt.Fprintf(os.Stderr, "unexpected pm args  %v. Expected %v\n", args, expected)
		os.Exit(1)
	}
}

func fakePgrep(args []string) {
	if os.Getenv("FSERVE_TEST_PGREP_ERROR") == "1" {
		fmt.Fprintf(os.Stderr, "Expected error\n")
		os.Exit(1)
	}
	if args[0] == "pm" {
		if os.Getenv("FSERVE_TEST_NO_SERVERS") == "1" {
			// mac exits with 1
			if runtime.GOOS == "darwin" {
				os.Exit(1)
			}
			os.Exit(0)
		} else {
			// return 3 pm instances
			fmt.Printf(`1000
		2000
		3000`)
			os.Exit(0)
		}
	}
	fmt.Fprintf(os.Stderr, "unexpected pgrep args  %v", args)
	os.Exit(1)
}

func fakePS(args []string) {
	if os.Getenv("FSERVE_TEST_PS_ERROR") == "1" {
		fmt.Fprintf(os.Stderr, "Expected error\n")
		os.Exit(1)
	}
	fmt.Println("    PID TTY      STAT   TIME COMMAND")
	for _, arg := range args {
		switch arg {
		case "1000":
			// some internal process
			fmt.Println("1000 ?       I<     0:00  [tpm_dev_wq]")
		case "2000":
			// pm on port 8083
			fmt.Println("2000  pts/0    Sl     0:00 /sdk/path/tools/x64/pm serve -repo /home/developer/.fuchsia/packages/amber-files -l :8083")
		case "3000":
			// pm on port 8084
			fmt.Println("3000  pts/0    Sl     0:00 /sdk/path/tools/x64/pm serve -repo /home/developer/.fuchsia/packages/amber-files -l :8084 -q")
		}
	}
}

func fakeRmRf(args []string) {
	command := strings.Join(args, " ")
	if command == fmt.Sprintf("-Rf %v", validDir) {
		os.Exit(0)
	}
	fmt.Fprintf(os.Stderr, "Directory does not exist\n")
	os.Exit(1)
}

type testProcess struct {
	Pid int
}

func (proc testProcess) Kill() error {
	// This mock only kills pid == 2000
	if proc.Pid != 2000 {
		return fmt.Errorf("Unexpected pid %v in Kill", proc.Pid)
	}
	return nil
}

func mockedFindProcess(pid int) (osProcess, error) {
	proc := testProcess{Pid: pid}

	return &proc, nil
}

func mockWait4NoError(pid int, wstatus *syscall.WaitStatus, flags int, usage *syscall.Rusage) (int, error) {
	return 0, nil
}

func mockWait4WithError(pid int, wstatus *syscall.WaitStatus, flags int, usage *syscall.Rusage) (int, error) {
	// set wstatus to exited with code 1.
	*wstatus = syscall.WaitStatus(0x100)
	return 0, nil
}

func copyFile(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()

	out, err := os.Create(dst)
	if err != nil {
		return err
	}
	defer out.Close()

	_, err = io.Copy(out, in)
	if err != nil {
		return err
	}
	return out.Close()
}
