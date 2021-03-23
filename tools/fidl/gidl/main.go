// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"io/ioutil"
	"log"
	"os"
	"sort"
	"strings"

	gidlc "go.fuchsia.dev/fuchsia/tools/fidl/gidl/c"
	gidlconfig "go.fuchsia.dev/fuchsia/tools/fidl/gidl/config"
	gidldart "go.fuchsia.dev/fuchsia/tools/fidl/gidl/dart"
	gidlcorpus "go.fuchsia.dev/fuchsia/tools/fidl/gidl/fuzzer_corpus"
	gidlgolang "go.fuchsia.dev/fuchsia/tools/fidl/gidl/golang"
	gidlhlcpp "go.fuchsia.dev/fuchsia/tools/fidl/gidl/hlcpp"
	gidlir "go.fuchsia.dev/fuchsia/tools/fidl/gidl/ir"
	gidlllcpp "go.fuchsia.dev/fuchsia/tools/fidl/gidl/llcpp"
	gidlparser "go.fuchsia.dev/fuchsia/tools/fidl/gidl/parser"
	gidlreference "go.fuchsia.dev/fuchsia/tools/fidl/gidl/reference"
	gidlrust "go.fuchsia.dev/fuchsia/tools/fidl/gidl/rust"
	gidlwalker "go.fuchsia.dev/fuchsia/tools/fidl/gidl/walker"
	"go.fuchsia.dev/fuchsia/tools/fidl/lib/fidlgen"
)

// Generator is a function that generates conformance tests for a particular
// backend and returns a map of test name to file bytes. The test name is
// added as a suffix to the name of the file before the extension
// (e.g. my_file.go -> my_file_test_name.go).
// The first file is the "main output file".
type Generator func(gidlir.All, fidlgen.Root, gidlconfig.GeneratorConfig) ([]byte, error)

var conformanceGenerators = map[string]Generator{
	"c":             gidlc.GenerateConformanceTests,
	"go":            gidlgolang.GenerateConformanceTests,
	"llcpp":         gidlllcpp.GenerateConformanceTests,
	"hlcpp":         gidlhlcpp.GenerateConformanceTests,
	"dart":          gidldart.GenerateConformanceTests,
	"rust":          gidlrust.GenerateConformanceTests,
	"fuzzer_corpus": gidlcorpus.GenerateConformanceTests,
}

var benchmarkGenerators = map[string]Generator{
	"go":        gidlgolang.GenerateBenchmarks,
	"llcpp":     gidlllcpp.GenerateBenchmarks,
	"hlcpp":     gidlhlcpp.GenerateBenchmarks,
	"rust":      gidlrust.GenerateBenchmarks,
	"dart":      gidldart.GenerateBenchmarks,
	"reference": gidlreference.GenerateBenchmarks,
	"walker":    gidlwalker.GenerateBenchmarks,
}

var allGenerators = map[string]map[string]Generator{
	"conformance": conformanceGenerators,
	"benchmark":   benchmarkGenerators,
}

var allGeneratorTypes = func() []string {
	var list []string
	for generatorType := range allGenerators {
		list = append(list, generatorType)
	}
	sort.Strings(list)
	return list
}()

var allLanguages = func() []string {
	var list []string
	seen := make(map[string]struct{})
	for _, generatorMap := range allGenerators {
		for language := range generatorMap {
			if _, ok := seen[language]; !ok {
				seen[language] = struct{}{}
				list = append(list, language)
			}
		}
	}
	sort.Strings(list)
	return list
}()

var allWireFormats = []gidlir.WireFormat{
	gidlir.V1WireFormat,
}

// GIDLFlags stores the command-line flags for the GIDL program.
type GIDLFlags struct {
	JSONPath *string
	Language *string
	Type     *string
	// TODO(fxbug.dev/52371) It should not be necessary to specify the number of generated files.
	NumOutputFiles             *int
	Out                        *string
	RustBenchmarksFidlLibrary  *string
	CppBenchmarksFidlLibrary   *string
	FuzzerCorpusHostDir        *string
	FuzzerCorpusPackageDataDir *string
}

// Valid indicates whether the parsed Flags are valid to be used.
func (gidlFlags GIDLFlags) Valid() bool {
	return len(*gidlFlags.JSONPath) != 0 && flag.NArg() != 0
}

var flags = GIDLFlags{
	JSONPath: flag.String("json", "",
		"relative path to the FIDL intermediate representation."),
	Language: flag.String("language", "",
		fmt.Sprintf("target language (%s)", strings.Join(allLanguages, "/"))),
	Type: flag.String("type", "", fmt.Sprintf("output type (%s)", strings.Join(allGeneratorTypes, "/"))),
	Out:  flag.String("out", "", "path to write output to"),
	RustBenchmarksFidlLibrary: flag.String("rust-benchmarks-fidl-library", "",
		"name for the fidl library used in the rust benchmarks"),
	CppBenchmarksFidlLibrary: flag.String("cpp-benchmarks-fidl-library", "",
		"name for the fidl library used in the cpp benchmarks"),
	FuzzerCorpusHostDir: flag.String("fuzzer-corpus-host-dir", "",
		"output directory for fuzzer_corpus"),
	FuzzerCorpusPackageDataDir: flag.String("fuzzer-corpus-package-data-dir", "",
		"directory to which fuzzer_corpus output files are mapped in their fuchsia package's data directory"),
}

func parseGidlIr(filename string) gidlir.All {
	f, err := os.Open(filename)
	if err != nil {
		panic(err)
	}
	config := gidlparser.Config{
		Languages:   allLanguages,
		WireFormats: allWireFormats,
	}
	result, err := gidlparser.NewParser(filename, f, config).Parse()
	if err != nil {
		panic(err)
	}
	return result
}

func parseFidlJSONIr(filename string) fidlgen.Root {
	bytes, err := ioutil.ReadFile(filename)
	if err != nil {
		panic(err)
	}
	var result fidlgen.Root
	if err := json.Unmarshal(bytes, &result); err != nil {
		panic(err)
	}
	return result
}

func main() {
	flag.Parse()

	if !flag.Parsed() || !flags.Valid() {
		flag.PrintDefaults()
		os.Exit(1)
	}

	var config gidlconfig.GeneratorConfig
	if *flags.RustBenchmarksFidlLibrary != "" {
		config.RustBenchmarksFidlLibrary = *flags.RustBenchmarksFidlLibrary
	}
	if *flags.CppBenchmarksFidlLibrary != "" {
		config.CppBenchmarksFidlLibrary = *flags.CppBenchmarksFidlLibrary
	}
	if *flags.FuzzerCorpusHostDir != "" {
		config.FuzzerCorpusHostDir = *flags.FuzzerCorpusHostDir
	}
	if *flags.FuzzerCorpusPackageDataDir != "" {
		config.FuzzerCorpusPackageDataDir = *flags.FuzzerCorpusPackageDataDir
	}

	ir := parseFidlJSONIr(*flags.JSONPath)

	var parsedGidlFiles []gidlir.All
	for _, path := range flag.Args() {
		parsedGidlFiles = append(parsedGidlFiles, parseGidlIr(path))
	}
	gidl := gidlir.FilterByBinding(gidlir.Merge(parsedGidlFiles), *flags.Language)

	// For simplicity, we do not allow FIDL that GIDL depends on to have
	// dependent libraries. This makes it much simpler to have everything
	// in the IR, and avoid cross-references.

	// TODO(fxbug.dev/7802): While transitioning "zx" from [Internal] to a normal
	// library, tolerate but ignore a dependency on zx.
	if len(ir.Libraries) == 1 && ir.Libraries[0].Name == "zx" {
		ir.Libraries = make([]fidlgen.Library, 0)
	}

	if len(ir.Libraries) != 0 {
		var libs []string
		for _, l := range ir.Libraries {
			libs = append(libs, string(l.Name))
		}
		panic(fmt.Sprintf(
			"GIDL does not work with FIDL libraries with dependents, found: %s",
			strings.Join(libs, ",")))
	}

	language := *flags.Language
	if language == "" {
		panic("must specify --language")
	}

	gidlir.ValidateAllType(gidl, *flags.Type)
	generatorMap, ok := allGenerators[*flags.Type]
	if !ok {
		panic(fmt.Sprintf("unknown generator type: %s", *flags.Type))
	}
	generator, ok := generatorMap[language]
	if !ok {
		log.Fatalf("unknown language for %s: %s", *flags.Type, language)
	}

	mainFile, err := generator(gidl, ir, config)
	if err != nil {
		log.Fatal(err)
	}

	if *flags.Out == "" {
		log.Fatalf("no -out path specified for main file")
	}
	if err := ioutil.WriteFile(*flags.Out, mainFile, 0666); err != nil {
		log.Fatal(err)
	}
}
