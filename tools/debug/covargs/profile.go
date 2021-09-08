// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package covargs

import (
	"context"

	"go.fuchsia.dev/fuchsia/tools/debug/symbolize"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

const llvmProfileSinkType = "llvm-profile"

type ProfileEntry struct {
	Profile string   `json:"profile"`
	Modules []string `json:"modules"`
}

// MergeEntries combines data from runtests and symbolizer, returning
// a sequence of entries, where each entry contains a raw profile and all
// modules (specified by build ID) present in that profile.
func MergeEntries(ctx context.Context, dumps map[string]symbolize.DumpEntry, summary runtests.DataSinkMap) ([]ProfileEntry, error) {
	sinkToModules := make(map[string]map[string]struct{})
	for _, sink := range summary[llvmProfileSinkType] {
		moduleSet, ok := sinkToModules[sink.File]
		if !ok {
			moduleSet = make(map[string]struct{})
		}

		if len(sink.BuildIDs) > 0 {
			for _, buildID := range sink.BuildIDs {
				moduleSet[buildID] = struct{}{}
			}
		} else {
			dump, ok := dumps[sink.Name]
			if !ok {
				logger.Warningf(ctx, "%s not found in summary file; unable to determine module build IDs\n", sink.Name)
				continue
			}
			for _, mod := range dump.Modules {
				moduleSet[mod.Build] = struct{}{}
			}
		}
		sinkToModules[sink.File] = moduleSet
	}

	entries := []ProfileEntry{}
	for sink, moduleSet := range sinkToModules {
		var modules []string
		for module := range moduleSet {
			modules = append(modules, module)
		}
		entries = append(entries, ProfileEntry{
			Modules: modules,
			Profile: sink,
		})
	}

	return entries, nil
}
