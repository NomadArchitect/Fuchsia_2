// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package main

import (
	"bufio"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"io/ioutil"
	"log"
	"net/url"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"
)

type FileSystemSizes []FileSystemSize

type FileSystemSize struct {
	Name  string      `json:"name"`
	Value json.Number `json:"value"`
	Limit json.Number `json:"limit"`
}

type SizeLimits struct {
	// Specifies a size limit in bytes for ICU data files.
	ICUDataLimit json.Number `json:"icu_data_limit"`
	// Specifies a size limit in bytes for uncategorized packages.
	CoreLimit json.Number `json:"core_limit"`
	// Specifies the files that contribute to the ICU data limit.
	ICUData []string `json:"icu_data"`
	// Specifies the files that contributed to the distributed shared library
	// size limits.
	DistributedShlibs []string `json:"distributed_shlibs"`
	// Specifies the distributed shared library size limit in bytes.
	DistributedShlibsLimit json.Number `json:"distributed_shlibs_limit"`
	// Specifies a series of size components/categories with a struct describing each.
	Components []Component `json:"components"`
}

// Display prints the contents of node and its children into a string.  All
// strings are printed at the supplied indentation level.
type DisplayFn func(node *Node, level int) string

type Node struct {
	fullPath string
	size     int64
	copies   int64
	parent   *Node
	children map[string]*Node
	// display is a function used to print the contents of Node in a human-friendly way.
	// If unset, a default display function is used.
	display DisplayFn
}

type Component struct {
	Component string      `json:"component"`
	Limit     json.Number `json:"limit"`
	Src       []string    `json:"src"`
}

type ComponentSize struct {
	Size   int64 `json:"size"`
	Budget int64 `json:"budget"`
	nodes  []*Node
}

type Blob struct {
	dep  []string
	size int64
	name string
	hash string
}

type BlobFromSizes struct {
	SourcePath string `json:"source_path"`
	Merkle     string `json:"merkle"`
	Bytes      int    `json:"bytes"`
	Size       int    `json:"size"`
}

type BlobFromJSON struct {
	Merkle     string `json:"merkle"`
	Path       string `json:"path"`
	SourcePath string `json:"source_path"`
}

const (
	MetaFar             = "meta.far"
	PackageList         = "gen/build/images/blob.manifest.list"
	BlobsJSON           = "blobs.json"
	ConfigData          = "config-data"
	DataPrefix          = "data/"
	SizeCheckerJSON     = "size_checker.json"
	FileSystemSizesJSON = "filesystem_sizes.json"
)

func newDummyNode() *Node {
	return newNode("")
}

func newNode(p string) *Node {
	return &Node{
		fullPath: p,
		children: make(map[string]*Node),
	}
}

// newNodeWithDisplay creates a new Node, and supplies a custom function to be
// used to print its contents.
func newNodeWithDisplay(p string, display DisplayFn) *Node {
	n := newNode(p)
	n.display = display
	return n
}

// Breaks down the given path divided by "/" and updates the size of each node on the path with the
// size of the given blob.
func (root *Node) add(p string, blob *Blob) {
	fullPath := strings.Split(p, "/")
	curr := root
	var nodePath string
	// A blob may be used by many packages, so the size of each blob is the total space allocated to
	// that blob in blobfs.
	// We divide the size by the total number of packages that depend on it to get a rough estimate of
	// the size of the individual blob.
	size := blob.size / int64(len(blob.dep))
	curr.size += size
	for _, name := range fullPath {
		name = strings.TrimSuffix(name, ".meta")
		nodePath = filepath.Join(nodePath, name)
		if _, ok := curr.children[name]; !ok {
			target := newNode(nodePath)
			target.parent = curr
			curr.children[name] = target
		}
		curr = curr.children[name]
		curr.size += size
	}
}

// Finds the node whose fullPath matches the given path. The path is guaranteed to be unique as it
// corresponds to the filesystem of the build directory. Detaches the node from the tree.
func (root *Node) detachByPath(p string) *Node {
	fullPath := strings.Split(p, "/")
	curr := root

	for _, name := range fullPath {
		next := curr.children[name]
		if next == nil {
			return nil
		}
		curr = next
	}
	curr.detach()
	return curr
}

// Detach this subtree from its parent, removing the size of this subtree from
// the aggregate size in the parent.
func (root *Node) detach() {
	size := root.size
	curr := root
	for curr.parent != nil {
		curr.parent.size -= size
		curr = curr.parent
	}
	if root.parent != nil {
		delete(root.parent.children, filepath.Base(root.fullPath))
	}
	root.parent = nil
}

// Returns the only child of a node. Useful for finding the root node.
func (node *Node) getOnlyChild() (*Node, error) {
	if len(node.children) == 1 {
		for _, child := range node.children {
			return child, nil
		}
	}

	return nil, fmt.Errorf("this node does not contain a single child.")
}

// displayAsDefault returns a human-readable representation of the supplied
// node and its children, using level as the indentation level for display
// pretty-printing.
func displayAsDefault(node *Node, level int) string {
	var copies string
	if node.copies > 1 {
		copies = fmt.Sprintf("| %3d %-6s", node.copies, "copies")
	} else if node.copies == 1 {
		copies = fmt.Sprintf("| %3d %-6s", node.copies, "copy")
	}

	var path = strings.TrimPrefix(node.fullPath, "obj/")
	path = strings.TrimPrefix(path, "lib/")
	if level > 1 {
		path = filepath.Base(path)
	}
	var maxLen = 80 - 2*level
	if maxLen < 0 {
		maxLen = 0
	}
	var pathLength = len(path)
	if pathLength > maxLen {
		var startPos = pathLength - maxLen + 3
		if startPos > pathLength {
			startPos = pathLength
		}
		path = "..." + path[startPos:]
	}
	path = fmt.Sprintf("%s%s", strings.Repeat("  ", level), path)
	ret := fmt.Sprintf("%-80s | %10s %10s\n", path, formatSize(node.size), copies)

	// Iterate over the childen in a sorted order.
	keys := make([]string, 0, len(node.children))
	for k := range node.children {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	for _, k := range keys {
		n := node.children[k]
		ret += n.storageBreakdown(level + 1)
	}
	return ret
}

// displayAsBlob returns a human-readable representation of the supplied Node and
// its children, formatted suitably for a blob ID, at the given indentation
// level.
func displayAsBlob(node *Node, level int) string {
	nc := len(node.children)
	ret := fmt.Sprintf("%vBlob ID %v (%v reuses):\n", strings.Repeat("  ", level), node.fullPath, nc)

	// Iterate over the childen in a sorted order.
	keys := make([]string, 0, len(node.children))
	for k := range node.children {
		keys = append(keys, k)
	}
	sort.Strings(keys)

	for _, k := range keys {
		n := node.children[k]
		ret += n.storageBreakdown(level + 1)
	}
	return ret
}

// MetaRegex matches strings like: "/some/path/foo.meta/something_else" and
// grabs "foo" from them.
var metaRegex = regexp.MustCompile(`/([^/]+)\.meta/`)

// displayAsMeta returns a human-readable representation of the supplied Node and
// its children, formatted suitably as a metadata item, at the given indentation
// level.
func displayAsMeta(node *Node, level int) string {
	m := metaRegex.FindStringSubmatch(node.fullPath)
	var n string
	if len(m) == 0 || m[1] == "" {
		n = node.fullPath
	} else {
		n = m[1]
	}
	ret := fmt.Sprintf("%s%s\n", strings.Repeat("  ", level), n)
	// No children to iterate over.
	return ret
}

// storageBreakdown constructs a string including detailed storage
// consumption for the subtree of this node.
//
// `level` controls the indentation level to preserve hierarchy in the
// output.
func (node *Node) storageBreakdown(level int) string {
	if node.display == nil {
		// If the node has no custom display function.
		return displayAsDefault(node, level)
	}
	return node.display(node, level)
}

// Formats a given number into human friendly string representation of bytes, rounded to 2 decimal places.
func formatSize(sizeInBytes int64) string {
	sizeInMiB := float64(sizeInBytes) / (1024 * 1024)
	return fmt.Sprintf("%.2f MiB", sizeInMiB)
}

// Extract all the packages from a given blob.manifest.list and blobs.json.
// It also returns a map containing all blobs, with the merkle root as the key.
func extractPackages(buildDir, packageListFileName, blobsJSON string) (blobMap map[string]*Blob, packages []string, err error) {
	blobMap = make(map[string]*Blob)

	var merkleRootToSizeMap map[string]int64
	if merkleRootToSizeMap, err = openAndProcessBlobsJSON(filepath.Join(buildDir, blobsJSON)); err != nil {
		return
	}

	packageList, err := os.Open(filepath.Join(buildDir, packageListFileName))
	if err != nil {
		return
	}
	defer packageList.Close()

	packageListScanner := bufio.NewScanner(packageList)
	for packageListScanner.Scan() {
		pkg, err := openAndParseBlobsManifest(blobMap, merkleRootToSizeMap, buildDir, packageListScanner.Text())
		if err != nil {
			return blobMap, packages, err
		}

		packages = append(packages, pkg...)
	}

	return
}

// Opens a blobs.manifest file to populate the blob map and extract all meta.far blobs.
// We expect each entry of blobs.manifest to have the following format:
// `$MERKLE_ROOT=$PATH_TO_BLOB`
func openAndParseBlobsManifest(
	blobMap map[string]*Blob,
	merkleRootToSizeMap map[string]int64,
	buildDir, blobsManifestFileName string) ([]string, error) {
	blobsManifestFile, err := os.Open(filepath.Join(buildDir, blobsManifestFileName))
	if err != nil {
		return nil, err
	}
	defer blobsManifestFile.Close()

	return parseBlobsManifest(blobMap, merkleRootToSizeMap, blobsManifestFileName, blobsManifestFile), nil
}

// Similar to openAndParseBlobsManifest, except it doesn't throw an I/O error.
func parseBlobsManifest(
	merkleRootToBlobMap map[string]*Blob,
	merkleRootToSizeMap map[string]int64,
	blobsManifestFileName string, blobsManifestFile io.Reader) []string {
	packages := []string{}

	blobsManifestScanner := bufio.NewScanner(blobsManifestFile)
	for blobsManifestScanner.Scan() {
		temp := strings.Split(blobsManifestScanner.Text(), "=")
		merkleRoot := temp[0]
		fileName := temp[1]
		if blob, ok := merkleRootToBlobMap[merkleRoot]; !ok {
			blob = &Blob{
				dep:  []string{blobsManifestFileName},
				name: fileName,
				size: merkleRootToSizeMap[merkleRoot],
				hash: merkleRoot,
			}

			merkleRootToBlobMap[merkleRoot] = blob
			// This blob is a Fuchsia package.
			if strings.HasSuffix(fileName, MetaFar) {
				packages = append(packages, fileName)
			}
		} else {
			blob.dep = append(blob.dep, blobsManifestFileName)
		}
	}

	return packages
}

// Translates blobs.json into a map, with the key as the merkle root and the value as the size of
// that blob.
func openAndProcessBlobsJSON(blobsJSONFilePath string) (map[string]int64, error) {
	blobsJSONFile, err := os.Open(blobsJSONFilePath)
	if err != nil {
		return nil, err
	}
	defer blobsJSONFile.Close()

	return processBlobsJSON(blobsJSONFile)
}

func processBlobsJSON(blobsJSONFile io.Reader) (map[string]int64, error) {
	blobs := []BlobFromSizes{}
	if err := json.NewDecoder(blobsJSONFile).Decode(&blobs); err != nil {
		return nil, err
	}

	m := map[string]int64{}
	for _, b := range blobs {
		m[b.Merkle] = int64(b.Size)
	}
	return m, nil
}

type processingState struct {
	blobMap           map[string]*Blob
	icuDataMap        map[string]*Node
	distributedShlibs map[string]*Node
	root              *Node
}

// Process the packages extracted from blob.manifest.list and process the blobs.json file to build a
// tree of packages.
func openAndParseBlobsJSON(
	buildDir string,
	packages []string,
	state *processingState) error {
	absBuildDir, err := filepath.Abs(buildDir)
	if err != nil {
		return fmt.Errorf("could not find abs path of directory: %v: %w", buildDir, err)
	}
	for _, metaFar := range packages {
		// From the meta.far file, we can get the path to the blobs.json for that package.
		dir := filepath.Dir(metaFar)
		blobsJSON := filepath.Join(buildDir, dir, BlobsJSON)
		// We then parse the blobs.json
		blobs := []BlobFromJSON{}
		data, err := ioutil.ReadFile(blobsJSON)
		if err != nil {
			return fmt.Errorf(readError(blobsJSON, err))
		}
		if err := json.Unmarshal(data, &blobs); err != nil {
			return fmt.Errorf(unmarshalError(blobsJSON, err))
		}
		// Finally, we add the blob and the package to the tree.
		parseBlobsJSON(state, blobs, dir, absBuildDir)
	}
	return nil
}

// Similar to openAndParseBlobsJSON except it doesn't throw an I/O error.
func parseBlobsJSON(
	state *processingState,
	blobs []BlobFromJSON,
	pkgPath string,
	absBuildDir string) {
	for _, blob := range blobs {
		// If the blob is an ICU data file, we don't add it to the tree.
		// We check the path instead of the source path because prebuilt packages have hashes as the
		// source path for their blobs
		baseBlobFilepath := filepath.Base(blob.Path)

		var (
			// Node must always be a pointer stored in state.icuDataMap, or nil.
			node *Node
			ok   bool
		)
		if node, ok = state.icuDataMap[baseBlobFilepath]; ok {
			// The size of each blob is the total space occupied by the blob in blobfs (each blob may be
			// referenced several times by different packages). Therefore, once we have already add the
			// size, we should remove it from the map
			if state.blobMap[blob.Merkle] != nil {
				if node == nil {
					state.icuDataMap[baseBlobFilepath] = newNode(baseBlobFilepath)
					// Ensure that node remains a pointer into the map.
					node = state.icuDataMap[baseBlobFilepath]
				}
				node.size += state.blobMap[blob.Merkle].size
				node.copies += 1
				delete(state.blobMap, blob.Merkle)
			}
			// Save the full path of the ICU data file, so ICU data file
			// proliferation can be debugged.
			var blobNode *Node
			blobNode = node.children[blob.Merkle]
			if blobNode == nil {
				blobNode = newNodeWithDisplay(blob.Merkle, displayAsBlob)
				node.children[blob.Merkle] = blobNode
			}
			icuCopyNode := newNodeWithDisplay(blob.SourcePath, displayAsMeta)
			blobNode.children[blob.SourcePath] = icuCopyNode
		} else if node, ok = state.distributedShlibs[blob.Path]; ok {
			if state.blobMap[blob.Merkle] != nil {
				if node == nil {
					state.distributedShlibs[blob.Path] = newNode(blob.Path)
					node = state.distributedShlibs[blob.Path]
				}
				node.size += state.blobMap[blob.Merkle].size
				node.copies += 1
				delete(state.blobMap, blob.Merkle)
			}
		} else {
			var configPkgPath string
			if filepath.Base(pkgPath) == ConfigData && strings.HasPrefix(blob.Path, DataPrefix) {
				// If the package is config-data, we want to group the blobs by the name of the package
				// the config data is for.
				// By contract defined in config.gni, the path has the format 'data/$for_pkg/$outputs'
				configPkgPath = strings.TrimPrefix(blob.Path, DataPrefix)
			}
			state.root.add(filepath.Join(pkgPath, configPkgPath), state.blobMap[blob.Merkle])
		}
	}
}

func parseBlobfsBudget(buildDir, fileSystemSizesJSONFilename string) int64 {
	fileSystemSizesJSON := filepath.Join(buildDir, fileSystemSizesJSONFilename)
	fileSystemSizesJSONData, err := ioutil.ReadFile(fileSystemSizesJSON)
	if err != nil {
		log.Fatal(readError(fileSystemSizesJSON, err))
	}
	var fileSystemSizes = new(FileSystemSizes)
	if err := json.Unmarshal(fileSystemSizesJSONData, &fileSystemSizes); err != nil {
		log.Fatal(unmarshalError(fileSystemSizesJSON, err))
	}
	for _, fileSystemSize := range *fileSystemSizes {
		if fileSystemSize.Name == "blob/contents_size" {
			budget, err := fileSystemSize.Limit.Int64()
			if err != nil {
				log.Fatalf("Failed to parse %s as an int64: %s\n", fileSystemSize.Limit, err)
			}
			return budget
		}
	}
	return 0
}

// Processes the given sizeLimits and throws an error if any component in the sizeLimits is above its
// allocated space limit.
func parseSizeLimits(sizeLimits *SizeLimits, buildDir, packageList, blobsJSON string) map[string]*ComponentSize {
	outputSizes := map[string]*ComponentSize{}
	blobMap, packages, err := extractPackages(buildDir, packageList, blobsJSON)
	if err != nil {
		return outputSizes
	}

	// We create a set of ICU data filenames.
	icuDataMap := make(map[string]*Node)
	for _, icu_data := range sizeLimits.ICUData {
		icuDataMap[icu_data] = newNode(icu_data)
	}

	// We also create a map of paths that should be considered distributed shlibs.
	distributedShlibs := make(map[string]*Node)
	for _, path := range sizeLimits.DistributedShlibs {
		distributedShlibs[path] = newNode(path)
	}
	st := processingState{
		blobMap,
		icuDataMap,
		distributedShlibs,
		// The dummy node will have the root node as its only child.
		newDummyNode(),
	}
	// We process the meta.far files that were found in the blobs.manifest here.
	if err := openAndParseBlobsJSON(buildDir, packages, &st); err != nil {
		return outputSizes
	}

	var distributedShlibsNodes []*Node
	var totalDistributedShlibsSize int64
	for _, node := range st.distributedShlibs {
		totalDistributedShlibsSize += node.size
		distributedShlibsNodes = append(distributedShlibsNodes, node)
	}

	var icuDataNodes []*Node
	var totalIcuDataSize int64
	for _, node := range st.icuDataMap {
		totalIcuDataSize += node.size
		icuDataNodes = append(icuDataNodes, node)
	}

	var total int64
	root, err := st.root.getOnlyChild()
	if err != nil {
		return outputSizes
	}

	for _, component := range sizeLimits.Components {
		var size int64
		var nodes []*Node

		for _, src := range component.Src {
			if node := root.detachByPath(src); node != nil {
				nodes = append(nodes, node)
				size += node.size
			}
		}
		total += size
		budget, err := component.Limit.Int64()
		if err != nil {
			log.Fatalf("Failed to parse %s as an int64: %s\n", component.Limit, err)
		}

		// There is only ever one copy of Update ZBIs.
		if component.Component == "Update ZBIs" {
			budget /= 2
			size /= 2
		}
		outputSizes[component.Component] = &ComponentSize{
			Size:   size,
			Budget: budget,
			nodes:  nodes,
		}
	}

	ICUDataLimit, err := sizeLimits.ICUDataLimit.Int64()
	if err != nil {
		log.Fatalf("Failed to parse %s as an int64: %s\n", sizeLimits.ICUDataLimit, err)
	}
	const icuDataName = "ICU Data"
	outputSizes[icuDataName] = &ComponentSize{
		Size:   totalIcuDataSize,
		Budget: ICUDataLimit,
		nodes:  icuDataNodes,
	}

	CoreSizeLimit, err := sizeLimits.CoreLimit.Int64()
	if err != nil {
		log.Fatalf("Failed to parse %s as an int64: %s\n", sizeLimits.CoreLimit, err)
	}
	const coreName = "Core system+services"
	coreNodes := make([]*Node, 0)
	// `root` contains the leftover nodes that have not been detached by the path
	// filters.
	coreNodes = append(coreNodes, root)
	outputSizes[coreName] = &ComponentSize{
		Size:   root.size,
		Budget: CoreSizeLimit,
		nodes:  coreNodes,
	}

	if sizeLimits.DistributedShlibsLimit.String() != "" {
		DistributedShlibsSizeLimit, err := sizeLimits.DistributedShlibsLimit.Int64()
		if err != nil {
			log.Fatalf("Failed to parse %s as an int64: %s\n", sizeLimits.DistributedShlibsLimit, err)
		}

		const distributedShlibsName = "Distributed shared libraries"
		outputSizes[distributedShlibsName] = &ComponentSize{
			Size:   totalDistributedShlibsSize,
			Budget: DistributedShlibsSizeLimit,
			nodes:  distributedShlibsNodes,
		}
	}

	return outputSizes
}

func readError(file string, err error) string {
	return verbError("read", file, err)
}

func unmarshalError(file string, err error) string {
	return verbError("unmarshal", file, err)
}

func verbError(verb, file string, err error) string {
	return fmt.Sprintf("Failed to %s %s: %s", verb, file, err)
}

func writeOutputSizes(sizes map[string]*ComponentSize, outPath string) error {
	f, err := os.Create(outPath)
	if err != nil {
		return err
	}
	defer f.Close()

	encoder := json.NewEncoder(f)
	encoder.SetIndent("", "  ")
	simpleSizes := make(map[string]interface{})
	budgetSuffix := ".budget"
	// Owner/context links to provide shortcut to component specific size stats.
	ownerSuffix := ".owner"
	for name, cs := range sizes {
		simpleSizes[name] = cs.Size
		simpleSizes[name+budgetSuffix] = cs.Budget
		simpleSizes[name+ownerSuffix] = "http://go/fuchsia-size-stats/single_component/?f=component:in:" + url.QueryEscape(name)
	}
	if err := encoder.Encode(&simpleSizes); err != nil {
		log.Fatal("failed to encode simpleSizes: ", err)
	}
	return nil
}

func generateReport(outputSizes map[string]*ComponentSize, showBudgetOnly bool, ignorePerComponentBudget bool, blobFsBudget int64) (bool, string) {
	var overBudget = false
	var totalSize int64 = 0
	var totalBudget int64 = 0
	var totalRemaining int64 = 0
	var report strings.Builder
	componentNames := make([]string, 0, len(outputSizes))
	for componentName := range outputSizes {
		componentNames = append(componentNames, componentName)
	}
	sort.Strings(componentNames)
	report.WriteString("\n")
	report.WriteString(fmt.Sprintf("%-80s | %-10s | %-10s | %-10s\n", "Component", "Size", "Budget", "Remaining"))
	report.WriteString(strings.Repeat("-", 119) + "\n")
	for _, componentName := range componentNames {
		var componentSize = outputSizes[componentName]
		var remainingBudget = componentSize.Budget - componentSize.Size
		var startColorCharacter string
		var endColorCharacter string

		// If any component is overbudget, then size_checker will fail.
		if componentSize.Size > componentSize.Budget && !ignorePerComponentBudget {
			overBudget = true
		}

		if showBudgetOnly {
			if componentSize.Size > componentSize.Budget {
				// Red
				startColorCharacter = "\033[31m"
			} else {
				// Green
				startColorCharacter = "\033[32m"
			}
			endColorCharacter = "\033[0m"
		}

		totalSize += componentSize.Size
		totalBudget += componentSize.Budget
		totalRemaining += remainingBudget
		report.WriteString(
			fmt.Sprintf("%-80s | %10s | %10s | %s%10s%s\n", componentName, formatSize(componentSize.Size), formatSize(componentSize.Budget), startColorCharacter, formatSize(remainingBudget), endColorCharacter))
		if !showBudgetOnly {
			for _, n := range componentSize.nodes {
				report.WriteString(n.storageBreakdown(1))
			}
			report.WriteString("\n")
		}

	}
	report.WriteString(strings.Repeat("-", 119) + "\n")

	report.WriteString(fmt.Sprintf("%-80s | %10s | %10s | %10s\n", "Total", formatSize(totalSize), formatSize(totalBudget), formatSize(totalRemaining)))
	report.WriteString(fmt.Sprintf("%-80s | %10s | %10s | %10s\n", "Allocated System Data Budget", formatSize(totalBudget), formatSize(blobFsBudget), formatSize(blobFsBudget-totalBudget)))

	if totalSize > blobFsBudget {
		report.WriteString(
			fmt.Sprintf("ERROR: Total data size [%s] exceeds total system data budget [%s]\n", formatSize(totalSize), formatSize(blobFsBudget)))
		overBudget = true
	}

	if totalBudget > blobFsBudget && !ignorePerComponentBudget {
		report.WriteString(
			fmt.Sprintf("WARNING: Total per-component data budget [%s] exceeds total system data budget [%s]\n", formatSize(totalBudget), formatSize(blobFsBudget)))
		overBudget = true
	}

	return overBudget, report.String()
}

func main() {
	flag.Usage = func() {
		fmt.Fprintln(os.Stderr, `Usage: size_checker [--budget-only] [--ignore-per-component-budget] --build-dir BUILD_DIR [--sizes-json-out SIZES_JSON]

A executable that checks if any component from a build has exceeded its allocated space limit.

See //tools/size_checker for more details.`)
		flag.PrintDefaults()
	}
	var buildDir string
	flag.StringVar(&buildDir, "build-dir", "", `(required) the output directory of a Fuchsia build.`)
	var fileSizeOutPath string
	flag.StringVar(&fileSizeOutPath, "sizes-json-out", "", "If set, will write a json object to this path with schema { <name (str)>: <file size (int)> }.")
	var showBudgetOnly bool
	flag.BoolVar(&showBudgetOnly, "budget-only", false, "If set, only budgets and total sizes of components will be shown.")
	var ignorePerComponentBudget bool
	flag.BoolVar(&ignorePerComponentBudget, "ignore-per-component-budget", false,
		"If set, output will go to stderr only if the total size of components exceeds the total blobFs budget.")

	flag.Parse()

	if buildDir == "" {
		flag.Usage()
		os.Exit(2)
	}

	sizeCheckerJSON := filepath.Join(buildDir, SizeCheckerJSON)
	sizeCheckerJSONData, err := ioutil.ReadFile(sizeCheckerJSON)
	if err != nil {
		log.Fatal(readError(sizeCheckerJSON, err))
	}
	var sizeLimits SizeLimits
	if err := json.Unmarshal(sizeCheckerJSONData, &sizeLimits); err != nil {
		log.Fatal(unmarshalError(sizeCheckerJSON, err))
	}
	// If there are no components, then there are no work to do. We are done.
	if len(sizeLimits.Components) == 0 {
		os.Exit(0)
	}

	outputSizes := parseSizeLimits(&sizeLimits, buildDir, PackageList, BlobsJSON)
	if len(fileSizeOutPath) > 0 {
		if err := writeOutputSizes(outputSizes, fileSizeOutPath); err != nil {
			log.Fatal(err)
		}
	}

	blobFsBudget := parseBlobfsBudget(buildDir, FileSystemSizesJSON)
	overBudget, report := generateReport(outputSizes, showBudgetOnly, ignorePerComponentBudget, blobFsBudget)

	if overBudget {
		log.Fatal(report)
	} else {
		log.Println(report)
	}
}
