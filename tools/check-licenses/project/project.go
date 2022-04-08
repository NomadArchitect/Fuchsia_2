// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package project

import (
	"bufio"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/license"
)

// Project struct follows the format of README.fuchsia files.
// For more info, see the following article:
//   https://fuchsia.dev/fuchsia-src/development/source_code/third-party-metadata
type Project struct {
	Root            string
	ReadmePath      string
	Files           []*file.File
	SearchableFiles []*file.File
	LicenseFileType file.FileType
	RegularFileType file.FileType
	CustomFields    []string
	SearchResults   []*license.SearchResult

	// These fields are taken directly from the README.fuchsia files
	Name               string
	URL                string
	Version            string
	License            string
	LicenseFile        []*file.File
	UpstreamGit        string
	Description        string
	LocalModifications string
}

// Order implements sort.Interface for []*Project based on the Root field.
type Order []*Project

func (a Order) Len() int           { return len(a) }
func (a Order) Swap(i, j int)      { a[i], a[j] = a[j], a[i] }
func (a Order) Less(i, j int) bool { return a[i].Root < a[j].Root }

// NewProject creates a Project object from a README.fuchsia file.
func NewProject(readmePath string, projectRootPath string) (*Project, error) {
	var err error
	licenseFilePaths := make([]string, 0)

	// Make all projectRootPath values relative to Config.FuchsiaDir.
	if strings.Contains(projectRootPath, Config.FuchsiaDir) {
		projectRootPath, err = filepath.Rel(Config.FuchsiaDir, projectRootPath)
		if err != nil {
			return nil, err
		}
	}

	// If a project in the fuchsia tree is missing a README.fuchsia file
	// (or has a malformed README.fuchsia file), we can create our own
	// README.fuchsia file in a custom location.
	//
	// Those custom readmes are processed during initialization, and
	// will be included in this AllProjects map.
	//
	// If we get here and find that this project already exists, just
	// return the previously initialized instance.
	if _, ok := AllProjects[projectRootPath]; ok {
		plusVal(NumPreviousProjectRetrieved, projectRootPath)

		// Now we know a custom-initialized project exists for this directory.
		//
		// If a real (non-custom) README.fuchsia file also exists
		// in this location, we must have wanted to skip it (perhaps it is malformed).
		//
		// Keep a record of these situations, so we can resolve them.
		if _, err := os.Stat(readmePath); err == nil {
			plusVal(DuplicateReadmeFiles, readmePath)
		}

		return AllProjects[projectRootPath], nil
	}

	// There are a ton of rust_crate projects that don't (and will never) have a README.fuchsia file.
	// Handle those projects separately.
	if strings.Contains(projectRootPath, "rust_crates") {
		return NewSpecialProject(projectRootPath)
	}

	// Same goes for golib projects
	if strings.Contains(projectRootPath, "golibs") {
		return NewSpecialProject(projectRootPath)
	}

	// Same goes for 3p golang.org projects
	if strings.Contains(projectRootPath, "golang.org") {
		return NewSpecialProject(projectRootPath)
	}

	// Same goes for several syzkaller golang projects
	if strings.Contains(projectRootPath, "syzkaller/vendor") {
		return NewSpecialProject(projectRootPath)
	}

	// Same goes for dart-pkg projects
	if strings.Contains(projectRootPath, "dart-pkg") {
		return NewSpecialProject(projectRootPath)
	}

	// Double-check that this README.fuchsia file actually exists.
	if _, err := os.Stat(readmePath); os.IsNotExist(err) {
		return nil, err
	}

	p := &Project{
		Root:            projectRootPath,
		ReadmePath:      readmePath,
		LicenseFileType: file.SingleLicense,
		RegularFileType: file.Any,
	}

	f, err := os.Open(readmePath)
	if err != nil {
		return nil, fmt.Errorf("NewProject(%v): %v\n", projectRootPath, err)
	}
	defer f.Close()

	s := bufio.NewScanner(f)
	s.Split(bufio.ScanLines)

	multiline := ""
	for s.Scan() {
		var line = s.Text()
		if strings.HasPrefix(line, "Name:") {
			p.Name = strings.TrimSpace(strings.TrimPrefix(line, "Name:"))
			multiline = ""
		} else if strings.HasPrefix(line, "URL:") {
			p.URL = strings.TrimSpace(strings.TrimPrefix(line, "URL:"))
			multiline = ""
		} else if strings.HasPrefix(line, "Version:") {
			p.Version = strings.TrimSpace(strings.TrimPrefix(line, "Version:"))
			multiline = ""
		} else if strings.HasPrefix(line, "License:") {
			p.License = strings.TrimSpace(strings.TrimPrefix(line, "License:"))
			multiline = ""
		} else if strings.HasPrefix(line, "License File:") {
			f := strings.TrimSpace(strings.TrimPrefix(line, "License File:"))
			if len(f) > 0 {
				licenseFilePaths = append(licenseFilePaths, f)
			}
			multiline = ""
		} else if strings.HasPrefix(line, "Upstream Git:") {
			p.UpstreamGit = strings.TrimSpace(strings.TrimPrefix(line, "Upstream Git:"))
			multiline = ""
		} else if strings.HasPrefix(line, "check-licenses:") {
			p.CustomFields = append(p.CustomFields, (strings.TrimSpace(strings.TrimPrefix(line, "check-licenses:"))))
		} else if strings.HasPrefix(line, "Description:") {
			multiline = "Description"
		} else if strings.HasPrefix(line, "Local Modifications:") {
			multiline = "Local Modifications"
		} else if multiline == "Description" {
			p.Description += strings.TrimSpace(strings.TrimPrefix(line, "Description:")) + "\n"
		} else if multiline == "Local Modifications" {
			p.LocalModifications += strings.TrimSpace(strings.TrimPrefix(line, "Local Modifications:")) + "\n"
		} else if strings.TrimSpace(line) == "" {
			// Empty lines are OK
		} else {
			plusVal(UnknownReadmeLines, readmePath)
		}
	}

	// All projects must have a name.
	if p.Name == "" {
		plusVal(MissingName, p.ReadmePath)
	}

	// All projects must point to a license file.
	if len(licenseFilePaths) == 0 {
		plusVal(MissingLicenseFile, p.ReadmePath)
	}

	if err := p.processCustomFields(); err != nil {
		return nil, err
	}

	for _, l := range licenseFilePaths {
		l = filepath.Join(Config.FuchsiaDir, p.Root, l)
		l = filepath.Clean(l)

		licenseFile, err := file.NewFile(l, p.LicenseFileType)
		if err != nil {
			return nil, err
		}
		p.LicenseFile = append(p.LicenseFile, licenseFile)
	}

	plusVal(NumProjects, p.Root)
	AllProjects[p.Root] = p

	return p, nil
}

// We can put some information in the README.fuchsia files to help check-licenses
// do the right thing (e.g. specify the format of the NOTICE file).
func (p *Project) processCustomFields() error {
	for _, line := range p.CustomFields {
		if strings.HasPrefix(line, "license format:") {
			ft := strings.TrimSpace(strings.TrimPrefix(line, "license format:"))
			if val, ok := file.FileTypes[ft]; ok {
				p.LicenseFileType = val
			} else {
				return fmt.Errorf("Format %v isn't a valid License Format.", ft)
			}
		} else if strings.HasPrefix(line, "file format:") {
			ft := strings.TrimSpace(strings.TrimPrefix(line, "file format:"))
			if val, ok := file.FileTypes[ft]; ok {
				p.RegularFileType = val
			} else {
				return fmt.Errorf("Format %v isn't a valid License Format.", ft)
			}
		}
	}
	return nil
}

func (p *Project) AddFiles(filepaths []string) error {
	licenseFileMap := make(map[string]bool, 0)
	for _, lpath := range p.LicenseFile {
		licenseFileMap[lpath.Path] = true
	}

	for _, path := range filepaths {
		if _, ok := licenseFileMap[path]; ok {
			continue
		}

		f, err := file.NewFile(path, p.RegularFileType)
		if err != nil {
			return err
		}
		p.Files = append(p.Files, f)

		ext := filepath.Ext(path)
		if _, ok := file.Config.Extensions[ext]; ok {
			p.SearchableFiles = append(p.SearchableFiles, f)
		}
	}
	return nil
}
