// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package testsharder

import (
	"container/heap"
	"context"
	"encoding/json"
	"fmt"
	"hash/fnv"
	"math"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"time"

	"go.fuchsia.dev/fuchsia/tools/build"
	"go.fuchsia.dev/fuchsia/tools/lib/logger"
	"go.fuchsia.dev/fuchsia/tools/testing/runtests"
)

const (
	// The maximum number of runs that testsharder will calculate for a multiplied
	// test if totalRuns is unset.
	// TODO(olivernewman): Apply a maximum to user-specified values too, but
	// probably not in testsharder since we'll want to get feedback to users ASAP if
	// validation fails.
	multipliedTestMaxRuns = 1000

	// The maximum number of tests that a multiplier can match. testsharder will
	// fail if this is exceeded.
	maxMatchesPerMultiplier = 5

	// The prefix added to the names of shards that run affected tests.
	AffectedShardPrefix = "affected:"

	// The prefix added to the names of shards that run multiplied tests.
	MultipliedShardPrefix = "multiplied:"

	// The prefix added to the names of shards that run unaffected tests.
	UnaffectedShardPrefix = "unaffected:"

	// The name of the key of the expected duration test tag.
	expectedDurationTagKey = "expected_duration_milliseconds"
)

// MultiplyShards will return an error that unwraps to this if a multiplier's
// "name" field does not compile to a valid regex.
var errInvalidMultiplierRegex = fmt.Errorf("invalid multiplier regex")

// MultiplyShards will return an error that unwraps to this if a multiplier
// matches too many tests.
var errTooManyMultiplierMatches = fmt.Errorf("a multiplier cannot match more than %d tests", maxMatchesPerMultiplier)

// ApplyModifiers will return an error that unwraps to this if multiple default
// test modifiers are provided.
var errMultipleDefaultModifiers = fmt.Errorf("too many default modifiers, only one is allowed")

// MarkShardsSkipped will return an error that unwraps to this if the shards
// contain an affected test.
var errSkippedAffectedTest = fmt.Errorf("attempted to skip an affected test")

func ExtractDeps(shards []*Shard, fuchsiaBuildDir string) error {
	for _, shard := range shards {
		if err := extractDepsFromShard(shard, fuchsiaBuildDir); err != nil {
			return err
		}
	}
	return nil
}

func extractDepsFromShard(shard *Shard, fuchsiaBuildDir string) error {
	var shardDeps []string
	for i := range shard.Tests {
		test, deps, err := extractDepsFromTest(shard.Tests[i], fuchsiaBuildDir)
		if err != nil {
			return err
		}
		// extractDepsFromTest may modify the test, so we need to overwrite the
		// entry.
		shard.Tests[i] = test
		// Any test that doesn't run on Fuchsia is invoked via an executable in
		// the build out directory. The executable itself needs to be copied to
		// the testing bot along with the test's deps.
		if test.OS != "fuchsia" && test.Path != "" {
			deps = append(deps, test.Path)
		}
		shardDeps = append(shardDeps, deps...)
	}
	shard.AddDeps(shardDeps)
	return nil
}

func extractDepsFromTest(test Test, fuchsiaBuildDir string) (Test, []string, error) {
	if test.RuntimeDepsFile == "" {
		return test, nil, nil
	}
	path := filepath.Join(fuchsiaBuildDir, test.RuntimeDepsFile)
	f, err := os.Open(path)
	if err != nil {
		return test, nil, err
	}
	defer f.Close()
	// RuntimeDepsFile is no longer needed at this point and clutters the output.
	test.RuntimeDepsFile = ""
	var deps []string
	err = json.NewDecoder(f).Decode(&deps)
	return test, deps, err
}

// MultiplyShards appends new shards to shards where each new shard contains one test
// repeated multiple times according to the specifications in multipliers.
// It also removes all multiplied tests from the input shards.
func MultiplyShards(
	ctx context.Context,
	shards []*Shard,
	multipliers []TestModifier,
	testDurations TestDurationsMap,
	// TODO(olivernewman): Use the adjusted target duration calculated by
	// WithTargetDuration instead of the original target duration.
	targetDuration time.Duration,
	targetTestCount int,
) ([]*Shard, error) {
	var tooManyMatchesMultipliers []string
	for _, multiplier := range multipliers {
		if multiplier.TotalRuns < 0 {
			continue
		}
		type multiplierMatch struct {
			shardIdx int
			test     Test
			testIdx  int
		}
		var exactMatches []*multiplierMatch
		var regexMatches []*multiplierMatch

		nameRegex, err := regexp.Compile(multiplier.Name)
		if err != nil {
			return nil, fmt.Errorf("%w %q: %s", errInvalidMultiplierRegex, multiplier.Name, err)
		}

		for si, shard := range shards {
			for ti, test := range shard.Tests {
				// An empty OS matches all OSes.
				if multiplier.OS != "" && multiplier.OS != test.OS {
					continue
				}

				match := &multiplierMatch{shardIdx: si, test: test, testIdx: ti}
				if multiplier.Name == test.Name {
					exactMatches = append(exactMatches, match)
				} else if nameRegex.FindString(test.Name) != "" {
					regexMatches = append(regexMatches, match)
				} else {
					continue
				}

				if multiplier.TotalRuns > 0 {
					match.test.Runs = multiplier.TotalRuns
				} else if targetDuration > 0 {
					// We both cap the number of runs and apply a safety factor because
					// we want to keep the total runs to a reasonable number
					// in case the test takes longer than expected. We're conservative because
					// if a shard exceeds its timeout, it's really painful for users.
					expectedDuration := testDurations.Get(test).MedianDuration
					match.test.Runs = min(
						int(float64(targetDuration)/float64(expectedDuration)),
						multipliedTestMaxRuns,
					)
					if match.test.Runs == 0 {
						// In the case where the TotalRuns is not set and the targetDuration is
						// less than the expectedDuration, the calculated runs will be 0, but we
						// still want to run the test so we should set the runs to 1.
						match.test.Runs = 1
					}
				} else if targetTestCount > 0 {
					match.test.Runs = targetTestCount
				} else {
					match.test.Runs = 1
				}
				match.test.RunAlgorithm = StopOnFailure
				match.test.StopRepeatingAfterSecs = int(targetDuration.Seconds())
			}
		}

		// We'll consider partial regex matches only when we have no exact
		// matches.
		matches := exactMatches
		if len(matches) == 0 {
			matches = regexMatches
		}

		if len(matches) > maxMatchesPerMultiplier {
			tooManyMatchesMultipliers = append(tooManyMatchesMultipliers, multiplier.Name)
			logger.Errorf(ctx, "Multiplier %q matches too many tests (%d), maximum is %d",
				multiplier.Name, len(matches), maxMatchesPerMultiplier)
			continue
		} else if len(tooManyMatchesMultipliers) > 0 {
			// If we've already failed validation of a previous multiplier,
			// then we're bound to fail so there's no use constructing the
			// shard. Just make sure that we proceed to emit logs for other
			// multipliers that match too many tests.
			continue
		}

		shardIdxToTestIdx := make([][]int, len(shards))
		for _, m := range matches {
			// If a test is multiplied, it doesn't matter if it was originally
			// in an affected shard or not and it's confusing for it to have
			// two prefixes. So don't include the "affected" prefix.
			shardName := strings.TrimPrefix(shards[m.shardIdx].Name, AffectedShardPrefix)
			// If the test was already multiplied by another modifier, keep the same
			// multiplier shard but prefer the lower number of runs.
			alreadyMultiplied := strings.HasPrefix(shardName, MultipliedShardPrefix)
			if alreadyMultiplied {
				test := shards[m.shardIdx].Tests[m.testIdx]
				shards[m.shardIdx].Tests[m.testIdx].Runs = min(test.Runs, m.test.Runs)
			} else {
				shards = append(shards, &Shard{
					Name:  MultipliedShardPrefix + shardName + "-" + normalizeTestName(m.test.Name),
					Tests: []Test{m.test},
					Env:   shards[m.shardIdx].Env,
				})
				shardIdxToTestIdx[m.shardIdx] = append(shardIdxToTestIdx[m.shardIdx], m.testIdx)
			}
		}
		// Remove the multiplied tests from the other shard. It's wasteful to run it in
		// different shards.
		shrunkShards := 0
		for si, tis := range shardIdxToTestIdx {
			shard := shards[si-shrunkShards]
			// If entire shard is empty, remove it.
			if len(tis) == len(shard.Tests) {
				copy(shards[si-shrunkShards:], shards[si-shrunkShards+1:])
				shards = shards[:len(shards)-1]
				shrunkShards++
				continue
			}
			// Remove individual tests from the shard.
			shrunk := 0
			for _, ti := range tis {
				copy(shard.Tests[ti-shrunk:], shard.Tests[ti-shrunk+1:])
				shard.Tests = shard.Tests[:len(shard.Tests)-1]
				shrunk++
			}
		}
	}
	if len(tooManyMatchesMultipliers) > 0 {
		return nil, fmt.Errorf(
			"%d multiplier(s) match too many tests: %w",
			len(tooManyMatchesMultipliers), errTooManyMultiplierMatches,
		)
	}

	return shards, nil
}

// AddExpectedDurationTags uses the given TestDurations to annotate each test
// with an expected duration tag.
func AddExpectedDurationTags(shards []*Shard, testDurations TestDurationsMap) []*Shard {
	for _, shard := range shards {
		var newTests []Test
		for _, test := range shard.Tests {
			td := testDurations.Get(test)
			test.Tags = append(test.Tags, build.TestTag{
				Key:   expectedDurationTagKey,
				Value: strconv.FormatInt(td.MedianDuration.Milliseconds(), 10),
			})
			newTests = append(newTests, test)
		}
		shard.Tests = newTests
	}
	return shards
}

// ApplyModifiers applies the given test modifiers to tests in the given shards.
func ApplyModifiers(shards []*Shard, modTests []TestModifier) ([]*Shard, error) {
	defaultModTest := TestModifier{}
	foundDefault := false
	for _, modTest := range modTests {
		if modTest.Name == "*" {
			if foundDefault {
				return nil, errMultipleDefaultModifiers
			}
			defaultModTest = modTest
			foundDefault = true
		}
	}

	for _, shard := range shards {
		var modifiedTests []Test
		for _, test := range shard.Tests {
			test.applyModifier(defaultModTest)
			for _, modTest := range modTests {
				if modTest.Name == test.Name {
					test.applyModifier(modTest)
				}
			}
			modifiedTests = append(modifiedTests, test)
		}
		shard.Tests = modifiedTests
	}
	return shards, nil
}

// PartitionShards splits a set of shards in two using the given partition
// function.
func PartitionShards(shards []*Shard, partitionFunc func(Test) bool, prefix string) ([]*Shard, []*Shard) {
	var matchingShards []*Shard
	var nonmatchingShards []*Shard
	for _, shard := range shards {
		var matching []Test
		var nonmatching []Test
		for _, test := range shard.Tests {
			if partitionFunc(test) {
				matching = append(matching, test)
			} else {
				nonmatching = append(nonmatching, test)
			}
		}
		if len(matching) > 0 {
			matchingShards = append(matchingShards, &Shard{
				Name:  prefix + shard.Name,
				Tests: matching,
				Env:   shard.Env,
			})
		}
		if len(nonmatching) > 0 {
			shard.Tests = nonmatching
			nonmatchingShards = append(nonmatchingShards, shard)
		}
	}
	return matchingShards, nonmatchingShards
}

// MarkShardsSkipped marks the entire set of shards skipped.
func MarkShardsSkipped(shards []*Shard) ([]*Shard, error) {
	var newShards []*Shard
	for _, shard := range shards {
		var summary runtests.TestSummary
		for _, test := range shard.Tests {
			if test.Affected {
				return nil, errSkippedAffectedTest
			}
			summary.Tests = append(summary.Tests, runtests.TestDetails{
				Name:    test.Name,
				GNLabel: test.Label,
				Result:  runtests.TestSkipped,
				Tags:    test.Tags,
			})
		}
		newShards = append(newShards, &Shard{
			Name:    shard.Name,
			Tests:   shard.Tests,
			Env:     shard.Env,
			Summary: summary,
		})
	}
	return newShards, nil
}

func min(a, b int) int {
	if a < b {
		return a
	}
	return b
}

func divRoundUp(a, b int) int {
	if a%b == 0 {
		return a / b
	}
	return (a / b) + 1
}

// WithTargetDuration returns a list of shards such that all shards are expected
// to complete in approximately `targetDuration` time.
// If targetDuration <= 0, just returns its input.
// Alternatively, accepts a `targetTestCount` argument for backwards compatibility.
//
// Each resulting shard will have a TimeoutSecs field populated dynamically
// based on the expected total runtime of its tests. The caller can choose to
// respect and enforce this timeout, or ignore it.
func WithTargetDuration(
	shards []*Shard,
	targetDuration time.Duration,
	targetTestCount,
	maxShardsPerEnvironment int,
	testDurations TestDurationsMap,
) []*Shard {
	if targetDuration <= 0 && targetTestCount <= 0 {
		return shards
	}
	if maxShardsPerEnvironment <= 0 {
		maxShardsPerEnvironment = math.MaxInt64
	}

	if targetDuration > 0 {
		for _, shard := range shards {
			var shardDuration time.Duration
			// If any single test is expected to take longer than `targetDuration`,
			// it's no use creating shards whose entire expected runtimes are
			// shorter than that one test. So in that case we use the longest test's
			// expected duration as the target duration.
			for _, t := range shard.Tests {
				duration := testDurations.Get(t).MedianDuration
				if duration > targetDuration {
					targetDuration = duration
				}
				shardDuration += duration * time.Duration(t.minRequiredRuns())
			}
			// If any environment would exceed the maximum shard count, then its
			// shard durations will exceed the specified target duration. So
			// increase the target duration accordingly for the other
			// environments.
			subShardCount := divRoundUp(int(shardDuration), int(targetDuration))
			if subShardCount > maxShardsPerEnvironment {
				targetDuration = time.Duration(divRoundUp(int(shardDuration), maxShardsPerEnvironment))
			}
		}
	}

	output := make([]*Shard, 0, len(shards))
	for _, shard := range shards {
		numNewShards := 0
		if targetDuration > 0 {
			var total time.Duration
			for _, t := range shard.Tests {
				total += testDurations.Get(t).MedianDuration * time.Duration(t.minRequiredRuns())
			}
			numNewShards = divRoundUp(int(total), int(targetDuration))
		} else {
			var total int
			for _, t := range shard.Tests {
				total += t.minRequiredRuns()
			}
			numNewShards = divRoundUp(total, targetTestCount)
		}
		if numNewShards == 0 {
			// If targetDuration is set but all durations are zero, we'll
			// determine that we need zero new shards. In this case, we'll be
			// careful and assume that we need the maximum allowed shards to be
			// able to fit all tests or one shard per test if the number of tests
			// is less than the maximum allowed shards.
			numNewShards = min(len(shard.Tests), maxShardsPerEnvironment)
		}
		numNewShards = min(numNewShards, maxShardsPerEnvironment)

		newShards := shardByTime(shard, testDurations, numNewShards)
		output = append(output, newShards...)
	}
	return output
}

type subshard struct {
	duration time.Duration
	tests    []Test
}

// A subshardHeap is a min heap of subshards, using subshard duration as the key
// to sort by.
// It implements heap.Interface.
type subshardHeap []subshard

func (h subshardHeap) Len() int {
	return len(h)
}

func (h subshardHeap) Less(i, j int) bool {
	if h[i].duration != h[j].duration {
		return h[i].duration < h[j].duration
	}
	// All durations being equal, fall back to comparing test counts. This
	// ensures that even if all expected durations are zero (which generally
	// shouldn't happen, but is possible), we'll still divide tests evenly by
	// test count across shards.
	return len(h[i].tests) < len(h[j].tests)
}

func (h subshardHeap) Swap(i, j int) {
	h[i], h[j] = h[j], h[i]
}

func (h *subshardHeap) Push(s interface{}) {
	*h = append(*h, s.(subshard))
}

func (h *subshardHeap) Pop() interface{} {
	old := *h
	n := len(old)
	s := old[n-1]
	*h = old[0 : n-1]
	return s
}

// shardByTime breaks a single original shard into numNewShards subshards such
// that each subshard has approximately the same expected total duration.
//
// It does this using a greedy approximation algorithm for static multiprocessor
// scheduling (https://en.wikipedia.org/wiki/Multiprocessor_scheduling). It
// first sorts the tests in descending order by expected duration and then
// successively allocates each test to the subshard with the lowest total
// expected duration so far.
//
// Within each returned shard, tests will be sorted pseudo-randomly.
func shardByTime(shard *Shard, testDurations TestDurationsMap, numNewShards int) []*Shard {
	sort.Slice(shard.Tests, func(index1, index2 int) bool {
		test1, test2 := shard.Tests[index1], shard.Tests[index2]
		duration1 := testDurations.Get(test1).MedianDuration
		duration2 := testDurations.Get(test2).MedianDuration
		if duration1 == duration2 {
			// Sort by name for tests of equal duration to ensure deterministic
			// ordering.
			return test1.Name < test2.Name
		}
		// "greater than" instead of "less than" to achieve descending ordering
		return duration1 > duration2
	})

	var h subshardHeap
	for i := 0; i < numNewShards; i++ {
		// Initialize each subshard to have an empty list of tests so that it will
		// be properly outputted in the json output file as a list instead of a nil
		// value if the shard ends up with zero tests.
		s := subshard{tests: []Test{}}
		h = append(h, s)
	}

	for _, test := range shard.Tests {
		runsPerShard := divRoundUp(test.minRequiredRuns(), numNewShards)
		extra := runsPerShard*numNewShards - test.minRequiredRuns()
		for i := 0; i < numNewShards; i++ {
			testCopy := test
			var runs int
			if i < numNewShards-extra {
				runs = runsPerShard
			} else {
				runs = runsPerShard - 1
			}
			if runs == 0 {
				break
			}
			// Only if the test must be run more than once should we split it
			// across multiple shards.
			if test.minRequiredRuns() > 1 {
				testCopy.Runs = runs
			}
			// Assign this test to the subshard with the lowest total expected
			// duration at this iteration of the for loop.
			ss := heap.Pop(&h).(subshard)
			ss.duration += testDurations.Get(test).MedianDuration * time.Duration(testCopy.minRequiredRuns())
			ss.tests = append(ss.tests, testCopy)
			heap.Push(&h, ss)
		}
	}

	// Sort the resulting shards by the basename of the first test. Otherwise,
	// changes to the input set of tests (adding, removing or renaming a test)
	// result in confusing reordering of the shard names. This ensures that a
	// given named shard (e.g. "QEMU-(1)") will contain roughly the same set of
	// longer-running tests across multiple builds, even if the input set of
	// tests changes. Shorter tests are more likely to be switched between
	// shards because we're sorting by the name of the longest test.
	sort.Slice(h, func(i, j int) bool {
		return h[i].tests[0].Name < h[j].tests[0].Name
	})

	newShards := make([]*Shard, 0, numNewShards)
	for i, subshard := range h {
		// If we left tests in descending order by duration, updates to
		// checked-in test durations could arbitrarily reorder tests, making
		// those updates more likely to break non-hermetic tests.
		// To avoid that, sort by a hash of the name so that similar tests (e.g.
		// all tests from one area of the codebase, or all host-side tests)
		// tests don't always run sequentially, since similar tests are more
		// likely to non-hermetically conflict with each other. Using a
		// deterministic hash ensures that given tests A and B in the same
		// shard, A will *always* run before B or vice versa.
		sort.Slice(subshard.tests, func(i, j int) bool {
			return hash(subshard.tests[i].Name) < hash(subshard.tests[j].Name)
		})
		name := shard.Name
		if numNewShards > 1 {
			name = fmt.Sprintf("%s-(%d)", shard.Name, i+1)
		}
		newShards = append(newShards, &Shard{
			Name:        name,
			Tests:       subshard.tests,
			Env:         shard.Env,
			TimeoutSecs: int(computeShardTimeout(subshard).Seconds()),
		})
	}
	return newShards
}

// computeShardTimeout dynamically calculates an appropriate timeout for the
// given shard based on the number of tests it runs and the expected duration of
// each test, with a large buffer to account for tests occasionally taking much
// longer than expected.
//
// The constants we use to compute the timeout are somewhat arbitrary and can be
// adjusted if necessary.
func computeShardTimeout(s subshard) time.Duration {
	// Allow for tests running longer than expected by a constant factor. It's
	// unlikely that *every* test would be this much slower than expected, but
	// it's conceivable that a significant number of tests could be so affected.
	perTestMultiplier := time.Duration(2)
	// Conservative estimated overhead for the shard independent of how many
	// tests the shard runs.
	shardOverhead := 10 * time.Minute
	// Conservative estimate of the overhead for running each test.
	perTestOverhead := time.Second

	testCount := time.Duration(len(s.tests))

	// Take into account the expected overhead if a test times out to make sure
	// that the shard timeout leaves time for individual tests to time out. This
	// is determined by taking the maximum test timeout minus the average
	// expected test runtime, and doubling it if the test is allowed to run
	// multiple times or if there are multiple tests.
	longestTimeoutTest := s.tests[0]
	for _, test := range s.tests {
		if test.Timeout > longestTimeoutTest.Timeout {
			longestTimeoutTest = test
		}
	}
	avgDuration := s.duration / testCount
	testTimeoutOverhead := longestTimeoutTest.Timeout - avgDuration
	if longestTimeoutTest.Runs > 1 || len(s.tests) > 1 {
		// Only add enough overhead to guarantee two test runs enough
		// time to time out. If we allowed enough overhead for *all*
		// tests to time out then the task timeout would be way too
		// high. If all tests are timing out we should cancel the shard
		// early rather than waiting for all the tests to finish, which
		// would be wasteful.
		testTimeoutOverhead *= 2
	}

	timeout := perTestMultiplier * (s.duration + testCount*perTestOverhead)
	if testTimeoutOverhead > 0 {
		timeout += testTimeoutOverhead
	}
	timeout += shardOverhead
	return timeout
}

func hash(s string) uint32 {
	h := fnv.New32a()
	h.Write([]byte(s))
	return h.Sum32()
}

// Removes leading slashes and replaces all other `/` with `_`. This allows the
// shard name to appear in filepaths.
func normalizeTestName(name string) string {
	trimmedName := strings.TrimLeft(name, "/")
	return strings.ReplaceAll(trimmedName, "/", "_")
}

// Applies the realm label to all tests on all shards provided.
func ApplyRealmLabel(shards []*Shard, realmLabel string) {
	for _, shard := range shards {
		for i := range shard.Tests {
			shard.Tests[i].RealmLabel = realmLabel
		}
	}
}

// ApplyTestTimeouts sets the timeout field on every test to the specified
// duration. Timeouts already declared for tests in tests.json take precedence.
func ApplyTestTimeouts(shards []*Shard, perTestTimeout time.Duration) {
	for _, shard := range shards {
		for i := range shard.Tests {
			testTimeoutSecs := shard.Tests[i].Test.TimeoutSecs
			if testTimeoutSecs == 0 {
				shard.Tests[i].Timeout = perTestTimeout
			} else {
				shard.Tests[i].Timeout = time.Duration(testTimeoutSecs) * time.Second
			}
		}
	}
}
