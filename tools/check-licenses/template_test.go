// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package checklicenses

import (
	"io/ioutil"
	"path/filepath"
	"testing"
)

func TestSaveToOutputFile(t *testing.T) {
	path := filepath.Join(t.TempDir(), "config.json")
	json := `{"skipFiles":[".gitignore"],"skipDirs":[".git"],"textExtensionList":["go"],"maxReadSize":6144,"outputFilePrefix":"NOTICE","outputFileExtensions":["txt"],"singleLicenseFiles":["LICENSE"],"licensePatternDir":"golden/","baseDir":".","target":"all","logLevel":"verbose"}`
	if err := ioutil.WriteFile(path, []byte(json), 0o600); err != nil {
		t.Errorf("%v(): got %v", t.Name(), err)
	}
	config, err := NewConfig(path)
	if err != nil {
		t.Errorf("%v(): got %v", t.Name(), err)
	}
	// TODO(omerlevran): Add test.
	config.OutputFileExtensions = []string{"html.gz"}
	config.OutputFileExtensions = []string{"html"}
}
