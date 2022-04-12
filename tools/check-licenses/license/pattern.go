// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package license

import (
	"fmt"
	"io/ioutil"
	"path/filepath"
	"regexp"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
)

// Pattern contains a searchable regex pattern for finding license text
// in source files and LICENSE files across the repository.
type Pattern struct {
	// Name is the name of the license pattern file.
	Name string

	// Type is the type of license that this pattern matches.
	// e.g. BSD, MIT, etc..
	// This is set using the name of the parent folder where the pattern file lives.
	Type string

	// Category is the category that this license belongs to.
	// Approved, Restricted, Allowlist_Only
	// This is set using the name of the grandparent folder where the pattern file lives.
	Category string

	// AllowList is a string of regex patterns that match with project paths.
	// This license pattern is only allowed to match with projects listed here.
	AllowList []string

	// Matches maintains a slice of pointers to the data fragments that it matched against.
	// This is used in some templates in the result package, for grouping license texts
	// by pattern type in the resulting NOTICE file.
	Matches []*file.FileData

	// The regex pattern.
	// This is exported so the result package can save the pattern to disk at the end.
	Re *regexp.Regexp

	// Maps that keep track of previous successful and failed
	// searches, keyed using filedata hash.
	previousMatches    map[string]bool
	previousMismatches map[string]bool

	isHeader bool
}

// Order implements sort.Interface for []*Pattern based on the Name field.
type Order []*Pattern

func (a Order) Len() int           { return len(a) }
func (a Order) Swap(i, j int)      { a[i], a[j] = a[j], a[i] }
func (a Order) Less(i, j int) bool { return a[i].Name < a[j].Name }

// NewPattern returns a Pattern object with the regex pattern loaded from the .lic folder.
// Some preprocessing is done to the pattern (e.g. removing code comment characters).
func NewPattern(path string) (*Pattern, error) {
	bytes, err := ioutil.ReadFile(path)
	if err != nil {
		return nil, err
	}
	regex := string(bytes)

	// Remove any duplicate whitespace characters
	regex = strings.Join(strings.Fields(regex), " ")

	// Update regex to ignore multiple white spaces, newlines, comments.
	regex = strings.ReplaceAll(regex, ` `, `([\s\\#\*\/]|\^L)*`)

	// Convert date strings to a regex that supports any date
	dates := regexp.MustCompile(`(\D)[\d]{4}(\D)`)
	regex = dates.ReplaceAllString(regex, `$1[\d]{4}$2`)

	re, err := regexp.Compile(regex)
	if err != nil {
		return nil, fmt.Errorf("%s: %w", path, err)
	}

	name := filepath.Base(path)

	// Retrieve the license type (e.g. MIT, BSD) from the filepath.
	licType := filepath.Base(filepath.Dir(path))

	// Retrieve the license category (e.g. Approved, Restricted) from the filepath.
	licCategory := filepath.Base(filepath.Dir(filepath.Dir(path)))

	allowlist := make([]string, 0)
	if licCategory == "approved" || licCategory == "notice" {
		allowlist = append(allowlist, ".*")
	} else {
		// allowlist_only and restricted
		// TODO: make restricted licenses un-allowlist-able.
		if regexes, ok := AllowListPatternMap[name]; ok {
			allowlist = append(allowlist, regexes...)
		}
	}

	return &Pattern{
		Name:               name,
		Type:               licType,
		Category:           licCategory,
		AllowList:          allowlist,
		Matches:            make([]*file.FileData, 0),
		previousMatches:    make(map[string]bool),
		previousMismatches: make(map[string]bool),
		Re:                 re,
	}, nil
}

// Search the given data slice for text that matches this Pattern regex.
func (p *Pattern) Search(d *file.FileData) bool {
	// If the data is empty, and this pattern is "_empty", return true.
	if len(d.Data) == 0 && p.Name == "_empty" {
		return true
	}

	// If we've seen this data segment before, return the previous result.
	// This should be faster than running the regex search.
	if _, ok := p.previousMatches[d.Hash()]; ok && !p.isHeader {
		p.Matches = append(p.Matches, d)
		return true
	} else if _, ok := p.previousMismatches[d.Hash()]; ok {
		return false
	}

	if m := p.Re.Find(d.Data); m != nil {
		// If this is a source file with the copyright header info at the top,
		// modify the filedata object to hold the copyright info instead of the
		// full file contents.
		if p.isHeader {
			d.SetData(m)
		}

		p.Matches = append(p.Matches, d)
		p.previousMatches[d.Hash()] = true

		return true
	}

	p.previousMismatches[d.Hash()] = true
	return false
}
