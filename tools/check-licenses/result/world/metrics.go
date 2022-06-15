// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package world

type MetricsInterface interface {
	Counts() map[string]int
	Values() map[string][]string
}

type ResultMetrics struct {
	counts map[string]int      `json:"counts"`
	values map[string][]string `json:"values"`
}

const (
	GnDesc      = "GN Desc"
	GnGen       = "GN Gen"
	X20Licenses = "X20 Licenses"
)

var Metrics *ResultMetrics

func init() {
	Metrics = &ResultMetrics{
		counts: make(map[string]int),
		values: make(map[string][]string),
	}
}

func plus1(key string) {
	Metrics.counts[key] = Metrics.counts[key] + 1
}

func plusVal(key string, val string) {
	plus1(key)
	Metrics.values[key] = append(Metrics.values[key], val)
}

func (m *ResultMetrics) Counts() map[string]int {
	return m.counts
}

func (m *ResultMetrics) Values() map[string][]string {
	return m.values
}
