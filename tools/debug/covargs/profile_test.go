// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package covargs

import (
	"context"
	"fmt"
	"path/filepath"
	"reflect"
	"sort"
	"testing"

	"go.fuchsia.dev/fuchsia/tools/debug/elflib"
	"go.fuchsia.dev/fuchsia/tools/debug/symbolize"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

type repository []elflib.BinaryFileRef

func (r repository) GetBuildObject(buildID string) (symbolize.FileCloser, error) {
	for _, file := range r {
		if file.BuildID == buildID {
			return symbolize.NopFileCloser(file.Filepath), nil
		}
	}
	return nil, fmt.Errorf("could not find file associated with %s", buildID)
}

var testRepository = repository{
	{Filepath: filepath.Join("testdata", "libc.so"), BuildID: "5bf6a28a259b95b4f20ffbcea0cbb149"},
	{Filepath: filepath.Join("testdata", "libfbl.so"), BuildID: "4fcb712aa6387724a9f465a32cd8c14b"},
	{Filepath: filepath.Join("testdata", "foo"), BuildID: "12ef5c50b3ed3599c07c02d4509311be"},
	{Filepath: filepath.Join("testdata", "bar"), BuildID: "e242ed3bffccdf271b7fbaf34ed72d08"},
}

var testDumps = map[string]symbolize.DumpEntry{
	"llvm-profile.1234": {
		Modules: []symbolize.Module{
			{Name: "foo", Build: "12ef5c50b3ed3599c07c02d4509311be", Id: 0},
			{Name: "libc.so", Build: "5bf6a28a259b95b4f20ffbcea0cbb149", Id: 1},
			{Name: "libfbl.so", Build: "4fcb712aa6387724a9f465a32cd8c14b", Id: 2},
		},
		Segments: []symbolize.Segment{},
		Type:     "llvm-profile",
		Name:     "llvm-profile.1234",
	},
	"llvm-profile.5678": {
		Modules: []symbolize.Module{
			{Name: "bar", Build: "e242ed3bffccdf271b7fbaf34ed72d08", Id: 0},
			{Name: "libc.so", Build: "5bf6a28a259b95b4f20ffbcea0cbb149", Id: 1},
		},
		Segments: []symbolize.Segment{},
		Type:     "llvm-profile",
		Name:     "llvm-profile.5678",
	},
}

var testSummary = runtests.DataSinkMap{
	"llvm-profile": {
		{Name: "llvm-profile.1234", File: "build/llvm-profile.4321"},
		{Name: "llvm-profile.5678", File: "build/llvm-profile.8765"},
		// Duplicate sinks should be deduped.
		{Name: "llvm-profile.1234", File: "build/llvm-profile.4321"},
		{Name: "llvm-profile.5678", File: "build/llvm-profile.8765"},
	},
}

var testEntries = []ProfileEntry{
	{Profile: "build/llvm-profile.4321", Modules: []string{"12ef5c50b3ed3599c07c02d4509311be", "5bf6a28a259b95b4f20ffbcea0cbb149", "4fcb712aa6387724a9f465a32cd8c14b"}},
	{Profile: "build/llvm-profile.8765", Modules: []string{"e242ed3bffccdf271b7fbaf34ed72d08", "5bf6a28a259b95b4f20ffbcea0cbb149"}},
}

func sortEntries(entries []ProfileEntry) {
	sort.Slice(entries, func(i, j int) bool { return entries[i].Profile < entries[j].Profile })
	for _, entry := range entries {
		sort.Strings(entry.Modules)
	}
}

func TestMergeEntries(t *testing.T) {
	ctx := context.Background()

	entries, err := MergeEntries(ctx, testDumps, testSummary)
	if err != nil {
		t.Fatal(err)
	}
	sortEntries(entries)
	sortEntries(testEntries)

	if !reflect.DeepEqual(entries, testEntries) {
		t.Error("expected", testEntries, "but got", entries)
	}
}
