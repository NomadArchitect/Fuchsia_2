// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {diagnostics_data::Severity, test_list::TestTag};

/// Parameters that specify how a single test suite should be executed.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct TestParams {
    /// Test URL.
    pub test_url: String,

    /// |timeout_seconds|: Test timeout. Must be more than zero.
    pub timeout_seconds: Option<std::num::NonZeroU32>,

    /// Filter tests based on glob pattern(s).
    pub test_filters: Option<Vec<String>>,

    // Run disabled tests.
    pub also_run_disabled_tests: bool,

    /// Test concurrency count.
    pub parallel: Option<u16>,

    /// Arguments to pass to test using command line.
    pub test_args: Vec<String>,

    /// Maximum allowable log severity for the test.
    pub max_severity_logs: Option<Severity>,

    /// List of tags to associate with this test's output.
    pub tags: Vec<TestTag>,
}

/// Parameters that specify how the overall test run should be executed.
pub struct RunParams {
    /// The behavior of the test run if a suite times out.
    pub timeout_behavior: TimeoutBehavior,

    /// If set, stop executing tests after this number of normal test failures occur.
    pub stop_after_failures: Option<std::num::NonZeroU32>,
}

/// Sets the behavior of the overall run if a suite terminates with a timeout.
pub enum TimeoutBehavior {
    /// Immediately terminate any suites that haven't started.
    TerminateRemaining,
    /// Continue executing any suites that haven't finished.
    Continue,
}
