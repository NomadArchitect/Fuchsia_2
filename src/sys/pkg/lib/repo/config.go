// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package repo

import (
	"encoding/json"
	"fmt"

	tuf_data "github.com/theupdateframework/go-tuf/data"
)

// Config is a struct that mirrors an associated FIDL table
// definition in //sdk/fidl/fuchsia.pkg/repo.fidl. Documentation is as in
// that file. Ditto for the types that comprise its definition.
//
// Keep these in sync with their repo.fidl counterparts as well as the custom
//
type Config struct {
	URL              string         `json:"repo_url"`
	RootKeys         []KeyConfig    `json:"root_keys"`
	Mirrors          []MirrorConfig `json:"mirrors"`
	RootVersion      uint32         `json:"root_version"`
	RootThreshold    uint32         `json:"root_threshold"`
	UpdatePackageURL string         `json:"update_package_url,omitempty"`
}

type MirrorConfig struct {
	URL       string `json:"mirror_url"`
	Subscribe bool   `json:"subscribe"`
	BlobURL   string `json:"blob_mirror_url,omitempty"`
}

type KeyConfig struct {
	// ED25519Key is a 32-byte, lowercase, hex-encoded key.
	ED25519Key string
}

// We replicate the serialization/deserialization logic given in
// //src/sys/pkg/lib/fidl-fuchsia-pkg-ext/src/repo.rs;
// Per this logic, we set the BlobURL field in MirrorConfig as omitempty (above),
// and give custom marshaling logic to the key config.
//

// This alias allows to make use of the default (un)marshalling logic of Config as we redefine it.
type config Config

func (cfg *Config) MarshalJSON() ([]byte, error) {
	cfg2 := config(*cfg)
	if cfg2.RootVersion == 0 {
		cfg2.RootVersion = 1
	}
	if cfg2.RootThreshold == 0 {
		cfg2.RootThreshold = 1
	}
	return json.Marshal(&cfg2)
}

func (cfg *Config) UnmarshalJSON(data []byte) error {
	var cfg2 config
	if err := json.Unmarshal(data, &cfg2); err != nil {
		return err
	}
	if cfg2.RootVersion == 0 {
		cfg2.RootVersion = 1
	}
	if cfg2.RootThreshold == 0 {
		cfg2.RootThreshold = 1
	}
	*cfg = Config(cfg2)
	return nil
}

type typeAndValue struct {
	Type  string `json:"type"`
	Value string `json:"value"`
}

func (key *KeyConfig) MarshalJSON() ([]byte, error) {
	return json.Marshal(&typeAndValue{
		Type:  tuf_data.KeyTypeEd25519,
		Value: key.ED25519Key,
	})
}

func (key *KeyConfig) UnmarshalJSON(data []byte) error {
	var tv typeAndValue
	if err := json.Unmarshal(data, &tv); err != nil {
		return err
	}

	switch tv.Type {
	case tuf_data.KeyTypeEd25519:
		key.ED25519Key = tv.Value
		return nil
	default:
		return fmt.Errorf("unexpected key type: %q", tv.Type)
	}
}

// GetRootKeys returns the list of public key config objects as read from the
// contents of a repository's root metadata file.
func GetRootKeys(root *tuf_data.Root) ([]KeyConfig, error) {
	var rootKeys []KeyConfig
	for _, k := range root.UniqueKeys()["root"] {
		v := k.Value.Public.String()
		var key KeyConfig
		switch k.Type {
		case tuf_data.KeyTypeEd25519:
			key.ED25519Key = v
		default:
			return nil, fmt.Errorf("unexpected key type: %q", k.Type)
		}
		rootKeys = append(rootKeys, key)
	}
	return rootKeys, nil
}
