// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod component;

use {
    fidl_fuchsia_component_runner as fcrunner, fidl_fuchsia_data as fdata, std::path::Path,
    thiserror::Error,
};

/// An error encountered operating on `ComponentStartInfo`.
#[derive(Debug, PartialEq, Eq, Error)]
pub enum StartInfoError {
    #[error("missing url")]
    MissingUrl,
}

/// An error encountered trying to get entry out of `ComponentStartInfo->program`.
#[derive(Debug, PartialEq, Eq, Error)]
pub enum StartInfoProgramError {
    #[error("\"program.binary\" must be specified")]
    MissingBinary,

    #[error("the value of \"program.binary\" must be a string")]
    InValidBinaryType,

    #[error("the value of \"program.binary\" must be a relative path")]
    BinaryPathNotRelative,

    #[error("invalid type in arguments")]
    InvalidArguments,

    #[error("\"program\" must be specified")]
    NotFound,

    #[error("invalid type for key \"{0}\"")]
    InvalidType(String),

    #[error("invalid value for key \"{0}\", expected one of \"{1}\", found \"{2}\"")]
    InvalidValue(String, String, String),
}

/// Retrieves component URL from start_info or errors out if not found.
pub fn get_resolved_url(
    start_info: &fcrunner::ComponentStartInfo,
) -> Result<String, StartInfoError> {
    match &start_info.resolved_url {
        Some(url) => Ok(url.to_string()),
        _ => Err(StartInfoError::MissingUrl),
    }
}

/// Returns a reference to the value corresponding to the key.
pub fn get_value<'a>(dict: &'a fdata::Dictionary, key: &str) -> Option<&'a fdata::DictionaryValue> {
    match &dict.entries {
        Some(entries) => {
            for entry in entries {
                if entry.key == key {
                    return entry.value.as_ref().map(|val| &**val);
                }
            }
            None
        }
        _ => None,
    }
}

/// Retrieve a reference to the enum value corresponding to the key.
pub fn get_enum<'a>(
    dict: &'a fdata::Dictionary,
    key: &str,
    variants: &[&str],
) -> Result<Option<&'a str>, StartInfoProgramError> {
    match get_value(dict, key) {
        Some(fdata::DictionaryValue::Str(value)) => {
            if variants.contains(&value.as_str()) {
                Ok(Some(value.as_ref()))
            } else {
                Err(StartInfoProgramError::InvalidValue(
                    key.to_owned(),
                    format!("{:?}", variants),
                    value.to_owned(),
                ))
            }
        }
        Some(_) => Err(StartInfoProgramError::InvalidType(key.to_owned())),
        None => Ok(None),
    }
}

/// Retrieve value of type bool. Defaults to 'false' if key is not found.
pub fn get_bool<'a>(dict: &'a fdata::Dictionary, key: &str) -> Result<bool, StartInfoProgramError> {
    match get_enum(dict, key, &["true", "false"])? {
        Some("true") => Ok(true),
        _ => Ok(false),
    }
}

fn get_program_value<'a>(
    start_info: &'a fcrunner::ComponentStartInfo,
    key: &str,
) -> Option<&'a fdata::DictionaryValue> {
    get_value(start_info.program.as_ref()?, key)
}

/// Retrieve a string from the program dictionary in ComponentStartInfo.
pub fn get_program_string<'a>(
    start_info: &'a fcrunner::ComponentStartInfo,
    key: &str,
) -> Option<&'a str> {
    if let fdata::DictionaryValue::Str(value) = get_program_value(start_info, key)? {
        Some(value)
    } else {
        None
    }
}

/// Retrieve a StrVec from the program dictionary in ComponentStartInfo.
pub fn get_program_strvec<'a>(
    start_info: &'a fcrunner::ComponentStartInfo,
    key: &str,
) -> Option<&'a Vec<String>> {
    if let fdata::DictionaryValue::StrVec(value) = get_program_value(start_info, key)? {
        Some(value)
    } else {
        None
    }
}

/// Retrieves program.binary from ComponentStartInfo and makes sure that path is relative.
pub fn get_program_binary(
    start_info: &fcrunner::ComponentStartInfo,
) -> Result<String, StartInfoProgramError> {
    if let Some(program) = &start_info.program {
        if let Some(val) = get_value(program, "binary") {
            if let fdata::DictionaryValue::Str(bin) = val {
                if !Path::new(bin).is_absolute() {
                    Ok(bin.to_string())
                } else {
                    Err(StartInfoProgramError::BinaryPathNotRelative)
                }
            } else {
                Err(StartInfoProgramError::InValidBinaryType)
            }
        } else {
            Err(StartInfoProgramError::MissingBinary)
        }
    } else {
        Err(StartInfoProgramError::NotFound)
    }
}

/// Retrieves program.args from ComponentStartInfo and validates them.
pub fn get_program_args(
    start_info: &fcrunner::ComponentStartInfo,
) -> Result<Vec<String>, StartInfoProgramError> {
    if let Some(vec) = get_program_strvec(start_info, "args") {
        Ok(vec.iter().map(|v| v.clone()).collect())
    } else {
        Ok(vec![])
    }
}

#[cfg(test)]
mod tests {
    use {super::*, test_case::test_case};

    #[test_case(Some("some_url"), Ok("some_url".to_owned()) ; "when url is valid")]
    #[test_case(None, Err(StartInfoError::MissingUrl) ; "when url is missing")]
    fn get_resolved_url_test(maybe_url: Option<&str>, expected: Result<String, StartInfoError>) {
        let start_info = fcrunner::ComponentStartInfo {
            resolved_url: maybe_url.map(str::to_owned),
            program: None,
            ns: None,
            outgoing_dir: None,
            runtime_dir: None,
            ..fcrunner::ComponentStartInfo::EMPTY
        };
        assert_eq!(get_resolved_url(&start_info), expected,);
    }

    #[test_case(Some("bin/myexecutable"), Ok("bin/myexecutable".to_owned()) ; "when binary value is valid")]
    #[test_case(Some("/bin/myexecutable"), Err(StartInfoProgramError::BinaryPathNotRelative) ; "when binary path is not relative")]
    #[test_case(None, Err(StartInfoProgramError::NotFound) ; "when program stanza is not set")]
    fn get_program_binary_test(
        maybe_value: Option<&str>,
        expected: Result<String, StartInfoProgramError>,
    ) {
        let start_info = match maybe_value {
            Some(value) => new_start_info(Some(new_program_stanza("binary", value))),
            None => new_start_info(None),
        };
        assert_eq!(get_program_binary(&start_info), expected);
    }

    #[test]
    fn get_program_binary_test_when_binary_key_is_missing() {
        let start_info = new_start_info(Some(new_program_stanza("some_other_key", "bin/foo")));
        assert_eq!(get_program_binary(&start_info), Err(StartInfoProgramError::MissingBinary));
    }

    #[test_case(&[], Ok(vec![]) ; "when args is empty")]
    #[test_case(&["a".to_owned()], Ok(vec!["a".to_owned()]) ; "when args is a")]
    #[test_case(&["a".to_owned(), "b".to_owned()], Ok(vec!["a".to_owned(), "b".to_owned()]) ; "when args a and b")]
    fn get_program_args_test(
        args: &[String],
        expected: Result<Vec<String>, StartInfoProgramError>,
    ) {
        let start_info = new_start_info(Some(new_program_stanza_with_vec("args", Vec::from(args))));
        assert_eq!(get_program_args(&start_info), expected);
    }

    fn new_start_info(program: Option<fdata::Dictionary>) -> fcrunner::ComponentStartInfo {
        fcrunner::ComponentStartInfo {
            program: program,
            ns: None,
            outgoing_dir: None,
            runtime_dir: None,
            resolved_url: None,
            ..fcrunner::ComponentStartInfo::EMPTY
        }
    }

    fn new_program_stanza(key: &str, value: &str) -> fdata::Dictionary {
        fdata::Dictionary {
            entries: Some(vec![fdata::DictionaryEntry {
                key: key.to_owned(),
                value: Some(Box::new(fdata::DictionaryValue::Str(value.to_owned()))),
            }]),
            ..fdata::Dictionary::EMPTY
        }
    }

    fn new_program_stanza_with_vec(key: &str, values: Vec<String>) -> fdata::Dictionary {
        fdata::Dictionary {
            entries: Some(vec![fdata::DictionaryEntry {
                key: key.to_owned(),
                value: Some(Box::new(fdata::DictionaryValue::StrVec(values))),
            }]),
            ..fdata::Dictionary::EMPTY
        }
    }
}
