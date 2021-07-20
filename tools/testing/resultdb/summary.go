// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"encoding/json"
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"path/filepath"
	"strconv"
	"time"

	resultpb "go.chromium.org/luci/resultdb/proto/v1"
	sinkpb "go.chromium.org/luci/resultdb/sink/proto/v1"
	"google.golang.org/protobuf/types/known/durationpb"
	"google.golang.org/protobuf/types/known/timestamppb"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
	"go.fuchsia.dev/fuchsia/tools/testing/testparser"
)

// ParseSummary unmarshalls the summary.json file content into runtests.TestSummary struct.
func ParseSummary(filePath string) (*runtests.TestSummary, error) {
	content, err := ioutil.ReadFile(filePath)
	if err != nil {
		return nil, err
	}
	var summary runtests.TestSummary
	if err := json.Unmarshal(content, &summary); err != nil {
		return nil, err
	}
	return &summary, nil
}

// SummaryToResultSink converts runtests.TestSummary data into an array of result_sink TestResult.
func SummaryToResultSink(s *runtests.TestSummary, tags []*resultpb.StringPair, outputRoot string) []*sinkpb.TestResult {
	if len(outputRoot) == 0 {
		outputRoot, _ = os.Getwd()
	}
	rootPath, _ := filepath.Abs(outputRoot)
	var r []*sinkpb.TestResult
	for _, test := range s.Tests {
		if len(test.Cases) > 0 {
			testCases := testCaseToResultSink(test.Cases, tags, &test)
			r = append(r, testCases...)
		}
		if testResult, err := testDetailsToResultSink(tags, &test, rootPath); err == nil {
			r = append(r, testResult)
		}
	}
	return r
}

// invocationLevelArtifacts creates resultdb artifacts for syslog and serial log to be sent to ResultDB.
func invocationLevelArtifacts(outputRoot string) map[string]*sinkpb.Artifact {
	if len(outputRoot) == 0 {
		outputRoot, _ = os.Getwd()
	}
	rootPath, _ := filepath.Abs(outputRoot)
	artifacts := map[string]*sinkpb.Artifact{}

	// TODO(yuanzhi) Make this an argument that the recipe can pass to the resultdb uploader
	// instead of hardcoding here.
	for _, invocationLog := range [...]string{
		"infra_and_test_std_and_klog.txt",
		"serial_log.txt",
		"syslog.txt",
		"triage_output",
	} {
		logFile := filepath.Join(rootPath, invocationLog)
		if isReadable(logFile) {
			artifacts[invocationLog] = &sinkpb.Artifact{
				Body:        &sinkpb.Artifact_FilePath{FilePath: logFile},
				ContentType: "text/plain",
			}
		}
	}
	return artifacts
}

// testCaseToResultSink converts TestCaseResult defined in //tools/testing/testparser/result.go
// to ResultSink's TestResult. A testcase will not be converted if test result cannot be
// mapped to result_sink.Status.
func testCaseToResultSink(testCases []testparser.TestCaseResult, tags []*resultpb.StringPair, testDetail *runtests.TestDetails) []*sinkpb.TestResult {
	var testResult []*sinkpb.TestResult

	// Ignore error, testStatus will be set to resultpb.TestStatus_STATUS_UNSPECIFIED if error != nil.
	// And when passed to determineExpected, resultpb.TestStatus_STATUS_UNSPECIFIED will be handled correctly.
	testStatus, _ := testDetailResultToResultDBStatus(testDetail.Result)

	for _, testCase := range testCases {
		testID := fmt.Sprintf("%s/%s:%s", testDetail.Name, testCase.SuiteName, testCase.CaseName)
		r := sinkpb.TestResult{
			TestId: testID,
			Tags:   append([]*resultpb.StringPair{{Key: "format", Value: testCase.Format}}, tags...),
		}
		testCaseStatus, err := testCaseStatusToResultDBStatus(testCase.Status)
		if err != nil {
			log.Printf("[Warn] Skip uploading testcase: %s to ResultDB due to error: %v", testID, err)
			continue
		}
		r.Status = testCaseStatus
		r.StartTime = timestamppb.New(testDetail.StartTime)
		if testCase.Duration > 0 {
			r.Duration = durationpb.New(testCase.Duration)
		}
		r.Expected = determineExpected(testStatus, testCaseStatus)
		testResult = append(testResult, &r)
	}
	return testResult
}

// testDetailsToResultSink converts TestDetail defined in /tools/testing/runtests/runtests.go
// to ResultSink's TestResult. Returns (nil, error) if a test result cannot be mapped to
// result_sink.Status
func testDetailsToResultSink(tags []*resultpb.StringPair, testDetail *runtests.TestDetails, outputRoot string) (*sinkpb.TestResult, error) {
	r := sinkpb.TestResult{
		TestId: testDetail.Name,
		Tags: append([]*resultpb.StringPair{
			{Key: "gn_label", Value: testDetail.GNLabel},
			{Key: "test_case_count", Value: strconv.Itoa(len(testDetail.Cases))},
		}, tags...),
	}
	testStatus, err := testDetailResultToResultDBStatus(testDetail.Result)
	if err != nil {
		log.Printf("[Warn] Skip uploading testcase: %s to ResultDB due to error: %v", testDetail.Name, err)
		return nil, err
	}
	r.Status = testStatus

	r.StartTime = timestamppb.New(testDetail.StartTime)
	if testDetail.DurationMillis > 0 {
		r.Duration = durationpb.New(time.Duration(testDetail.DurationMillis) * time.Millisecond)
	}
	for _, of := range testDetail.OutputFiles {
		outputFile := filepath.Join(outputRoot, of)
		if isReadable(outputFile) {
			r.Artifacts = map[string]*sinkpb.Artifact{
				filepath.Base(outputFile): {
					Body:        &sinkpb.Artifact_FilePath{FilePath: outputFile},
					ContentType: "text/plain",
				},
			}
		} else {
			log.Printf("[Warn] outputFile: %s is not readable, skip.", outputFile)
		}
	}

	r.SummaryHtml = `<details><summary>triage_output</summary>
	<pre><text-artifact artifact-id="triage_output" inv-level/></pre>
	</details>
	`

	r.Expected = determineExpected(testStatus, resultpb.TestStatus_STATUS_UNSPECIFIED)
	return &r, nil
}

// determineExpected checks if a test result is expected.
//
// For example, if a test case failed but fail is the correct behavior, we will mark
// expected to true. On the other hand, if a test case failed and failure is the incorrect
// behavior then we will mark expected to false. This is completely determined by
// the status recorded by the test suite vs. status recorded for the test case.
//
// If a test is reported "PASS", then we will report all test cases within the same
// test to pass as well. If a test is reported other than "PASS" or "SKIP", we will
// process the test cases based on the test case result.
func determineExpected(testStatus resultpb.TestStatus, testCaseStatus resultpb.TestStatus) bool {
	switch testStatus {
	case resultpb.TestStatus_PASS, resultpb.TestStatus_SKIP:
		return true
	case resultpb.TestStatus_FAIL, resultpb.TestStatus_CRASH, resultpb.TestStatus_ABORT, resultpb.TestStatus_STATUS_UNSPECIFIED:
		switch testCaseStatus {
		case resultpb.TestStatus_PASS, resultpb.TestStatus_SKIP:
			return true
		case resultpb.TestStatus_FAIL, resultpb.TestStatus_CRASH, resultpb.TestStatus_ABORT, resultpb.TestStatus_STATUS_UNSPECIFIED:
			return false
		}
	}
	return false
}

func testCaseStatusToResultDBStatus(result testparser.TestCaseStatus) (resultpb.TestStatus, error) {
	switch result {
	case testparser.Pass:
		return resultpb.TestStatus_PASS, nil
	case testparser.Fail:
		return resultpb.TestStatus_FAIL, nil
	case testparser.Skip:
		return resultpb.TestStatus_SKIP, nil
	}
	return resultpb.TestStatus_STATUS_UNSPECIFIED, fmt.Errorf("cannot map Result: %s to result_sink test_result status", result)
}

func testDetailResultToResultDBStatus(result runtests.TestResult) (resultpb.TestStatus, error) {
	switch result {
	case runtests.TestSuccess:
		return resultpb.TestStatus_PASS, nil
	case runtests.TestFailure:
		return resultpb.TestStatus_FAIL, nil
	}
	return resultpb.TestStatus_STATUS_UNSPECIFIED, fmt.Errorf("cannot map Result: %s to result_sink test_result status", result)
}

func isReadable(p string) bool {
	if len(p) == 0 {
		return false
	}
	info, err := os.Stat(p)
	if err != nil {
		return false
	}
	if info.IsDir() {
		return false
	}
	f, err := os.Open(p)
	if err != nil {
		return false
	}
	_ = f.Close()
	return true
}
