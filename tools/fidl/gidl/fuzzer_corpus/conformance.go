// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fuzzer_corpus

import (
	"bytes"
	"encoding/binary"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path"

	gidlconfig "go.fuchsia.dev/fuchsia/tools/fidl/gidl/config"
	gidlir "go.fuchsia.dev/fuchsia/tools/fidl/gidl/ir"
	fidl "go.fuchsia.dev/fuchsia/tools/fidl/lib/fidlgen"
)

type distributionEntry struct {
	Source      string `json:"source"`
	Destination string `json:"destination"`
}

type testCase struct {
	name        string
	objectTypes []fidl.ObjectType
	bytes       []byte
}

func getEncoding(encodings []gidlir.Encoding) (gidlir.Encoding, bool) {
	for _, encoding := range encodings {
		// Only supported encoding wire format: v1.
		if encoding.WireFormat == gidlir.V1WireFormat {
			return encoding, true
		}
	}

	return gidlir.Encoding{}, false
}

func getHandleDispositionEncoding(encodings []gidlir.HandleDispositionEncoding) (gidlir.HandleDispositionEncoding, bool) {
	for _, encoding := range encodings {
		// Only supported encoding wire format: v1.
		if encoding.WireFormat == gidlir.V1WireFormat {
			return encoding, true
		}
	}

	return gidlir.HandleDispositionEncoding{}, false
}

func getHandleObjectTypes(handles []gidlir.Handle, defs []gidlir.HandleDef) []fidl.ObjectType {
	var objectTypes []fidl.ObjectType
	for _, h := range handles {
		objectTypes = append(objectTypes, fidl.ObjectTypeFromHandleSubtype(defs[h].Subtype))
	}
	return objectTypes
}

func convertEncodeSuccesses(gtcs []gidlir.EncodeSuccess) []testCase {
	var tcs []testCase
	for _, gtc := range gtcs {
		encoding, ok := getHandleDispositionEncoding(gtc.Encodings)
		if !ok {
			continue
		}

		tcs = append(tcs, testCase{
			name:        fmt.Sprintf("EncodeSuccess_%s", gtc.Name),
			objectTypes: getHandleObjectTypes(gidlir.GetHandlesFromHandleDispositions(encoding.HandleDispositions), gtc.HandleDefs),
			bytes:       encoding.Bytes,
		})
	}

	return tcs
}

func convertDecodeSuccesses(gtcs []gidlir.DecodeSuccess) (tcs []testCase) {
	for _, gtc := range gtcs {
		encoding, ok := getEncoding(gtc.Encodings)
		if !ok {
			continue
		}

		tcs = append(tcs, testCase{
			name:        fmt.Sprintf("DecodeSuccess_%s", gtc.Name),
			objectTypes: getHandleObjectTypes(encoding.Handles, gtc.HandleDefs),
			bytes:       encoding.Bytes,
		})
	}

	return tcs
}

func convertDecodeFailures(gtcs []gidlir.DecodeFailure) (tcs []testCase) {
	for _, gtc := range gtcs {
		encoding, ok := getEncoding(gtc.Encodings)
		if !ok {
			continue
		}

		tcs = append(tcs, testCase{
			name:        fmt.Sprintf("DecodeFailure_%s", gtc.Name),
			objectTypes: getHandleObjectTypes(encoding.Handles, gtc.HandleDefs),
			bytes:       encoding.Bytes,
		})
	}

	return tcs
}

func getData(tc testCase) []byte {
	var buf bytes.Buffer
	binary.Write(&buf, binary.LittleEndian, uint64(len(tc.objectTypes)))
	for _, objectType := range tc.objectTypes {
		binary.Write(&buf, binary.LittleEndian, uint32(objectType))
	}
	buf.Write(tc.bytes)
	return buf.Bytes()
}

func writeTestCase(hostDir string, packageDataDir string, tc testCase) (distributionEntry, error) {
	data := getData(tc)

	filePath := path.Join(hostDir, tc.name)
	file, err := os.Create(filePath)
	if err != nil {
		return distributionEntry{}, err
	}
	defer file.Close()

	if _, err = file.Write(data); err != nil {
		return distributionEntry{}, err
	}

	return distributionEntry{
		Source:      filePath,
		Destination: path.Join("data", packageDataDir, tc.name),
	}, err
}

func GenerateConformanceTests(gidl gidlir.All, _ fidl.Root, config gidlconfig.GeneratorConfig) ([]byte, error) {
	if config.FuzzerCorpusHostDir == "" {
		return nil, errors.New("Must specify --fuzzer-corpus-host-dir when generating fuzzer_corpus")
	}
	if config.FuzzerCorpusPackageDataDir == "" {
		return nil, errors.New("Must specify --fuzzer-corpus-package-data-dir when generating fuzzer_corpus")
	}

	os.RemoveAll(config.FuzzerCorpusHostDir)
	err := os.MkdirAll(config.FuzzerCorpusHostDir, os.ModePerm)
	if err != nil {
		return nil, err
	}

	var manifest []distributionEntry

	for _, tcs := range [][]testCase{
		convertEncodeSuccesses(gidl.EncodeSuccess),
		convertDecodeSuccesses(gidl.DecodeSuccess),
		convertDecodeFailures(gidl.DecodeFailure),
	} {
		for _, tc := range tcs {
			entry, err := writeTestCase(config.FuzzerCorpusHostDir, config.FuzzerCorpusPackageDataDir, tc)
			if err != nil {
				return nil, err
			}
			manifest = append(manifest, entry)
		}
	}

	return json.Marshal(manifest)
}
