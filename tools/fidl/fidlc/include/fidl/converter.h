// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef TOOLS_FIDL_FIDLC_INCLUDE_FIDL_CONVERTER_H_
#define TOOLS_FIDL_FIDLC_INCLUDE_FIDL_CONVERTER_H_

// The ConvertingTreeVisitor takes a raw::File, and translates its textual
// representation from one syntax to another.
#include <stack>

#include "conversion.h"
#include "flat/name.h"
#include "flat_ast.h"
#include "tree_visitor.h"
#include "underlying_type.h"

namespace fidl::conv {

class Converting;

class ConvertingTreeVisitor : public raw::DeclarationOrderTreeVisitor {
  friend Converting;

 public:
  explicit ConvertingTreeVisitor(fidl::utils::Syntax syntax, const flat::Library* library)
      : to_syntax_(syntax), last_conversion_end_(nullptr), last_comment_(0), library_(library) {}

  // The following block of visitors are purposeful noops. Their nodes are
  // guaranteed to be identical in both the old and new syntax, so its best to
  // just ignore their contents, and merely copy the gaps between convertible
  // elements wholesale instead.
  void OnAttribute(const raw::Attribute& element) override {}
  void OnAttributeList(std::unique_ptr<raw::AttributeList> const& element) override {}
  void OnBitsMember(std::unique_ptr<raw::BitsMember> const& element) override {}
  void OnComposeProtocol(std::unique_ptr<raw::ComposeProtocol> const& element) override {}
  void OnEnumMember(std::unique_ptr<raw::EnumMember> const& element) override {}
  void OnResourceDeclaration(
      std::unique_ptr<fidl::raw::ResourceDeclaration> const& element) override {}
  void OnResourceProperty(std::unique_ptr<fidl::raw::ResourceProperty> const& element) override {}
  void OnServiceDeclaration(std::unique_ptr<raw::ServiceDeclaration> const& element) override {}
  void OnServiceMember(std::unique_ptr<raw::ServiceMember> const& element) override {}
  void OnSourceElementStart(const raw::SourceElement& element) override {}
  void OnSourceElementEnd(const raw::SourceElement& element) override {}
  void OnUsing(std::unique_ptr<raw::Using> const& element) override {}

  // TODO(azaslavsky): I'll eventually remove the commented out block below.  At
  //   the moment it serves as a useful list of TreeVisitor methods that are
  //   intended to be left unmodified by the ConvertingTreeVisitor.
  // void OnBinaryOperatorConstant(std::unique_ptr<BinaryOperatorConstant> const& element) override
  // {} void OnCompoundIdentifier(std::unique_ptr<CompoundIdentifier> const& element) override {}
  // void OnConstant(std::unique_ptr<Constant> const& element) override {}
  // void OnIdentifier(std::unique_ptr<Identifier> const& element) override;
  // void OnIdentifierConstant(std::unique_ptr<IdentifierConstant> const& element) override {}
  // void OnLiteral(std::unique_ptr<fidl::raw::Literal> const& element) override {}
  // void OnLiteralConstant(std::unique_ptr<LiteralConstant> const& element) override {}
  // void OnNullability(types::Nullability nullability) override {}
  // void OnParameterList(std::unique_ptr<ParameterList> const& element) override {}
  // void OnPrimitiveSubtype(types::PrimitiveSubtype subtype) override {}
  // void OnProtocolDeclaration(std::unique_ptr<ProtocolDeclaration> const& element) override {}
  // void OnProtocolMethod(std::unique_ptr<ProtocolMethod> const& element) override {}

  // The remaining "On*" methods are loosely organized by keyword.  All of them
  // must be overwritten by the implementation.

  // Bits.
  void OnBitsDeclaration(std::unique_ptr<raw::BitsDeclaration> const& element) override;

  // Constants.
  void OnConstDeclaration(std::unique_ptr<raw::ConstDeclaration> const& element) override;

  // Enums.
  void OnEnumDeclaration(std::unique_ptr<raw::EnumDeclaration> const& element) override;

  // Files.
  void OnFile(std::unique_ptr<raw::File> const& element) override;

  // Method Parameters.
  void OnParameter(std::unique_ptr<raw::Parameter> const& element) override;

  // Structs.
  void OnStructDeclaration(std::unique_ptr<raw::StructDeclaration> const& element) override;
  void OnStructMember(std::unique_ptr<raw::StructMember> const& element) override;

  // Tables.
  void OnTableDeclaration(std::unique_ptr<raw::TableDeclaration> const& element) override;
  void OnTableMember(std::unique_ptr<raw::TableMember> const& element) override;

  // Types.
  void OnTypeConstructorOld(std::unique_ptr<raw::TypeConstructorOld> const& element) override;

  // Unions.
  void OnUnionDeclaration(std::unique_ptr<raw::UnionDeclaration> const& element) override;
  void OnUnionMember(std::unique_ptr<raw::UnionMember> const& element) override;

  // Used to return a string with the converted output upon converter
  // completion.
  std::string* converted_output() { return &converted_output_; }

 private:
  // String built over the course of the visitor's execution containing the
  // converted output.
  std::string converted_output_;

  // Tracks which syntax we will be converting to.  Setting this value to
  // kExisting is useful to validate that ConvertingTreeVisitor is working
  // properly: any compile-able FIDL file should be returned from this converter
  // with no changes if kOld is used.
  const fidl::utils::Syntax to_syntax_;

  // A stack of currently active conversions.  Each conversion in the stack
  // operates on a set of characters that are strictly contained within those of
  // its parent (ex, if the first entry in the stack is converting the "bar baz"
  // portion of "foo bar baz quux," the second entry may only convert spans
  // within that range, like "bar" or "baz").
  std::stack<std::unique_ptr<Conversion>> open_conversions_;

  // A char pointer tracing the last end point of the last conversion seen thus
  // far.  This is used to verify that prefix ranges are not copied multiple
  // times when doing nested conversions, and to ensure that the remaining text
  // after the final conversion gets copied.
  const char* last_conversion_end_;

  // A list of all C-Style "//"-leading comments in the file (ie, all comments
  // except doc comments).  We need to store this because some of the conversion
  // spans may include weirdly-placed comments that we do not want to lose.
  // Instead, such comments should be appended to the conversion's prefix.
  std::vector<std::unique_ptr<Token>> comments_;

  // Keeps track of the last comment in the comments_ list to have been "tested"
  // for being inside a conversion span.  The char pointer at the vector index
  // pointed to by this member should never exceed the char pointer held in
  // last_conversion_end_.
  std::size_t last_comment_;

  // A pointer to the flat::Library representation of the file being visited.
  // This will be used when resolving and converting type definitions that
  // are behind aliases, defined in the imported libraries, and so forth.
  const flat::Library* library_;

  // Meant to be called from inside the "OnTypeConstructor" method in the
  // implementation.  For that method to do its work properly, it must be able
  // to deduce the built-in type underpinning the type declaration.  For
  // example, if OnTypeConstructor is currently looking at the type declaration
  // "Foo<Bar>:4," what do "Foo" and "Bar" represent?  The conversion applied
  // will look very different depending on which built-ins those identifiers
  // resolve to.
  std::optional<UnderlyingType> resolve(const std::unique_ptr<raw::TypeConstructorOld>& type_ctor);
};

class Converting {
 public:
  // Helper method for starting a new conversion.  It takes three arguments: a
  // Conversion object specifying the type of conversion being attempted, as
  // well as two tokens representing the start and end point of the span that
  // will need to be modified.  For example, if we are attempting to convert the
  // element "const uint8 FOO = 5;" the first argument will be a unique_ptr to a
  // NameAndTypeConversion (to re-order "uint8" after "FOO"), the second will be
  // a token pointing to "uint8," and the third a token pointing to "FOO."
  //
  // By specifying the start and end points within the element being converted,
  // we are able to advance the last_conversion_end_ pointer to the end, which
  // prevents double conversion.  Further, all text between the previous value
  // of last_conversion_end_ and the start token may be blindly copied, since we
  // are now sure that there are not conversions taking place in that span.
  Converting(ConvertingTreeVisitor* ctv, std::unique_ptr<Conversion> conversion, const Token& start,
             const Token& end);

  // If a conversion is not the last remaining entry in the open_conversions_
  // stack, its stringified output is simply passed to the top entry of that
  // stack, to be incorporated as a nested element in that entry.  If it is the
  // last entry, the text is written to the output string instead.
  ~Converting();

 private:
  ConvertingTreeVisitor* ctv_;
};

}  // namespace fidl::conv

#endif  // TOOLS_FIDL_FIDLC_INCLUDE_FIDL_CONVERTER_H_
