// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"fmt"
	"path/filepath"
	"testing"
	"time"

	resultpb "go.chromium.org/luci/resultdb/proto/v1"

	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

func TestParseSummary(t *testing.T) {
	const testCount = 10
	summary := createTestSummary(testCount)
	testResults, _ := SummaryToResultSink(summary, []*resultpb.StringPair{}, "")
	if len(testResults) != testCount {
		t.Errorf(
			"Parsed incorrect number of resultdb tests in TestSummary, got %d, want %d",
			len(testResults), testCount)
	}
	requests := createTestResultsRequests(testResults, testCount)
	if len(requests) != 1 {
		t.Errorf(
			"Grouped incorrect chunks of ResultDB sink requests, got %d, want 1",
			len(requests))
	}
	if len(requests[0].TestResults) != testCount {
		t.Errorf(
			"Incorrect number of TestResult in the first chunk, got %d, want %d",
			len(requests[0].TestResults), testCount)
	}
	if requests[0].TestResults[0].TestId != "test_0" {
		t.Errorf("Incorrect TestId parsed for first suite. got %s, want test_0", requests[0].TestResults[0].TestId)
	}
}

func TestSetTestDetailsToResultSink(t *testing.T) {
	detail := createTestDetailWithTestCase(5)
	extraTags := []*resultpb.StringPair{
		{Key: "key1", Value: "value1"},
	}
	result, _, err := testDetailsToResultSink(extraTags, detail, "")
	if err != nil {
		t.Fatalf("Cannot parse test detail. got %s", err)
	}

	tags := make(map[string]string)
	for _, tag := range result.Tags {
		tags[tag.Key] = tag.Value
	}

	if len(extraTags) != 1 {
		t.Errorf("extraTags(%v) got mutated, this value should not be changed.", extraTags)
	}
	// We only expect 3 tags
	// 1. gn_label:value
	// 2. test_case_count:value
	// 3. key1:value1
	if len(tags) != 3 {
		t.Errorf("tags(%v) contains unexpected values.", tags)
	}
	if extra, ok := tags["key1"]; !ok {
		t.Error("Did not find key1 in tags")
	} else if extra != "value1" {
		t.Errorf("Found incorrect key value tag, got %s, want value1", extra)
	}

	if testCaseCount, ok := tags["test_case_count"]; !ok {
		t.Error("Did not find test_case_count in tags")
	} else if testCaseCount != "5" {
		t.Errorf("Found incorrect test_case_count tag, got %s, want 5", testCaseCount)
	}

	if gnLabel, ok := tags["gn_label"]; !ok {
		t.Error("Did not find gn_label in tags")
	} else if gnLabel != detail.GNLabel {
		t.Errorf("Found incorrect gn_label tab, got %s, want %s", gnLabel, detail.GNLabel)
	}
}

func createTestSummary(testCount int) *runtests.TestSummary {
	t := []runtests.TestDetails{}
	for i := 0; i < testCount; i++ {
		t = append(t, runtests.TestDetails{
			Name:                 fmt.Sprintf("test_%d", i),
			GNLabel:              "some label",
			OutputFiles:          []string{"some file path"},
			Result:               runtests.TestSuccess,
			StartTime:            time.Now(),
			DurationMillis:       39797,
			IsTestingFailureMode: false,
		})
	}
	return &runtests.TestSummary{Tests: t}
}

func createTestDetailWithTestCase(testCase int) *runtests.TestDetails {
	t := []runtests.TestCaseResult{}
	for i := 0; i < testCase; i++ {
		t = append(t, runtests.TestCaseResult{
			DisplayName: fmt.Sprintf("foo/bar_%d", i),
			SuiteName:   "foo",
			CaseName:    fmt.Sprintf("bar_%d", i),
			Status:      runtests.TestSuccess,
			Format:      "Rust",
		})
	}
	return &runtests.TestDetails{
		Name:                 "foo",
		GNLabel:              "some label",
		OutputFiles:          []string{"some file path"},
		Result:               runtests.TestSuccess,
		StartTime:            time.Now(),
		DurationMillis:       39797,
		IsTestingFailureMode: false,
		Cases:                t,
	}
}

func TestIsReadable(t *testing.T) {
	if r := isReadable(""); r {
		t.Errorf("Empty string cannot be readable. got %t, want false", r)
	}
	if r := isReadable(*testDataDir); r {
		t.Errorf("Directory should not be readable. got %t, want false", r)
	}
	luciCtx := filepath.Join(*testDataDir, "lucictx.json")
	if r := isReadable(luciCtx); !r {
		t.Errorf("File %v should be readable. got %t, want true", luciCtx, r)
	}
}

func TestInvocationLevelArtifacts(t *testing.T) {
	artifacts := invocationLevelArtifacts(*testDataDir)
	foundSyslog := false
	foundSerial := false
	for logName := range artifacts {
		switch logName {
		case "syslog.txt":
			foundSyslog = true
		case "serial_log.txt":
			foundSerial = true
		default:
			t.Errorf("Found unexpected log (%s), expect only syslog.txt or serial_log.txt", logName)
		}
	}
	if !foundSyslog {
		t.Errorf("Did not find syslog.txt in output")
	}
	if !foundSerial {
		t.Errorf("Did not find serial_log.txt in output")
	}
}

func TestDetermineExpected(t *testing.T) {
	testCases := []struct {
		testStatus     resultpb.TestStatus
		testCaseStatus resultpb.TestStatus
		expected       bool
	}{
		{
			// test passed, test case result is ignored.
			testStatus:     resultpb.TestStatus_PASS,
			testCaseStatus: resultpb.TestStatus_FAIL,
			expected:       true,
		},
		{
			// test failed and has test case status,
			// report on test case result.
			testStatus:     resultpb.TestStatus_FAIL,
			testCaseStatus: resultpb.TestStatus_PASS,
			expected:       true,
		},
		{
			// test failed and no test case status,
			// report test result.
			testStatus:     resultpb.TestStatus_FAIL,
			testCaseStatus: resultpb.TestStatus_STATUS_UNSPECIFIED,
			expected:       false,
		},
		{
			// cannot determine test status,
			// report on test cast result.
			testStatus:     resultpb.TestStatus_STATUS_UNSPECIFIED,
			testCaseStatus: resultpb.TestStatus_PASS,
			expected:       true,
		},
		{
			// cannot determine both test and test case result
			testStatus:     resultpb.TestStatus_STATUS_UNSPECIFIED,
			testCaseStatus: resultpb.TestStatus_STATUS_UNSPECIFIED,
			expected:       false,
		},
		{
			testStatus:     resultpb.TestStatus_PASS,
			testCaseStatus: resultpb.TestStatus_PASS,
			expected:       true,
		},
		{
			testStatus:     resultpb.TestStatus_FAIL,
			testCaseStatus: resultpb.TestStatus_FAIL,
			expected:       false,
		},
	}
	for _, tc := range testCases {
		r := determineExpected(tc.testStatus, tc.testCaseStatus)
		if r != tc.expected {
			t.Errorf("TestDetermineExpected failed:\ntestSuite Status: %v, testCase Status: %v, got %t, want %t",
				tc.testStatus, tc.testCaseStatus, r, tc.expected)
		}
	}
}

func TestTruncateString(t *testing.T) {
	testCases := []struct {
		testStr string
		want    string
		limit   int // bytes
	}{
		{
			testStr: "ab£cdefg",
			want:    "",
			limit:   1,
		}, {
			testStr: "ab£cdefg",
			want:    "ab...",
			limit:   5,
		}, {
			testStr: "ab£cdefg",
			want:    "ab...",
			limit:   6,
		}, {
			testStr: "ab£cdefg",
			want:    "ab£...",
			limit:   7,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "",
			limit:   3,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "",
			limit:   4,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "",
			limit:   5,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥...",
			limit:   6,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥L...",
			limit:   7,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥LoveFuc...",
			limit:   13,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥LoveFuchsia",
			limit:   14,
		}, {
			testStr: "♥LoveFuchsia",
			want:    "♥LoveFuchsia",
			limit:   100,
		},
	}
	for _, tc := range testCases {
		r := truncateString(tc.testStr, tc.limit)
		if r != tc.want {
			t.Errorf("TestTruncateString failed for input: %q(%d), got %q, want %q",
				tc.testStr, tc.limit, r, tc.want)
		}
	}
}
