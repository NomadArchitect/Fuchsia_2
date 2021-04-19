// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fidlgen_cpp

import (
	"bytes"
	"fmt"
	"sort"

	"go.fuchsia.dev/fuchsia/tools/fidl/lib/fidlgen"
)

type Attributes struct {
	fidlgen.Attributes
}

// Docs returns C++ documentation comments.
func (a Attributes) Docs() string {
	var buf bytes.Buffer
	for _, c := range a.DocComments() {
		buf.WriteString("\n///")
		buf.WriteString(c)
	}
	return buf.String()
}

type declKind namespacedEnumMember

type declKinds struct {
	Bits     declKind
	Const    declKind
	Enum     declKind
	Protocol declKind
	Service  declKind
	Struct   declKind
	Table    declKind
	Union    declKind
}

// Kinds are the different kinds of FIDL declarations. They are used in
// header/impl templates to select the correct decl-specific template.
var Kinds = namespacedEnum(declKinds{}).(declKinds)

// A Kinded value is a declaration in FIDL, for which we would like to
// generate some corresponding C++ code.
type Kinded interface {
	Kind() declKind
}

type familyKind namespacedEnumMember

type familyKinds struct {
	// TrivialCopy identifies values for whom a copy is trivial (like integers)
	TrivialCopy familyKind

	// Reference identifies values with a non trivial copy for which we use a
	// reference on the caller argument.
	Reference familyKind

	// String identifies string values for which we can use a const reference
	// and for which we can optimize the field construction.
	String familyKind

	// Vector identifies vector values for which we can use a reference and for
	// which we can optimize the field construction.
	Vector familyKind
}

// FamilyKinds are general categories identifying what operation we should use
// to pass a value without a move (LLCPP). It also defines the way we should
// initialize a field.
var FamilyKinds = namespacedEnum(familyKinds{}).(familyKinds)

type typeKind namespacedEnumMember

type typeKinds struct {
	Array     typeKind
	Vector    typeKind
	String    typeKind
	Handle    typeKind
	Request   typeKind
	Primitive typeKind
	Bits      typeKind
	Enum      typeKind
	Const     typeKind
	Struct    typeKind
	Table     typeKind
	Union     typeKind
	Protocol  typeKind
}

// TypeKinds are the kinds of C++ types (arrays, primitives, structs, ...).
var TypeKinds = namespacedEnum(typeKinds{}).(typeKinds)

type Type struct {
	nameVariants

	WirePointer bool

	// Defines what operation we should use to pass a value without a move (LLCPP). It also
	// defines the way we should initialize a field.
	WireFamily familyKind

	// NeedsDtor indicates whether this type needs to be destructed explicitely
	// or not.
	NeedsDtor bool

	Kind typeKind

	IsResource bool
	Nullable   bool

	DeclarationName fidlgen.EncodedCompoundIdentifier

	// Set iff IsArray || IsVector
	ElementType *Type
	// Valid iff IsArray
	ElementCount int
}

// IsPrimitiveType returns true if this type is primitive.
func (t *Type) IsPrimitiveType() bool {
	return t.Kind == TypeKinds.Primitive || t.Kind == TypeKinds.Bits || t.Kind == TypeKinds.Enum
}

// WireArgumentDeclaration returns the argument declaration for this type for the wire variant.
func (t *Type) WireArgumentDeclaration(n string) string {
	switch t.WireFamily {
	case FamilyKinds.TrivialCopy:
		return t.String() + " " + n
	case FamilyKinds.Reference, FamilyKinds.Vector:
		return t.String() + "& " + n
	case FamilyKinds.String:
		return "const " + t.String() + "& " + n
	default:
		panic(fmt.Sprintf("Unknown wire family kind %v", t.WireFamily))
	}
}

// WireInitMessage returns message field initialization for the wire variant.
func (t *Type) WireInitMessage(n string) string {
	switch t.WireFamily {
	case FamilyKinds.TrivialCopy:
		return fmt.Sprintf("%s(%s)", n, n)
	case FamilyKinds.Reference:
		return fmt.Sprintf("%s(std::move(%s))", n, n)
	case FamilyKinds.String:
		return fmt.Sprintf("%s(%s)", n, n)
	case FamilyKinds.Vector:
		return fmt.Sprintf("%s(%s)", n, n)
	default:
		panic(fmt.Sprintf("Unknown wire family kind %v", t.WireFamily))

	}
}

type Member interface {
	NameAndType() (string, Type)
}

type Root struct {
	Headers         []string
	HandleTypes     []string
	RawLibrary      fidlgen.LibraryIdentifier
	Library         fidlgen.LibraryIdentifier
	LibraryReversed fidlgen.LibraryIdentifier
	Decls           []Kinded
	HeaderOptions
}

// NaturalDomainObjectsHeader computes the path to #include the natural domain
// object header.
func (r Root) NaturalDomainObjectsHeader() string {
	if r.NaturalDomainObjectsIncludeStem == "" {
		fidlgen.TemplateFatalf("Natural domain objects include stem was missing")
	}
	return fmt.Sprintf("%s/%s.h", formatLibraryPath(r.RawLibrary), r.NaturalDomainObjectsIncludeStem)
}

// HlcppBindingsHeader computes the path to #include the high-level C++ bindings
// header.
func (r Root) HlcppBindingsHeader() string {
	if r.HlcppBindingsIncludeStem == "" {
		fidlgen.TemplateFatalf("High-level C++ bindings include stem was missing")
	}
	return fmt.Sprintf("%s/%s.h", formatLibraryPath(r.RawLibrary), r.HlcppBindingsIncludeStem)
}

// WireBindingsHeader computes the path to #include the wire bindings header.
func (r Root) WireBindingsHeader() string {
	if r.WireBindingsIncludeStem == "" {
		fidlgen.TemplateFatalf("Wire bindings include stem was missing")
	}
	return fmt.Sprintf("%s/%s.h", formatLibraryPath(r.RawLibrary), r.WireBindingsIncludeStem)
}

// HeaderOptions are independent from the FIDL library IR, but used in the generated
// code to properly #include their dependencies.
type HeaderOptions struct {
	// PrimaryHeader will be used as the path to #include the generated header.
	PrimaryHeader string

	// IncludeStem is the suffix after library path when referencing includes.
	// Includes will be of the form
	//     #include <fidl/library/name/{include-stem}.h>
	IncludeStem string

	// NaturalDomainObjectsIncludeStem is the file stem of the natural
	// domain object header, if it needs to be included by the generated code.
	NaturalDomainObjectsIncludeStem string

	// HlcppBindingsIncludeStem is the file stem of the high-level C++ bindings
	// header, if it needs to be included by the generated code.
	HlcppBindingsIncludeStem string

	// WireBindingsIncludeStem is the file stem of the wire bindings (LLCPP)
	// header, if it needs to be included by the generated code.
	WireBindingsIncludeStem string
}

// SingleComponentLibraryName returns if the FIDL library name only consists of
// a single identifier (e.g. "library foo;"). This is significant because the
// unified namespace and the natural namespace are identical when the library
// only has one component.
func (r Root) SingleComponentLibraryName() bool {
	return len(r.Library) == 1
}

// Result holds information about error results on methods.
type Result struct {
	ValueMembers    []Parameter
	ResultDecl      nameVariants
	ErrorDecl       nameVariants
	ValueDecl       name
	ValueStructDecl nameVariants
	ValueTupleDecl  name
}

func (r Result) ValueArity() int {
	return len(r.ValueMembers)
}

var primitiveTypes = map[fidlgen.PrimitiveSubtype]string{
	fidlgen.Bool:    "bool",
	fidlgen.Int8:    "int8_t",
	fidlgen.Int16:   "int16_t",
	fidlgen.Int32:   "int32_t",
	fidlgen.Int64:   "int64_t",
	fidlgen.Uint8:   "uint8_t",
	fidlgen.Uint16:  "uint16_t",
	fidlgen.Uint32:  "uint32_t",
	fidlgen.Uint64:  "uint64_t",
	fidlgen.Float32: "float",
	fidlgen.Float64: "double",
}

// NameVariantsForPrimitive returns the C++ name of a FIDL primitive type.
func NameVariantsForPrimitive(val fidlgen.PrimitiveSubtype) nameVariants {
	if t, ok := primitiveTypes[val]; ok {
		return primitiveNameVariants(t)
	}
	panic(fmt.Sprintf("unknown primitive type: %v", val))
}

type identifierTransform bool

const (
	keepPartIfReserved   identifierTransform = false
	changePartIfReserved identifierTransform = true
)

func libraryParts(library fidlgen.LibraryIdentifier, identifierTransform identifierTransform) []string {
	parts := []string{}
	for _, part := range library {
		if identifierTransform == changePartIfReserved {
			parts = append(parts, changeIfReserved(string(part), nsComponentContext))
		} else {
			parts = append(parts, string(part))
		}
	}
	return parts
}

func formatLibraryPrefix(library fidlgen.LibraryIdentifier) string {
	return formatLibrary(library, "_", keepPartIfReserved)
}

func formatLibraryPath(library fidlgen.LibraryIdentifier) string {
	return formatLibrary(library, "/", keepPartIfReserved)
}

func codingTableName(ident fidlgen.EncodedCompoundIdentifier) string {
	ci := fidlgen.ParseCompoundIdentifier(ident)
	return formatLibrary(ci.Library, "_", keepPartIfReserved) + "_" + string(ci.Name) + string(ci.Member)
}

type compiler struct {
	symbolPrefix    string
	decls           fidlgen.DeclInfoMap
	library         fidlgen.LibraryIdentifier
	handleTypes     map[fidlgen.HandleSubtype]struct{}
	resultForStruct map[fidlgen.EncodedCompoundIdentifier]*Result
	resultForUnion  map[fidlgen.EncodedCompoundIdentifier]*Result
}

func (c *compiler) isInExternalLibrary(ci fidlgen.CompoundIdentifier) bool {
	if len(ci.Library) != len(c.library) {
		return true
	}
	for i, part := range c.library {
		if ci.Library[i] != part {
			return true
		}
	}
	return false
}

func (c *compiler) compileNameVariants(eci fidlgen.EncodedCompoundIdentifier) nameVariants {
	ci := fidlgen.ParseCompoundIdentifier(eci)
	declInfo, ok := c.decls[ci.EncodeDecl()]
	if !ok {
		panic(fmt.Sprintf("unknown identifier: %v", eci))
	}
	ctx := declContext(declInfo.Type)
	name := ctx.transform(ci) // Note: does not handle ci.Member
	if len(ci.Member) == 0 {
		return name
	}

	member := memberNameContext(declInfo.Type).transform(ci.Member)
	return name.nestVariants(member)
}

func (c *compiler) compileCodingTableType(eci fidlgen.EncodedCompoundIdentifier) string {
	val := fidlgen.ParseCompoundIdentifier(eci)
	if c.isInExternalLibrary(val) {
		panic(fmt.Sprintf("can't create coding table type for external identifier: %v", val))
	}

	return fmt.Sprintf("%s_%sTable", c.symbolPrefix, val.Name)
}

func (c *compiler) compileType(val fidlgen.Type) Type {
	r := Type{}
	r.Nullable = val.Nullable
	switch val.Kind {
	case fidlgen.ArrayType:
		t := c.compileType(*val.ElementType)
		// Because the unified bindings alias types from the natural domain objects,
		// the name _transformation_ would be identical between natural and unified,
		// here and below. We reserve the flexibility to specify different names
		// in the future.
		r.nameVariants = nameVariants{
			Natural: makeName("std::array").arrayTemplate(t.Natural, *val.ElementCount),
			Unified: makeName("std::array").arrayTemplate(t.Unified, *val.ElementCount),
			Wire:    makeName("fidl::Array").arrayTemplate(t.Wire, *val.ElementCount),
		}
		r.WirePointer = t.WirePointer
		r.WireFamily = FamilyKinds.Reference
		r.NeedsDtor = true
		r.Kind = TypeKinds.Array
		r.IsResource = t.IsResource
		r.ElementType = &t
		r.ElementCount = *val.ElementCount
	case fidlgen.VectorType:
		t := c.compileType(*val.ElementType)
		if val.Nullable {
			r.nameVariants.Natural = makeName("fidl::VectorPtr").template(t.Natural)
			r.nameVariants.Unified = makeName("fidl::VectorPtr").template(t.Unified)
		} else {
			r.nameVariants.Natural = makeName("std::vector").template(t.Natural)
			r.nameVariants.Unified = makeName("std::vector").template(t.Unified)
		}
		r.nameVariants.Wire = makeName("fidl::VectorView").template(t.Wire)
		r.WireFamily = FamilyKinds.Vector
		r.WirePointer = t.WirePointer
		r.NeedsDtor = true
		r.Kind = TypeKinds.Vector
		r.IsResource = t.IsResource
		r.ElementType = &t
	case fidlgen.StringType:
		if val.Nullable {
			r.Natural = makeName("fidl::StringPtr")
		} else {
			r.Natural = makeName("std::string")
		}
		r.Unified = r.Natural
		r.Wire = makeName("fidl::StringView")
		r.WireFamily = FamilyKinds.String
		r.NeedsDtor = true
		r.Kind = TypeKinds.String
	case fidlgen.HandleType:
		c.handleTypes[val.HandleSubtype] = struct{}{}
		r.nameVariants = nameVariantsForHandle(val.HandleSubtype)
		r.WireFamily = FamilyKinds.Reference
		r.NeedsDtor = true
		r.Kind = TypeKinds.Handle
		r.IsResource = true
	case fidlgen.RequestType:
		p := c.compileNameVariants(val.RequestSubtype)
		r.nameVariants = nameVariants{
			Natural: makeName("fidl::InterfaceRequest").template(p.Natural),
			Unified: makeName("fidl::InterfaceRequest").template(p.Unified),
			Wire:    makeName("fidl::ServerEnd").template(p.Wire),
		}
		r.WireFamily = FamilyKinds.Reference
		r.NeedsDtor = true
		r.Kind = TypeKinds.Request
		r.IsResource = true
	case fidlgen.PrimitiveType:
		r.nameVariants = NameVariantsForPrimitive(val.PrimitiveSubtype)
		r.WireFamily = FamilyKinds.TrivialCopy
		r.Kind = TypeKinds.Primitive
	case fidlgen.IdentifierType:
		name := c.compileNameVariants(val.Identifier)
		declInfo, ok := c.decls[val.Identifier]
		if !ok {
			panic(fmt.Sprintf("unknown identifier: %v", val.Identifier))
		}
		declType := declInfo.Type
		if declType == fidlgen.ProtocolDeclType {
			r.nameVariants = nameVariants{
				Natural: makeName("fidl::InterfaceHandle").template(name.Natural),
				Unified: makeName("fidl::InterfaceHandle").template(name.Unified),
				Wire:    makeName("fidl::ClientEnd").template(name.Wire),
			}
			r.WireFamily = FamilyKinds.Reference
			r.NeedsDtor = true
			r.Kind = TypeKinds.Protocol
			r.IsResource = true
		} else {
			switch declType {
			case fidlgen.BitsDeclType:
				r.Kind = TypeKinds.Bits
				r.WireFamily = FamilyKinds.TrivialCopy
			case fidlgen.EnumDeclType:
				r.Kind = TypeKinds.Enum
				r.WireFamily = FamilyKinds.TrivialCopy
			case fidlgen.ConstDeclType:
				r.Kind = TypeKinds.Const
				r.WireFamily = FamilyKinds.Reference
			case fidlgen.StructDeclType:
				r.Kind = TypeKinds.Struct
				r.DeclarationName = val.Identifier
				r.WireFamily = FamilyKinds.Reference
				r.WirePointer = val.Nullable
				r.IsResource = declInfo.IsResourceType()
			case fidlgen.TableDeclType:
				r.Kind = TypeKinds.Table
				r.DeclarationName = val.Identifier
				r.WireFamily = FamilyKinds.Reference
				r.WirePointer = val.Nullable
				r.IsResource = declInfo.IsResourceType()
			case fidlgen.UnionDeclType:
				r.Kind = TypeKinds.Union
				r.DeclarationName = val.Identifier
				r.WireFamily = FamilyKinds.Reference
				r.IsResource = declInfo.IsResourceType()
			default:
				panic(fmt.Sprintf("unknown declaration type: %v", declType))
			}

			if val.Nullable {
				r.nameVariants.Natural = makeName("std::unique_ptr").template(name.Natural)
				r.nameVariants.Unified = makeName("std::unique_ptr").template(name.Unified)
				if declType == fidlgen.UnionDeclType {
					r.nameVariants.Wire = name.Wire
				} else {
					r.nameVariants.Wire = makeName("fidl::ObjectView").template(name.Wire)
				}
				r.NeedsDtor = true
			} else {
				r.nameVariants = name
				r.NeedsDtor = true
			}
		}
	default:
		panic(fmt.Sprintf("unknown type kind: %v", val.Kind))
	}
	return r
}

func compile(r fidlgen.Root, h HeaderOptions) Root {
	root := Root{
		HeaderOptions: h,
	}
	library := make(fidlgen.LibraryIdentifier, 0)
	rawLibrary := make(fidlgen.LibraryIdentifier, 0)
	for _, identifier := range fidlgen.ParseLibraryName(r.Name) {
		safeName := changeIfReserved(string(identifier), nsComponentContext)
		library = append(library, fidlgen.Identifier(safeName))
		rawLibrary = append(rawLibrary, identifier)
	}
	c := compiler{
		symbolPrefix:    formatLibraryPrefix(rawLibrary),
		decls:           r.DeclsWithDependencies(),
		library:         fidlgen.ParseLibraryName(r.Name),
		handleTypes:     make(map[fidlgen.HandleSubtype]struct{}),
		resultForStruct: make(map[fidlgen.EncodedCompoundIdentifier]*Result),
		resultForUnion:  make(map[fidlgen.EncodedCompoundIdentifier]*Result),
	}

	root.RawLibrary = rawLibrary
	root.Library = library
	libraryReversed := make(fidlgen.LibraryIdentifier, len(library))
	for i, j := 0, len(library)-1; i < len(library); i, j = i+1, j-1 {
		libraryReversed[i] = library[j]
	}
	for i, identifier := range library {
		libraryReversed[len(libraryReversed)-i-1] = identifier
	}
	root.LibraryReversed = libraryReversed

	decls := make(map[fidlgen.EncodedCompoundIdentifier]Kinded)

	for _, v := range r.Bits {
		decls[v.Name] = c.compileBits(v)
	}

	for _, v := range r.Consts {
		decls[v.Name] = c.compileConst(v)
	}

	for _, v := range r.Enums {
		decls[v.Name] = c.compileEnum(v)
	}

	// Note: for Result calculation unions must be compiled before structs.
	for _, v := range r.Unions {
		decls[v.Name] = c.compileUnion(v)
	}

	for _, v := range r.Structs {
		// TODO(fxbug.dev/7704) remove once anonymous structs are supported
		if v.Anonymous {
			continue
		}
		decls[v.Name] = c.compileStruct(v)
	}

	for _, v := range r.Tables {
		decls[v.Name] = c.compileTable(v)
	}

	for _, v := range r.Protocols {
		decls[v.Name] = c.compileProtocol(v)
	}

	for _, v := range r.Services {
		decls[v.Name] = c.compileService(v)
	}

	for _, v := range r.DeclOrder {
		// We process only a subset of declarations mentioned in the declaration
		// order, ignore those we do not support.
		if d, known := decls[v]; known {
			root.Decls = append(root.Decls, d)
		}
	}

	for _, l := range r.Libraries {
		if l.Name == r.Name {
			// We don't need to include our own header.
			continue
		}
		libraryIdent := fidlgen.ParseLibraryName(l.Name)
		root.Headers = append(root.Headers, formatLibraryPath(libraryIdent))
	}

	// zx::channel is always referenced by the protocols in llcpp bindings API
	if len(r.Protocols) > 0 {
		c.handleTypes["channel"] = struct{}{}
	}

	// find all unique handle types referenced by the library
	var handleTypes []string
	for k := range c.handleTypes {
		handleTypes = append(handleTypes, string(k))
	}
	sort.Sort(sort.StringSlice(handleTypes))
	root.HandleTypes = handleTypes

	return root
}

func CompileHL(r fidlgen.Root, h HeaderOptions) Root {
	return compile(r.ForBindings("hlcpp"), h)
}

func CompileLL(r fidlgen.Root, h HeaderOptions) Root {
	return compile(r.ForBindings("llcpp"), h)
}

func CompileUnified(r fidlgen.Root, h HeaderOptions) Root {
	return compile(r.ForBindings("cpp"), h)
}

func CompileLibFuzzer(r fidlgen.Root, h HeaderOptions) Root {
	return compile(r.ForBindings("libfuzzer"), h)
}
