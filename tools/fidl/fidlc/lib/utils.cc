// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <cassert>
#include <iostream>
#include <regex>

#include <fidl/utils.h>

namespace fidl {
namespace utils {

// TODO(fxbug.dev/70247): Delete this
bool HasDeprecatedSyntaxToken(const fidl::SourceFile& source_file) {
  const std::string_view text = source_file.data();
  size_t line_start = 0;
  size_t next_line_break = text.find('\n');
  while (next_line_break != std::string_view::npos) {
    static std::string needle = "deprecated_syntax;";
    std::string_view line = text.substr(line_start, next_line_break - line_start);
    line.remove_prefix(std::min(line.find_first_not_of(" \t\f\v"), line.size()));
    line = line.substr(0, std::min(needle.size(), line.size()));
    if (line == needle)
      return true;
    if (!line.empty() && line[0] != '/')
      return false;

    line_start = next_line_break + 1;
    next_line_break = text.find('\n', line_start);
  }
  return false;
}

const std::string kLibraryComponentPattern = "[a-z][a-z0-9]*";
const std::string kIdentifierComponentPattern = "[A-Za-z]([A-Za-z0-9_]*[A-Za-z0-9])?";

bool IsValidLibraryComponent(const std::string& component) {
  static const std::regex kPattern("^" + kLibraryComponentPattern + "$");
  return std::regex_match(component, kPattern);
}

bool IsValidIdentifierComponent(const std::string& component) {
  static const std::regex kPattern("^" + kIdentifierComponentPattern + "$");
  return std::regex_match(component, kPattern);
}

bool IsValidFullyQualifiedMethodIdentifier(const std::string& fq_identifier) {
  static const std::regex kPattern("^" +
                                   // library identifier
                                   kLibraryComponentPattern + "(\\." + kLibraryComponentPattern +
                                   ")*" +
                                   // slash
                                   "/" +
                                   // protocol
                                   kIdentifierComponentPattern +
                                   // dot
                                   "\\." +
                                   // method
                                   kIdentifierComponentPattern + "$");
  return std::regex_match(fq_identifier, kPattern);
}

bool ends_with_underscore(const std::string& str) {
  assert(str.size() > 0);
  return str.back() == '_';
}

bool has_adjacent_underscores(const std::string& str) {
  return str.find("__") != std::string::npos;
}

bool has_konstant_k(const std::string& str) {
  return str.size() >= 2 && str[0] == 'k' && isupper(str[1]);
}

std::string strip_string_literal_quotes(std::string_view str) {
  assert(str.size() >= 2 && str[0] == '"' && str[str.size() - 1] == '"' &&
         "string must start and end with '\"' style quotes");
  return std::string(str.data() + 1, str.size() - 2);
}

// NOTE: we currently explicitly only support UNIX line endings
std::string strip_doc_comment_slashes(std::string_view str) {
  // In English, this regex says: "any number of tabs/spaces, followed by three
  // slashes is group 1, the remainder of the line is group 2.  Keep only group
  // 2."
  std::string no_slashes =
      regex_replace(std::string(str), std::regex{"([\\t ]*\\/\\/\\/)(.*)"}, "$2");
  if (no_slashes[no_slashes.size() - 1] != '\n') {
    return no_slashes + '\n';
  }
  return no_slashes;
}

std::string strip_konstant_k(const std::string& str) {
  if (has_konstant_k(str)) {
    return str.substr(1);
  } else {
    return str;
  }
}

bool is_lower_no_separator_case(const std::string& str) {
  static std::regex re{"^[a-z][a-z0-9]*$"};
  return str.size() > 0 && std::regex_match(str, re);
}

bool is_lower_snake_case(const std::string& str) {
  static std::regex re{"^[a-z][a-z0-9_]*$"};
  return str.size() > 0 && std::regex_match(str, re);
}

bool is_upper_snake_case(const std::string& str) {
  static std::regex re{"^[A-Z][A-Z0-9_]*$"};
  return str.size() > 0 && std::regex_match(str, re);
}

bool is_lower_camel_case(const std::string& str) {
  if (has_konstant_k(str)) {
    return false;
  }
  static std::regex re{"^[a-z][a-z0-9]*(([A-Z]{1,2}[a-z0-9]+)|(_[0-9]+))*([A-Z][a-z0-9]*)?$"};
  return str.size() > 0 && std::regex_match(str, re);
}

bool is_upper_camel_case(const std::string& str) {
  static std::regex re{
      "^(([A-Z]{1,2}[a-z0-9]+)(([A-Z]{1,2}[a-z0-9]+)|(_[0-9]+))*)?([A-Z][a-z0-9]*)?$"};
  return str.size() > 0 && std::regex_match(str, re);
}

bool is_konstant_case(const std::string& astr) {
  if (!has_konstant_k(astr)) {
    return false;
  }
  std::string str = strip_konstant_k(astr);
  return is_upper_camel_case(str);
}

static void add_word(std::string word, std::vector<std::string>& words,
                     const std::set<std::string>& stop_words) {
  if (stop_words.find(word) == stop_words.end()) {
    words.push_back(word);
  }
}

std::vector<std::string> id_to_words(const std::string& astr) { return id_to_words(astr, {}); }

std::vector<std::string> id_to_words(const std::string& astr, std::set<std::string> stop_words) {
  std::string str = strip_konstant_k(astr);
  std::vector<std::string> words;
  std::string word;
  bool last_char_was_upper_or_begin = true;
  for (size_t i = 0; i < str.size(); i++) {
    char ch = str[i];
    if (ch == '_' || ch == '-' || ch == '.') {
      if (word.size() > 0) {
        add_word(word, words, stop_words);
        word.clear();
      }
      last_char_was_upper_or_begin = true;
    } else {
      bool next_char_is_lower = ((i + 1) < str.size()) && islower(str[i + 1]);
      if (isupper(ch) && (!last_char_was_upper_or_begin || next_char_is_lower)) {
        if (word.size() > 0) {
          add_word(word, words, stop_words);
          word.clear();
        }
      }
      word.push_back(static_cast<char>(tolower(ch)));
      last_char_was_upper_or_begin = isupper(ch);
    }
  }
  if (word.size() > 0) {
    add_word(word, words, stop_words);
  }
  return words;
}

std::string to_lower_no_separator_case(const std::string& astr) {
  std::string str = strip_konstant_k(astr);
  std::string newid;
  for (const auto& word : id_to_words(str)) {
    newid.append(word);
  }
  return newid;
}

std::string to_lower_snake_case(const std::string& astr) {
  std::string str = strip_konstant_k(astr);
  std::string newid;
  for (const auto& word : id_to_words(str)) {
    if (newid.size() > 0) {
      newid.push_back('_');
    }
    newid.append(word);
  }
  return newid;
}

std::string to_upper_snake_case(const std::string& astr) {
  std::string str = strip_konstant_k(astr);
  auto newid = to_lower_snake_case(str);
  std::transform(newid.begin(), newid.end(), newid.begin(), ::toupper);
  return newid;
}

std::string to_lower_camel_case(const std::string& astr) {
  std::string str = strip_konstant_k(astr);
  bool prev_char_was_digit = false;
  std::string newid;
  for (const auto& word : id_to_words(str)) {
    if (newid.size() == 0) {
      newid.append(word);
    } else {
      if (prev_char_was_digit && isdigit(word[0])) {
        newid.push_back('_');
      }
      newid.push_back(static_cast<char>(toupper(word[0])));
      newid.append(word.substr(1));
    }
    prev_char_was_digit = isdigit(word.back());
  }
  return newid;
}

std::string to_upper_camel_case(const std::string& astr) {
  std::string str = strip_konstant_k(astr);
  bool prev_char_was_digit = false;
  std::string newid;
  for (const auto& word : id_to_words(str)) {
    if (prev_char_was_digit && isdigit(word[0])) {
      newid.push_back('_');
    }
    newid.push_back(static_cast<char>(toupper(word[0])));
    newid.append(word.substr(1));
    prev_char_was_digit = isdigit(word.back());
  }
  return newid;
}

std::string to_konstant_case(const std::string& str) { return "k" + to_upper_camel_case(str); }

std::string canonicalize(std::string_view identifier) {
  const auto size = identifier.size();
  std::string canonical;
  char prev = '_';
  for (size_t i = 0; i < size; i++) {
    const char c = identifier[i];
    if (c == '_') {
      if (prev != '_') {
        canonical.push_back('_');
      }
    } else if (((islower(prev) || isdigit(prev)) && isupper(c)) ||
               (prev != '_' && isupper(c) && i + 1 < size && islower(identifier[i + 1]))) {
      canonical.push_back('_');
      canonical.push_back(static_cast<char>(tolower(c)));
    } else {
      canonical.push_back(static_cast<char>(tolower(c)));
    }
    prev = c;
  }
  return canonical;
}

std::string StringJoin(const std::vector<std::string_view>& strings, std::string_view separator) {
  std::string result;
  bool first = true;
  for (const auto& part : strings) {
    if (!first) {
      result += separator;
    }
    first = false;
    result += part;
  }
  return result;
}

void PrintFinding(std::ostream& os, const Finding& finding) {
  os << finding.message() << " [";
  os << finding.subcategory();
  os << "]";
  if (finding.suggestion().has_value()) {
    auto& suggestion = finding.suggestion();
    os << "; " << suggestion->description();
    if (suggestion->replacement().has_value()) {
      os << "\n    Proposed replacement:  '" << *suggestion->replacement() << "'";
    }
  }
}

std::vector<std::string> FormatFindings(const Findings& findings, bool enable_color) {
  std::vector<std::string> lint;
  for (auto& finding : findings) {
    std::stringstream ss;
    PrintFinding(ss, finding);
    auto warning = reporter::Format("warning", std::make_optional(finding.span()), ss.str(),
                                    enable_color, finding.span().data().size());
    lint.push_back(warning);
  }
  return lint;
}

bool OnlyWhitespaceChanged(const std::string& unformatted_input,
                           const std::string& formatted_output) {
  std::string formatted = formatted_output;
  auto formatted_end = std::remove_if(formatted.begin(), formatted.end(), isspace);
  formatted.erase(formatted_end, formatted.end());

  std::string unformatted(unformatted_input);
  auto unformatted_end = std::remove_if(unformatted.begin(), unformatted.end(), isspace);
  unformatted.erase(unformatted_end, unformatted.end());

  return formatted == unformatted;
}

namespace {

bool IsLocationStart(std::string_view s) {
  // the only place something like `"foo":` can show up in valid JSON is if
  // "foo" is a key since strings that have quotes in them must escape the
  // quotes and there's no other place where a string value can be followed by
  // a colon.
  // Since there's only one location field in the schema, it is safe to use this
  // to check for it.
  return s.find("\"location\": {") != std::string_view::npos;
}

bool IsLocationEnd(std::string_view s) { return s.find('}') != std::string_view::npos; }

}  // namespace

bool IsIrEquals(const std::string& from_old, const std::string& from_new) {
  std::istringstream from_old_stream(from_old);
  std::istringstream from_new_stream(from_new);

  bool in_location = false;
  std::string old_line, new_line;
  while (true) {
    auto old_remaining = bool(std::getline(from_old_stream, old_line));
    auto new_remaining = bool(std::getline(from_new_stream, new_line));

    if (!old_remaining && !new_remaining)
      return true;
    if (!old_remaining || !new_remaining)
      return false;

    if (!in_location && old_line != new_line) {
      return false;
    }

    if (!in_location && IsLocationStart(old_line)) {
      in_location = true;
    } else if (in_location && IsLocationEnd(old_line)) {
      if (!IsLocationEnd(new_line))
        return false;
      in_location = false;
    }
  }
}

}  // namespace utils
}  // namespace fidl
