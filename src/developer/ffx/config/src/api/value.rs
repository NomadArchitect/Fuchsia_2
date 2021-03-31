// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    crate::api::{
        query::{ConfigQuery, SelectMode},
        ConfigError,
    },
    crate::mapping::{filter::filter, flatten::flatten},
    anyhow::anyhow,
    serde_json::Value,
    std::{
        convert::{From, TryFrom, TryInto},
        path::PathBuf,
    },
};

const ADDITIVE_RETURN_ERR: &str =
    "Additive mode can only be used with an array or Value return type.";
const _ADDITIVE_LEVEL_ERR: &str =
    "Additive mode can only be used if config level is not specified.";

pub struct ConfigValue(pub(crate) Option<Value>);

pub trait ValueStrategy {
    fn handle_arrays<'a, T: Fn(Value) -> Option<Value> + Sync>(
        next: &'a T,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a>;

    fn validate_query(query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError>;
}

impl From<ConfigValue> for Option<Value> {
    fn from(value: ConfigValue) -> Self {
        value.0
    }
}

impl From<Option<Value>> for ConfigValue {
    fn from(value: Option<Value>) -> Self {
        ConfigValue(value)
    }
}

impl ValueStrategy for Value {
    fn handle_arrays<'a, T: Fn(Value) -> Option<Value> + Sync>(
        next: &'a T,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a> {
        flatten(next)
    }

    fn validate_query(_query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError> {
        Ok(())
    }
}

impl TryFrom<ConfigValue> for Value {
    type Error = ConfigError;

    fn try_from(value: ConfigValue) -> std::result::Result<Self, Self::Error> {
        value.0.ok_or(anyhow!("no value").into())
    }
}

impl ValueStrategy for Option<Value> {
    fn handle_arrays<'a, T: Fn(Value) -> Option<Value> + Sync>(
        next: &'a T,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a> {
        flatten(next)
    }

    fn validate_query(_query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError> {
        Ok(())
    }
}

impl ValueStrategy for String {
    fn handle_arrays<'a, T: Fn(Value) -> Option<Value> + Sync>(
        next: &'a T,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a> {
        flatten(next)
    }

    fn validate_query(query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError> {
        match query.select {
            SelectMode::First => Ok(()),
            SelectMode::All => Err(anyhow!(ADDITIVE_RETURN_ERR).into()),
        }
    }
}

impl TryFrom<ConfigValue> for String {
    type Error = ConfigError;

    fn try_from(value: ConfigValue) -> std::result::Result<Self, Self::Error> {
        value
            .0
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .ok_or(anyhow!("no configuration String value found").into())
    }
}

impl ValueStrategy for Option<String> {
    fn handle_arrays<'a, T: Fn(Value) -> Option<Value> + Sync>(
        next: &'a T,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a> {
        flatten(next)
    }

    fn validate_query(query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError> {
        match query.select {
            SelectMode::First => Ok(()),
            SelectMode::All => Err(anyhow!(ADDITIVE_RETURN_ERR).into()),
        }
    }
}

impl TryFrom<ConfigValue> for Option<String> {
    type Error = ConfigError;

    fn try_from(value: ConfigValue) -> std::result::Result<Self, Self::Error> {
        Ok(value.0.and_then(|v| v.as_str().map(|s| s.to_string())))
    }
}

impl ValueStrategy for usize {
    fn handle_arrays<'a, T: Fn(Value) -> Option<Value> + Sync>(
        next: &'a T,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a> {
        flatten(next)
    }

    fn validate_query(query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError> {
        match query.select {
            SelectMode::First => Ok(()),
            SelectMode::All => Err(anyhow!(ADDITIVE_RETURN_ERR).into()),
        }
    }
}

impl TryFrom<ConfigValue> for usize {
    type Error = ConfigError;

    fn try_from(value: ConfigValue) -> std::result::Result<Self, Self::Error> {
        value
            .0
            .and_then(|v| {
                v.as_u64().and_then(|v| usize::try_from(v).ok()).or_else(|| {
                    if let Value::String(s) = v {
                        s.parse().ok()
                    } else {
                        None
                    }
                })
            })
            .ok_or(anyhow!("no configuration usize value found").into())
    }
}

impl ValueStrategy for u64 {
    fn handle_arrays<'a, T: Fn(Value) -> Option<Value> + Sync>(
        next: &'a T,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a> {
        flatten(next)
    }

    fn validate_query(query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError> {
        match query.select {
            SelectMode::First => Ok(()),
            SelectMode::All => Err(anyhow!(ADDITIVE_RETURN_ERR).into()),
        }
    }
}

impl TryFrom<ConfigValue> for u64 {
    type Error = ConfigError;

    fn try_from(value: ConfigValue) -> std::result::Result<Self, Self::Error> {
        value
            .0
            .and_then(|v| {
                v.as_u64().or_else(|| if let Value::String(s) = v { s.parse().ok() } else { None })
            })
            .ok_or(anyhow!("no configuration Number value found").into())
    }
}

impl ValueStrategy for i64 {
    fn handle_arrays<'a, T: Fn(Value) -> Option<Value> + Sync>(
        next: &'a T,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a> {
        flatten(next)
    }

    fn validate_query(query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError> {
        match query.select {
            SelectMode::First => Ok(()),
            SelectMode::All => Err(anyhow!(ADDITIVE_RETURN_ERR).into()),
        }
    }
}

impl TryFrom<ConfigValue> for i64 {
    type Error = ConfigError;

    fn try_from(value: ConfigValue) -> std::result::Result<Self, Self::Error> {
        value
            .0
            .and_then(|v| {
                v.as_i64().or_else(|| if let Value::String(s) = v { s.parse().ok() } else { None })
            })
            .ok_or(anyhow!("no configuration Number value found").into())
    }
}

impl ValueStrategy for bool {
    fn handle_arrays<'a, T: Fn(Value) -> Option<Value> + Sync>(
        next: &'a T,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a> {
        flatten(next)
    }

    fn validate_query(query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError> {
        match query.select {
            SelectMode::First => Ok(()),
            SelectMode::All => Err(anyhow!(ADDITIVE_RETURN_ERR).into()),
        }
    }
}

impl TryFrom<ConfigValue> for bool {
    type Error = ConfigError;

    fn try_from(value: ConfigValue) -> std::result::Result<Self, Self::Error> {
        value
            .0
            .and_then(|v| {
                v.as_bool().or_else(|| if let Value::String(s) = v { s.parse().ok() } else { None })
            })
            .ok_or(anyhow!("no configuration Boolean value found").into())
    }
}

impl ValueStrategy for PathBuf {
    fn handle_arrays<'a, T: Fn(Value) -> Option<Value> + Sync>(
        next: &'a T,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a> {
        flatten(next)
    }

    fn validate_query(query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError> {
        match query.select {
            SelectMode::First => Ok(()),
            SelectMode::All => Err(anyhow!(ADDITIVE_RETURN_ERR).into()),
        }
    }
}

impl TryFrom<ConfigValue> for PathBuf {
    type Error = ConfigError;

    fn try_from(value: ConfigValue) -> std::result::Result<Self, Self::Error> {
        value
            .0
            .and_then(|v| v.as_str().map(|s| PathBuf::from(s.to_string())))
            .ok_or(anyhow!("no configuration value found").into())
    }
}

impl<T: TryFrom<ConfigValue>> ValueStrategy for Vec<T> {
    fn handle_arrays<'a, F: Fn(Value) -> Option<Value> + Sync>(
        next: &'a F,
    ) -> Box<dyn Fn(Value) -> Option<Value> + Send + Sync + 'a> {
        filter(next)
    }

    fn validate_query(_query: &ConfigQuery<'_>) -> std::result::Result<(), ConfigError> {
        Ok(())
    }
}

impl<T: TryFrom<ConfigValue>> TryFrom<ConfigValue> for Vec<T> {
    type Error = ConfigError;

    fn try_from(value: ConfigValue) -> std::result::Result<Self, Self::Error> {
        value
            .0
            .and_then(|val| match val.as_array() {
                Some(v) => {
                    let result: Vec<T> = v
                        .iter()
                        .filter_map(|i| ConfigValue(Some(i.clone())).try_into().ok())
                        .collect();
                    if result.len() > 0 {
                        Some(result)
                    } else {
                        None
                    }
                }
                None => ConfigValue(Some(val)).try_into().map(|x| vec![x]).ok(),
            })
            .ok_or(anyhow!("no configuration value found").into())
    }
}

/// Merge's `Value` b into `Value` a.
pub fn merge(a: &mut Value, b: &Value) {
    match (a, b) {
        (&mut Value::Object(ref mut a), &Value::Object(ref b)) => {
            for (k, v) in b.iter() {
                self::merge(a.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
        (a, b) => *a = b.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_merge() {
        let mut proto = Value::Null;
        let a = json!({
            "list": [ "first", "second" ],
            "string": "This is a string",
            "object" : {
                "foo" : "foo-prime",
                "bar" : "bar-prime"
            }
        });
        let b = json!({
            "list": [ "third" ],
            "title": "This is a title",
            "otherObject" : {
                "yourHonor" : "I object!"
            }
        });
        merge(&mut proto, &a);
        assert_eq!(proto["list"].as_array().unwrap()[0].as_str().unwrap(), "first");
        assert_eq!(proto["list"].as_array().unwrap()[1].as_str().unwrap(), "second");
        assert_eq!(proto["string"].as_str().unwrap(), "This is a string");
        assert_eq!(proto["object"]["foo"].as_str().unwrap(), "foo-prime");
        assert_eq!(proto["object"]["bar"].as_str().unwrap(), "bar-prime");
        merge(&mut proto, &b);
        assert_eq!(proto["list"].as_array().unwrap()[0].as_str().unwrap(), "third");
        assert_eq!(proto["title"].as_str().unwrap(), "This is a title");
        assert_eq!(proto["string"].as_str().unwrap(), "This is a string");
        assert_eq!(proto["object"]["foo"].as_str().unwrap(), "foo-prime");
        assert_eq!(proto["object"]["bar"].as_str().unwrap(), "bar-prime");
        assert_eq!(proto["otherObject"]["yourHonor"].as_str().unwrap(), "I object!");
    }
}
