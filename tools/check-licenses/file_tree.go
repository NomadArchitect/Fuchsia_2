// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package checklicenses

import (
	"context"
	"log"
	"os"
	"path/filepath"
	"regexp"
	"runtime/trace"
	"sort"
	"strings"
	"sync"
)

// FileTree is an in memory representation of the state of the repository.
type FileTree struct {
	Name               string                `json:"name"`
	Path               string                `json:"path"`
	SingleLicenseFiles map[string][]*License `json:"project licenses"`
	Files              []*File               `json:"files"`
	Children           []*FileTree           `json:"children"`
	Parent             *FileTree             `json:"-"`
	StrictAnalysis     bool                  `json:"strict analysis"`

	sync.RWMutex
}

// filetreeByPath implements sort.Interface for []*FileTree based on the Path field.
type filetreeByPath []*FileTree

func (a filetreeByPath) Len() int           { return len(a) }
func (a filetreeByPath) Swap(i, j int)      { a[i], a[j] = a[j], a[i] }
func (a filetreeByPath) Less(i, j int) bool { return a[i].Path < a[j].Path }

// NewFileTree returns an instance of FileTree, given the input configuration
// file.
func NewFileTree(ctx context.Context, root string, parent *FileTree, config *Config, metrics *Metrics) (*FileTree, error) {
	defer trace.StartRegion(ctx, "NewFileTree").End()
	ft := FileTree{
		SingleLicenseFiles: make(map[string][]*License),
	}

	abs, err := filepath.Abs(root)
	if err != nil {
		return nil, err
	}

	ft.Name = filepath.Base(abs)
	ft.Path = abs
	ft.Parent = parent

	// If config.StrictAnalysis is true, we ignore all LICENSE files in the fuchsia directory.
	if parent == nil {
		ft.StrictAnalysis = config.StrictAnalysis
	} else {
		ft.StrictAnalysis = parent.StrictAnalysis
	}

	// If we are at a boundary where licenses change (e.g. "third_party" or "prebuilt" dirs),
	// turn off strict analysis. We don't have enough control over 3p repositories to enforce
	// having strict LICENSE information in all source files.
	for _, dirName := range config.StopLicensePropagation {
		if ft.Name == dirName {
			ft.StrictAnalysis = false
			break
		}
	}

	for _, customProjectLicense := range config.CustomProjectLicenses {
		if strings.HasSuffix(root, customProjectLicense.ProjectRoot) {
			metrics.increment("num_single_license_files")
			licLocation := filepath.Join(root, customProjectLicense.LicenseLocation)
			ft.SingleLicenseFiles[licLocation] = []*License{}
			break
		}
	}

	err = filepath.Walk(root, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			if strings.Contains(err.Error(), "no such file or directory") {
				log.Printf("skipping non-existing file: %s", path)
				return nil
			}
			return err
		}
		if skippable, err := isSkippable(config, path, info); skippable {
			log.Printf("skipping: %s", path)
			return err
		}
		if info.IsDir() {
			if path != root {
				child, err := NewFileTree(ctx, path, &ft, config, metrics)
				if err != nil {
					return err
				}
				ft.Children = append(ft.Children, child)
				return filepath.SkipDir
			}
			return nil
		}
		newFile, err := NewFile(path, &ft)

		// TODO(jcecil): a file named LICENSE in the fuchsia tree will be
		// entirely skipped when running in strict analysis mode, since it
		// doesn't have a valid text extension. We should still analyze
		// these files, even if we don't add them as SingleLicenseFiles.
		if hasLowerPrefix(info.Name(), config.SingleLicenseFiles) && !ft.StrictAnalysis {
			metrics.increment("num_single_license_files")
			ft.SingleLicenseFiles[path] = []*License{}
			return nil
		}

		extensions := config.StrictTextExtensionList
		if !ft.StrictAnalysis {
			extensions = append(extensions, config.TextExtensionList...)
		}
		if hasExt(info.Name(), extensions) {
			metrics.increment("num_non_single_license_files")
			if err == nil {
				ft.Files = append(ft.Files, newFile)
			}
		} else {
			log.Printf("ignoring: %s", path)
			metrics.increment("num_extensions_excluded")
		}
		return nil
	})
	if err != nil {
		// TODO(jcecil): This must be an error.
		return nil, err
	}

	sort.Sort(fileByPath(ft.Files))
	sort.Sort(filetreeByPath(ft.Children))

	return &ft, nil
}

func (ft *FileTree) propagateProjectLicenses(config *Config) {
	propagate := true
	for _, dirName := range config.StopLicensePropagation {
		if ft.Name == dirName {
			propagate = false
			break
		}
	}

	if propagate && ft.Parent != nil {
		for key, val := range ft.Parent.SingleLicenseFiles {
			ft.SingleLicenseFiles[key] = val
		}
	}

	for _, child := range ft.Children {
		child.propagateProjectLicenses(config)
	}
}

func (ft *FileTree) getSingleLicenseFileIterator() <-chan *FileTree {
	ch := make(chan *FileTree, 1)
	go func() {
		var curr *FileTree
		var q []*FileTree
		q = append(q, ft)
		var pos int
		for len(q) > 0 {
			pos = len(q) - 1
			curr = q[pos]
			q = q[:pos]
			if len(curr.SingleLicenseFiles) > 0 {
				ch <- curr
			}
			curr.RLock()
			for _, child := range curr.Children {
				q = append(q, child)
			}
			curr.RUnlock()
		}
		close(ch)
	}()
	return ch
}

func (ft *FileTree) getFileIterator() <-chan *File {
	ch := make(chan *File, 1)
	go func() {
		var curr *FileTree
		var q []*FileTree
		q = append(q, ft)
		var pos int
		for len(q) > 0 {
			pos = len(q) - 1
			curr = q[pos]
			q = q[:pos]
			for _, file := range curr.Files {
				ch <- file
			}
			curr.RLock()
			for _, child := range curr.Children {
				q = append(q, child)
			}
			curr.RUnlock()
		}
		close(ch)
	}()
	return ch
}

func (ft *FileTree) Equal(other *FileTree) bool {
	if ft.Name != other.Name {
		return false
	}
	if ft.Path != other.Path {
		return false
	}
	if ft.StrictAnalysis != other.StrictAnalysis {
		return false
	}

	if len(ft.SingleLicenseFiles) != len(other.SingleLicenseFiles) {
		return false
	}
	for k := range ft.SingleLicenseFiles {
		left := ft.SingleLicenseFiles[k]
		right := other.SingleLicenseFiles[k]
		if len(left) != len(right) {
			return false
		}
		for i := range left {
			if left[i] != right[i] {
				return false
			}
		}
	}

	if len(ft.Files) != len(other.Files) {
		return false
	}
	for i := range ft.Files {
		if !ft.Files[i].Equal(other.Files[i]) {
			return false
		}
	}

	if len(ft.Children) != len(other.Children) {
		return false
	}
	for k := range ft.Children {
		if ft.Children[k].Equal(other.Children[k]) {
			return false
		}
	}

	return true
}

// hasExt returns true if path has one of the extensions in the list.
func hasExt(path string, exts []string) bool {
	if ext := filepath.Ext(path); ext != "" {
		ext = ext[1:]
		for _, e := range exts {
			if e == ext {
				return true
			}
		}
	}
	return false
}

// hasLowerPrefix returns true if the name has one of files as a prefix in
// lower case.
func hasLowerPrefix(name string, files []string) bool {
	name = strings.ToLower(name)
	for _, f := range files {
		if strings.HasPrefix(name, f) {
			return true
		}
	}
	return false
}

func isSkippable(config *Config, path string, info os.FileInfo) (bool, error) {
	if info.Size() == 0 {
		// An empty file has no content to copyright. Skip.
		return true, nil
	}
	for _, skipFile := range config.SkipFiles {
		if strings.ToLower(info.Name()) == skipFile || strings.ToLower(path) == skipFile {
			return true, nil
		}
	}
	for _, dontSkipDir := range config.DontSkipDirs {
		if buildDirRegex(dontSkipDir).Find([]byte(path)) != nil {
			return false, nil
		}
	}

	parents := strings.Split(path, "/")

	for _, skipDir := range config.SkipDirs {
		var err error
		if info.IsDir() {
			err = filepath.SkipDir
		}
		for _, dontSkipDir := range config.DontSkipDirs {
			// Check if there are `dontSkipDirs` within this `skipDir`.
			// If there are none, then we can return `filepath.SkipDir`.
			if strings.HasPrefix(dontSkipDir, skipDir) {
				// There is a `dontSkipDir` within this `skipDir`, so we
				// cannot return `filepath.SkipDir`.
				err = nil
			}
		}
		if buildDirRegex(skipDir).Find([]byte(path)) != nil {
			return true, err
		}

		for _, p := range parents {
			if p == skipDir {
				return true, err
			}
		}
	}
	return false, nil
}

// Build the regex for matching skip/dontSkip paths.
//
// We want to be able to match our skip/dontSkip directories in three ways:
// 1. At start of the path
// 2. In the middle of the path
// 3. At the end of the path
//
// Normally this would call for `strings.Contains`, but we must be careful to
// not match on incorrect substring matches.
//
// For example:
// "this/path" should not match on "wait_but_this/path_is_different"
//
// So, the regex is adding slashes where appropriate for all three cases listed
// above. e.g. "this/path" -> "/this/path/" for the case of #2.
func buildDirRegex(want string) *regexp.Regexp {
	osSeparator := string(filepath.Separator)
	regex := strings.ReplaceAll(
		`(?:^A\`+osSeparator+`)|(?:\`+osSeparator+`A\`+osSeparator+`)|(?:\`+osSeparator+`A$)`,
		"A",
		strings.ReplaceAll(want, osSeparator, `\`+osSeparator))
	return regexp.MustCompile(regex)
}
