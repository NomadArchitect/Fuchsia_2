// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

package fidlgen_cpp

import (
	"fmt"
	"strings"

	"go.fuchsia.dev/fuchsia/tools/fidl/lib/fidlgen"
)

//
// Combined logic for constant values and const declarations.
//

type ConstantValue struct {
	Natural string
	Wire    string
}

func (cv ConstantValue) IsSet() bool {
	return cv.Natural != "" && cv.Wire != ""
}

func (cv ConstantValue) String() string {
	switch currentVariant {
	case noVariant:
		fidlgen.TemplateFatalf(
			"Called ConstantValue.String() on %s/%s when currentVariant isn't set.\n",
			cv.Natural, cv.Wire)
	case naturalVariant:
		return string(cv.Natural)
	case wireVariant:
		return string(cv.Wire)
	}
	panic("not reached")
}

func (c *compiler) compileConstant(val fidlgen.Constant, t *Type, typ fidlgen.Type) ConstantValue {
	switch val.Kind {
	case fidlgen.IdentifierConstant:
		ci := fidlgen.ParseCompoundIdentifier(val.Identifier)
		if len(ci.Member) > 0 {
			member := changeIfReserved(ci.Member)
			ci.Member = ""
			dn := c.compileDeclName(ci.Encode())
			return ConstantValue{
				Natural: dn.Natural.String() + "::" + member,
				Wire:    dn.Wire.String() + "::" + member,
			}
		} else {
			dn := c.compileDeclName(val.Identifier)
			return ConstantValue{Natural: dn.Natural.String(), Wire: dn.Wire.String()}
		}
	case fidlgen.LiteralConstant:
		lit := c.compileLiteral(val.Literal, typ)
		return ConstantValue{Natural: lit, Wire: lit}
	case fidlgen.BinaryOperator:
		return ConstantValue{
			Natural: fmt.Sprintf("static_cast<%s>(%s)", t.TypeName.Natural, val.Value),
			Wire:    fmt.Sprintf("static_cast<%s>(%s)", t.TypeName.Wire, val.Value),
		}
		// return ConstantValue{Natural: naturalVal, Wire: wireVal}
	default:
		panic(fmt.Sprintf("unknown constant kind: %v", val.Kind))
	}
}

type Const struct {
	fidlgen.Attributes
	DeclName
	Extern    bool
	Decorator string
	Type      Type
	Value     ConstantValue
}

func (Const) Kind() declKind {
	return Kinds.Const
}

var _ Kinded = (*Const)(nil)

func (c *compiler) compileConst(val fidlgen.Const) Const {
	n := c.compileDeclName(val.Name)
	v := Const{Attributes: val.Attributes,
		DeclName: n,
	}
	if val.Type.Kind == fidlgen.StringType {
		v.Extern = true
		v.Decorator = "const"
		v.Type = Type{
			TypeName: PrimitiveTypeName("char*"),
		}
		v.Value = c.compileConstant(val.Value, nil, val.Type)
	} else {
		t := c.compileType(val.Type)
		v.Extern = false
		v.Decorator = "constexpr"
		v.Type = t
		v.Value = c.compileConstant(val.Value, &t, val.Type)
	}

	return v
}

func (c *compiler) compileLiteral(val fidlgen.Literal, typ fidlgen.Type) string {
	switch val.Kind {
	case fidlgen.StringLiteral:
		return fmt.Sprintf("%q", val.Value)
	case fidlgen.NumericLiteral:
		if val.Value == "-9223372036854775808" || val.Value == "0x8000000000000000" {
			// C++ only supports nonnegative literals and a value this large in absolute
			// value cannot be represented as a nonnegative number in 64-bits.
			return "(-9223372036854775807ll-1)"
		}
		// TODO(fxbug.dev/7810): Once we expose resolved constants for defaults, e.g.
		// in structs, we will not need ignore hex and binary values.
		if strings.HasPrefix(val.Value, "0x") || strings.HasPrefix(val.Value, "0b") {
			return val.Value
		}

		// float32 literals must be marked as such.
		if strings.ContainsRune(val.Value, '.') {
			if typ.Kind == fidlgen.PrimitiveType && typ.PrimitiveSubtype == fidlgen.Float32 {
				return fmt.Sprintf("%sf", val.Value)
			} else {
				return val.Value
			}
		}

		if !strings.HasPrefix(val.Value, "-") {
			return fmt.Sprintf("%su", val.Value)
		}
		return val.Value
	case fidlgen.TrueLiteral:
		return "true"
	case fidlgen.FalseLiteral:
		return "false"
	case fidlgen.DefaultLiteral:
		return "default"
	default:
		panic(fmt.Sprintf("unknown literal kind: %v", val.Kind))
	}
}
