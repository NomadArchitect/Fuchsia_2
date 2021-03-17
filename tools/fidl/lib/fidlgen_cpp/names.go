// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fidlgen_cpp

import (
	"fmt"
	"os"
	"runtime/debug"
	"strings"

	fidl "go.fuchsia.dev/fuchsia/tools/fidl/lib/fidlgen"
)

type variant string

const (
	noVariant      variant = ""
	naturalVariant variant = "natural"
	wireVariant    variant = "wire"
)

var currentVariant = noVariant

func UseNatural() string {
	currentVariant = naturalVariant
	return ""
}

func UseWire() string {
	currentVariant = wireVariant
	return ""
}

// Namespace represents a C++ namespace.
type Namespace []string

// Namespace is implemented to satisfy the Namespaced interface.
func (ns Namespace) Namespace() Namespace {
	return ns
}

// String returns the fully qualified namespace including leading ::.
func (ns Namespace) String() string {
	return "::" + strings.Join(ns, "::")
}

// Append returns a new namespace with an additional component.
func (ns Namespace) Append(part string) Namespace {
	newNs := make([]string, len(ns)+1)
	copy(newNs, ns)
	newNs[len(ns)] = part
	return Namespace(newNs)
}

// DropLastComponent returns a new namespace with the final component removed.
func (ns Namespace) DropLastComponent() Namespace {
	if len(ns) == 0 {
		panic("Can't drop the end of an empty namespace")
	}
	new := make([]string, len(ns)-1)
	copy(new, ns)
	return Namespace(new)
}

// name represents a declaration name within a namespace.
// In the case of a nested class all of the containing classes are part of the name.
type name []string

// String returns the name of the declaration within its namespace, including any nesting parent classes.
func (n name) String() string {
	return strings.Join(n, "::")
}

// Unqualified return the unqualified declaration name.
func (n name) Unqualified() string {
	return n[len(n)-1]
}

// Prepend returns a new name with a prefix prepended.
func (n name) Prepend(prefix string) name {
	var newName name = make([]string, len(n))
	copy(newName, n)
	newName[len(n)-1] = prefix + n[len(n)-1]
	return newName
}

// Append returns a new name with a suffix appended.
func (n name) Append(suffix string) name {
	var newName name = make([]string, len(n))
	copy(newName, n)
	newName[len(n)-1] = n[len(n)-1] + suffix
	return newName
}

// Nest returns a new name for a class nested inside the existing name.
func (n name) Nest(nested string) name {
	return name(append(n, nested))
}

// DeclVariant represents the name of a C++ declaration within a namespace.
type DeclVariant struct {
	name      name
	namespace Namespace
}

// NewDeclVariant creates a new DeclVariant with a name and a namespace.
func NewDeclVariant(n string, namespace Namespace) DeclVariant {
	return DeclVariant{name: name([]string{n}), namespace: namespace}
}

// String returns the fully qualified name including leading ::, namespace and nested classes.
func (d DeclVariant) String() string {
	return d.namespace.String() + "::" + d.name.String()
}

// Name returns the name within the namespace, including nested classes.
func (d DeclVariant) Name() string {
	return d.name.String()
}

// Unqualified return the unqualified declaration name.
func (d DeclVariant) Unqualified() string {
	return d.name.Unqualified()
}

// Namespace returns the DeclVariant's namespace.
func (d DeclVariant) Namespace() Namespace {
	return d.namespace
}

// Type returns a TypeVariant for this DeclVariant.
func (d DeclVariant) Type() TypeVariant {
	return TypeVariant(d.String())
}

// AppendName returns a new DeclVariant with an suffix appended to the name portion.
func (d DeclVariant) AppendName(suffix string) DeclVariant {
	return DeclVariant{
		name:      d.name.Append(suffix),
		namespace: d.namespace,
	}
}

// PrependName returns a new DeclVariant with an prefix prepended to the name portion.
func (d DeclVariant) PrependName(prefix string) DeclVariant {
	return DeclVariant{
		name:      d.name.Prepend(prefix),
		namespace: d.namespace,
	}
}

// AppendNamespace returns a new DeclVariant with an additional C++ namespace component appended.
func (d DeclVariant) AppendNamespace(part string) DeclVariant {
	return DeclVariant{
		name:      d.name,
		namespace: d.namespace.Append(part),
	}
}

// Nest returns a new DeclVariant for a class nested inside the existing declaration.
func (d DeclVariant) Nest(nested string) DeclVariant {
	return DeclVariant{
		name:      d.name.Nest(nested),
		namespace: d.namespace,
	}
}

type DeclName struct {
	Natural DeclVariant
	Wire    DeclVariant
}

// CommonDeclName returns a DeclName with the same DeclVariant for both Wire and Natural variants.
func CommonDeclName(decl DeclVariant) DeclName {
	return DeclName{
		Natural: decl,
		Wire:    decl,
	}
}

func (dn DeclName) String() string {
	switch currentVariant {
	case noVariant:
		fmt.Printf("Called DeclName.String() on %s/%s when currentVariant isn't set.\n", dn.Natural, dn.Wire)
		debug.PrintStack()
		os.Exit(1)
	case naturalVariant:
		return dn.Natural.String()
	case wireVariant:
		return dn.Wire.String()
	}
	panic("not reached")
}

func (dn DeclName) Name() string {
	switch currentVariant {
	case noVariant:
		fmt.Printf("Called DeclName.Name() on %s/%s when currentVariant isn't set.\n", dn.Natural, dn.Wire)
		debug.PrintStack()
		os.Exit(1)
	case naturalVariant:
		return dn.Natural.Name()
	case wireVariant:
		return dn.Wire.Name()
	}
	panic("not reached")
}

func (dn DeclName) Namespace() Namespace {
	switch currentVariant {
	case noVariant:
		fmt.Printf("Called DeclName.Namespace() on %s/%s when currentVariant isn't set.\n", dn.Natural, dn.Wire)
		debug.PrintStack()
		os.Exit(1)
	case naturalVariant:
		return dn.Natural.Namespace()
	case wireVariant:
		return dn.Wire.Namespace()
	}
	panic("not reached")
}

// TypeName turns a DeclName into a TypeName.
func (dn DeclName) TypeName() TypeName {
	return TypeName{
		Natural: dn.Natural.Type(),
		Wire:    dn.Wire.Type(),
	}
}

// AppendName returns a new DeclName with an suffix appended to the name portions.
func (dn DeclName) AppendName(suffix string) DeclName {
	return DeclName{
		Natural: dn.Natural.AppendName(suffix),
		Wire:    dn.Wire.AppendName(suffix),
	}
}

// PrependName returns a new DeclName with an prefix prepended to the name portions.
func (dn DeclName) PrependName(prefix string) DeclName {
	return DeclName{
		Natural: dn.Natural.PrependName(prefix),
		Wire:    dn.Wire.PrependName(prefix),
	}
}

// AppendNamespace returns a new DeclName with additional C++ namespace components appended.
func (dn DeclName) AppendNamespace(c string) DeclName {
	return DeclName{
		Natural: dn.Natural.AppendNamespace(c),
		Wire:    dn.Wire.AppendNamespace(c),
	}
}

// DeclVariantFunc is a function that operates over a DeclVariant.
type DeclVariantFunc func(DeclVariant) DeclVariant

func (dn DeclName) MapNatural(f DeclVariantFunc) DeclName {
	return DeclName{
		Natural: f(dn.Natural),
		Wire:    dn.Wire,
	}
}

func (dn DeclName) MapWire(f DeclVariantFunc) DeclName {
	return DeclName{
		Natural: dn.Natural,
		Wire:    f(dn.Wire),
	}
}

func (dn DeclName) Member(member string) MemberName {
	return MemberName{decl: dn, member: member}
}

type MemberName struct {
	decl   DeclName
	member string
}

func (mn MemberName) Decl() DeclName { return mn.decl }

func (mn MemberName) Name() string { return mn.member }

// TypeVariant is implemented by something that can be a type name for a particular binding style.
type TypeVariant string

// WithTemplate wraps a TypeVariant with a template application.
func (name TypeVariant) WithTemplate(template string) TypeVariant {
	return TypeVariant(fmt.Sprintf("%s<%s>", template, name))
}

// WithArrayTemplate wraps a TypeVariant with a template application that takes an integer.
func (name TypeVariant) WithArrayTemplate(template string, arg int) TypeVariant {
	return TypeVariant(fmt.Sprintf("%s<%s, %v>", template, name, arg))
}

// TypeName is the name of a type for wire and natural types.
type TypeName struct {
	Natural TypeVariant
	Wire    TypeVariant
}

func (tn TypeName) String() string {
	switch currentVariant {
	case noVariant:
		fmt.Printf("Called TypeName.String() on %s/%s when currentVariant isn't set.\n", tn.Natural, tn.Wire)
		debug.PrintStack()
		os.Exit(1)
	case naturalVariant:
		return string(tn.Natural)
	case wireVariant:
		return string(tn.Wire)
	}
	panic("not reached")
}

// TypeNameForHandle returns the C++ name for a handle type
func TypeNameForHandle(t fidl.HandleSubtype) TypeName {
	return CommonTypeName(TypeVariant(fmt.Sprintf("::zx::%s", t)))
}

// CommonTypeName returns a TypeName with same name for both natural and wire types.
func CommonTypeName(name TypeVariant) TypeName {
	return TypeName{
		Natural: name,
		Wire:    name,
	}
}

// PrimitiveTypeName returns a TypeName for a primitive type, common across all bindings.
func PrimitiveTypeName(primitive string) TypeName {
	return TypeName{
		Natural: TypeVariant(primitive),
		Wire:    TypeVariant(primitive),
	}
}

// WithTemplates wraps type names with template applications.
func (tn TypeName) WithTemplates(natural, wire string) TypeName {
	return TypeName{
		Natural: tn.Natural.WithTemplate(natural),
		Wire:    tn.Wire.WithTemplate(wire),
	}
}

// WithArrayTemplates wraps type names with templates applications that take integers.
func (tn TypeName) WithArrayTemplates(natural, wire string, arg int) TypeName {
	return TypeName{
		Natural: tn.Natural.WithArrayTemplate(natural, arg),
		Wire:    tn.Wire.WithArrayTemplate(wire, arg),
	}
}

// TypeVariantFunc is a function that operates over a TypeVariant.
type TypeVariantFunc func(TypeVariant) TypeVariant

func (tn TypeName) MapNatural(f TypeVariantFunc) TypeName {
	return TypeName{
		Natural: f(tn.Natural),
		Wire:    tn.Wire,
	}
}

func (tn TypeName) MapWire(f TypeVariantFunc) TypeName {
	return TypeName{
		Natural: tn.Natural,
		Wire:    f(tn.Wire),
	}
}
