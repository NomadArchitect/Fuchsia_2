// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file

package main

import (
	"flag"
	"fmt"
	"strings"
	"testing"

	"github.com/google/go-cmp/cmp"
	"github.com/google/go-cmp/cmp/cmpopts"
	"go.fuchsia.dev/fuchsia/sdk/cts/plasa/model"
)

var testDir = flag.String("test_data_dir", "", "The directory where test data reside")

func TestTestDir(t *testing.T) {
	if *testDir == "" {
		t.Fatalf("the required flag --test_data_dir=... was not supplied")
	}
}

func TestReportGeneration(t *testing.T) {
	t.Parallel()
	tests := []struct {
		name           string
		args           args
		expectedOutput string
	}{
		{
			name: "fidling",
			args: args{
				allowlistFilenameRegexp: []string{"fidling"},
			},
			expectedOutput: `{
			"items": [
				{
					"name": "fuchsia::ui::composition::CreateImageArgs::ValueUnion_image_id::ValueUnion_image_id",
					"file": "fidling/gen/sdk/fidl/fuchsia.ui.composition/fuchsia.ui.composition/hlcpp/fuchsia/ui/composition/cpp/fidl.h",
					"line": 1675
				},
				{
					"name": "fuchsia::ui::composition::CreateImageArgs::ValueUnion_image_id::~ValueUnion_image_id",
					"file": "fidling/gen/sdk/fidl/fuchsia.ui.composition/fuchsia.ui.composition/hlcpp/fuchsia/ui/composition/cpp/fidl.h",
					"line": 1676
				}
			]}
			`,
		},
		{
			name: "fdio",
			args: args{
				allowlistNameRegexp: []string{`^fdio.*`},
			},
			expectedOutput: `{
			"items": [
				{"name": "fdio_bind_to_fd"},
				{"name": "fdio_create"},
				{"name": "fdio_cwd_clone"},
				{"name": "fdio_fd_clone"},
				{"name": "fdio_fd_create"},
				{"name": "fdio_fd_create_null"},
				{"name": "fdio_fd_transfer"},
				{"name": "fdio_get_service_handle"},
				{"name": "fdio_get_zxio"},
				{"name": "fdio_ns_bind"},
				{"name": "fdio_ns_bind_fd"},
				{"name": "fdio_ns_chdir"},
				{"name": "fdio_ns_connect"},
				{"name": "fdio_ns_create"},
				{"name": "fdio_ns_destroy"},
				{"name": "fdio_ns_export"},
				{"name": "fdio_ns_export_root"},
				{"name": "fdio_ns_free_flat_ns"},
				{"name": "fdio_ns_get_installed"},
				{"name": "fdio_ns_is_bound"},
				{"name": "fdio_ns_opendir"},
				{"name": "fdio_ns_unbind"},
				{"name": "fdio_null_create"},
				{"name": "fdio_open"},
				{"name": "fdio_open_at"},
				{"name": "fdio_open_fd"},
				{"name": "fdio_open_fd_at"},
				{"name": "fdio_service_clone"},
				{"name": "fdio_service_clone_to"},
				{"name": "fdio_service_connect"},
				{"name": "fdio_service_connect_at"},
				{"name": "fdio_service_connect_by_name"},
				{"name": "fdio_unbind_from_fd"},
				{"name": "fdio_zxio_create"}
			]}
			`,
		},
	}

	for _, test := range tests {
		test := test
		t.Run(test.name, func(t *testing.T) {
			dirName := *testDir
			var output strings.Builder
			if err := run(dirName, &output, test.args); err != nil {
				t.Fatalf("error invoking main.run() from the test:\n\t%v", err)
			}

			fmt.Printf("TEST: %+v: output: %+v\n", dirName, output.String())

			actual, err := model.ReadReportJSON(strings.NewReader(output.String()))
			if err != nil {
				t.Fatalf("error: could not read report from JSON:\n\t%v", err)
			}

			fmt.Printf("TEST: %+v: actual: %+v\n", dirName, actual)

			expected, err := model.ReadReportJSON(strings.NewReader(test.expectedOutput))
			if err != nil {
				t.Fatalf("error: could not read from expectedOutput:\n\t%v", err)
			}
			if !cmp.Equal(expected, actual, cmpopts.IgnoreUnexported(model.Report{})) {
				t.Errorf("error: want: %+v\n\ngot: %+v\n\ndiff: %v",
					expected, actual, cmp.Diff(actual, expected, cmpopts.IgnoreUnexported(model.Report{})))
			}
		})
	}
}
