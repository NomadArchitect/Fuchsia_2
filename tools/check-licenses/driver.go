// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package checklicenses

import (
	"context"
	"log"
	"runtime/trace"
	"time"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/directory"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/license"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/result"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/result/world"
)

func Execute(ctx context.Context, config *CheckLicensesConfig) error {
	start := time.Now()

	startInitialize := time.Now()
	log.Print("Initializing... ")
	if err := initialize(config); err != nil {
		log.Println("Error!")
		return err
	}
	log.Printf("Done. [%v]\n", time.Since(startInitialize))

	r := trace.StartRegion(ctx, "directory.NewDirectory("+config.Target+")")
	startDirectory := time.Now()
	log.Print("Discovering files and folders... ")
	_, err := directory.NewDirectory(config.Target, nil)
	if err != nil {
		log.Println("Error!")
		return err
	}
	log.Printf("Done. [%v]\n", time.Since(startDirectory))
	r.End()

	r = trace.StartRegion(ctx, "project.AnalyzeLicenses")
	startAnalyze := time.Now()
	log.Printf("Searching for license texts [%v projects]... ", len(project.AllProjects))
	err = project.AnalyzeLicenses()
	if err != nil {
		log.Println("Error!")
		return err
	}

	log.Printf("Done. [%v]\n", time.Since(startAnalyze))
	r.End()

	r = trace.StartRegion(ctx, "result.SaveResults")
	startSaveResults := time.Now()

	log.Print("Saving results... ")
	var s string
	s, err = result.SaveResults()
	if err != nil {
		return err
	}
	log.Printf("Done. [%v]\n", time.Since(startSaveResults))
	r.End()

	log.Printf("\nTotal runtime: %v\n============\n", time.Since(start))
	log.Println(s)
	return nil
}

func initialize(c *CheckLicensesConfig) error {
	if err := file.Initialize(c.File); err != nil {
		return err
	}
	if err := license.Initialize(c.License); err != nil {
		return err
	}
	if err := project.Initialize(c.Project); err != nil {
		return err
	}
	if err := directory.Initialize(c.Directory); err != nil {
		return err
	}
	if err := result.Initialize(c.Result); err != nil {
		return err
	}
	if err := world.Initialize(c.World); err != nil {
		return err
	}

	return nil
}
