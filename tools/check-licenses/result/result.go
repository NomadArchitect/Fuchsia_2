// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package result

import (
	"bytes"
	"compress/gzip"
	"encoding/json"
	"fmt"
	"io/ioutil"
	"os"
	"path/filepath"
	"sort"
	"strconv"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/check-licenses/file"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/filetree"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/license"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/project"
	"go.fuchsia.dev/fuchsia/tools/check-licenses/result/world"
)

const (
	indent = "  "
)

// SaveResults saves the results to the output files defined in the config file.
func SaveResults() (string, error) {
	var b strings.Builder

	s, err := savePackageInfo("license", license.Config, license.Metrics)
	if err != nil {
		return "", err
	}
	for _, p := range license.AllPatterns {
		filename := fmt.Sprintf("%v-%v-%v", p.Category, p.Type, p.Name)
		if err := writeFile(filepath.Join("license", "patterns", filename), []byte(p.Re.String())); err != nil {
			return "", err
		}
		var m strings.Builder
		for _, match := range p.Matches {
			m.WriteString(match.FilePath)
			m.WriteString("\n")
		}
		if err := writeFile(filepath.Join("license", "matches", filename), []byte(m.String())); err != nil {
			return "", err
		}
	}
	for _, d := range license.Unrecognized.Matches {
		filename := fmt.Sprintf("%v.lic", d.LibraryName)
		if err := writeFile(filepath.Join("license", "unrecognized", filename), []byte(d.Data)); err != nil {
			return "", err
		}
	}
	b.WriteString(s)

	s, err = savePackageInfo("project", project.Config, project.Metrics)
	if err != nil {
		return "", err
	}
	b.WriteString(s)

	s, err = savePackageInfo("file", file.Config, file.Metrics)
	if err != nil {
		return "", err
	}
	b.WriteString(s)

	s, err = savePackageInfo("filetree", filetree.Config, filetree.Metrics)
	if err != nil {
		return "", err
	}
	b.WriteString(s)

	s, err = savePackageInfo("result", Config, Metrics)
	if err != nil {
		return "", err
	}
	b.WriteString(s)

	s, err = savePackageInfo("world", world.Config, world.Metrics)
	if err != nil {
		return "", err
	}
	b.WriteString(s)

	err = RunChecks()
	if err != nil {
		if Config.ExitOnError {
			return "", err
		} else {
			// TODO: Log err to a file
		}
	}

	if Config.OutputLicenseFile {
		s, err = expandTemplates()
		if err != nil {
			return "", err
		}
		b.WriteString(s)
	} else {
		b.WriteString("Not expanding templates.\n")
	}

	if err = writeFile("summary", []byte(b.String())); err != nil {
		return "", err
	}

	b.WriteString("\n")
	if Config.OutDir != "" {
		b.WriteString(fmt.Sprintf("Full summary and output files -> %s\n", Config.OutDir))
	} else {
		b.WriteString("Set the 'outputdir' arg in the config file to save detailed information to disk.\n")
	}
	return b.String(), nil
}

// This retrieves all the relevant metrics information for a given package.
// e.g. the //tools/check-licenses/filetree package.
func savePackageInfo(pkgName string, c interface{}, m MetricsInterface) (string, error) {
	var b strings.Builder

	fmt.Fprintf(&b, "\n%s Metrics:\n", strings.Title(pkgName))

	counts := m.Counts()
	keys := make([]string, 0, len(counts))
	for k := range counts {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	for _, k := range keys {
		fmt.Fprintf(&b, "%s%s: %s\n", indent, k, strconv.Itoa(counts[k]))
	}
	if Config.OutDir != "" {
		if _, err := os.Stat(Config.OutDir); os.IsNotExist(err) {
			err := os.Mkdir(Config.OutDir, 0755)
			if err != nil {
				return "", err
			}
		}

		if err := saveConfig(pkgName, c); err != nil {
			return "", err
		}
		if err := saveMetrics(pkgName, m); err != nil {
			return "", err
		}
	}
	return b.String(), nil
}

// Save the config files so we can recreate this run in the future.
func saveConfig(pkg string, c interface{}) error {
	if bytes, err := json.MarshalIndent(c, "", "  "); err != nil {
		return err
	} else {
		return writeFile(filepath.Join(pkg, "_config.json"), bytes)
	}
}

// Save the "Values" metrics: freeform data stored in a map with string keys.
func saveMetrics(pkg string, m MetricsInterface) error {
	for k, v := range m.Values() {
		sort.Strings(v)
		if bytes, err := json.MarshalIndent(v, "", "  "); err != nil {
			return err
		} else {
			k = strings.Replace(k, " ", "_", -1)
			if err := writeFile(filepath.Join(pkg, k), bytes); err != nil {
				return err
			}
		}
	}
	return nil
}

func writeFile(path string, data []byte) error {
	path = filepath.Join(Config.OutDir, path)
	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return err
	}
	return os.WriteFile(path, data, 0666)
}

func compressGZ(path string) error {
	d, err := ioutil.ReadFile(path)
	if err != nil {
		return err
	}

	buf := bytes.Buffer{}
	zw := gzip.NewWriter(&buf)
	if _, err := zw.Write(d); err != nil {
		return err
	}
	if err := zw.Close(); err != nil {
		return err
	}
	path, err = filepath.Rel(Config.OutDir, path)
	if err != nil {
		return err
	}
	return writeFile(path+".gz", buf.Bytes())
}
