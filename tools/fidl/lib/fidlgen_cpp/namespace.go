// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fidlgen_cpp

import (
	"fmt"
	"strings"

	fidl "go.fuchsia.dev/fuchsia/tools/fidl/lib/fidlgen"
)

// Namespaced is implemented by types that have a C++ namespace.
type Namespaced interface {
	Namespace() Namespace
}

// During template processing this holds the current namespace.
var currentNamespace Namespace

// EnsureNamespace changes the current namespace to the one supplied and
// returns the C++ code required to switch to that namespace.
func EnsureNamespace(arg interface{}) string {
	lines := []string{}

	newNamespace := []string{}
	switch v := arg.(type) {
	case Namespaced:
		newNamespace = []string(v.Namespace())
	case string:
		newNamespace = strings.Split(v, "::")
		for len(newNamespace) > 0 && newNamespace[0] == "" {
			newNamespace = newNamespace[1:]
		}
	default:
		panic(fmt.Sprintf("Unexpected %T argument to EnsureNamespace", arg))
	}

	// Copy the namespaces
	new := make([]string, len(newNamespace))
	copy(new, newNamespace)
	current := make([]string, len(currentNamespace))
	copy(current, currentNamespace)

	// Remove common prefix
	for len(new) > 0 && len(current) > 0 && new[0] == current[0] {
		new = new[1:]
		current = current[1:]
	}

	// Leave the current namespace
	for i := len(current) - 1; i >= 0; i-- {
		lines = append(lines, fmt.Sprintf("}  // namespace %s", current[i]))
	}

	// Enter thew new namespace
	for i := 0; i < len(new); i++ {
		lines = append(lines, fmt.Sprintf("namespace %s {", new[i]))
	}

	// Update the current namespace variable
	currentNamespace = Namespace(newNamespace)

	return strings.Join(lines, "\n")
}

// During template processing this holds the stack of pushed & popped templates
var namespaceStack = []Namespace{}

func PushNamespace() string {
	namespaceStack = append(namespaceStack, currentNamespace)
	return ""
}

func PopNamespace() string {
	last := len(namespaceStack) - 1
	ns := namespaceStack[last]
	namespaceStack = namespaceStack[:last]
	return EnsureNamespace(ns)
}

func EndOfFile() string {
	if len(namespaceStack) != 0 {
		panic("The namespace stack isn't empty, there's a PopNamespace missing somewhere")
	}
	return EnsureNamespace("::")
}

//
// Predefined namespaces
//

func wireNamespace(library fidl.LibraryIdentifier) Namespace {
	return unifiedNamespace(library).Append("wire")
}

func naturalNamespace(library fidl.LibraryIdentifier) Namespace {
	parts := libraryParts(library, changePartIfReserved)
	return Namespace(parts)
}

func formatLibrary(library fidl.LibraryIdentifier, sep string, identifierTransform identifierTransform) string {
	name := strings.Join(libraryParts(library, identifierTransform), sep)
	return changeIfReserved(fidl.Identifier(name))
}

func unifiedNamespace(library fidl.LibraryIdentifier) Namespace {
	return Namespace([]string{formatLibrary(library, "_", keepPartIfReserved)})
}
