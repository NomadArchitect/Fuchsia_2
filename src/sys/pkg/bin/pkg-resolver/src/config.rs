// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::anyhow,
    fuchsia_syslog::{fx_log_err, fx_log_info},
    serde::Deserialize,
    std::{
        fs::File,
        io::{BufReader, Read},
    },
    thiserror::Error,
};

/// Static service configuration options.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Config {
    enable_dynamic_configuration: bool,
}

impl Config {
    pub fn enable_dynamic_configuration(&self) -> bool {
        self.enable_dynamic_configuration
    }

    pub fn load_from_config_data_or_default() -> Config {
        let f = match File::open("/config/data/config.json") {
            Ok(f) => f,
            Err(e) => {
                fx_log_info!("no config found, using defaults: {:#}", anyhow!(e));
                return Config::default();
            }
        };

        Self::load(BufReader::new(f)).unwrap_or_else(|e| {
            fx_log_err!("unable to load config, using defaults: {:#}", anyhow!(e));
            Config::default()
        })
    }

    fn load(r: impl Read) -> Result<Config, ConfigLoadError> {
        #[derive(Debug, Deserialize)]
        #[serde(deny_unknown_fields)]
        struct ParseConfig {
            enable_dynamic_configuration: bool,
        }

        let parse_config = serde_json::from_reader::<_, ParseConfig>(r)?;

        Ok(Config { enable_dynamic_configuration: parse_config.enable_dynamic_configuration })
    }
}

#[derive(Debug, Error)]
enum ConfigLoadError {
    #[error("parse error")]
    Parse(#[from] serde_json::Error),
}

#[cfg(test)]
mod tests {
    use {super::*, matches::assert_matches, serde_json::json};

    fn verify_load(input: serde_json::Value, expected: Config) {
        assert_eq!(
            Config::load(input.to_string().as_bytes()).expect("json value to be valid"),
            expected
        );
    }

    #[test]
    fn test_load_valid_configs() {
        for val in [true, false].iter() {
            verify_load(
                json!({
                    "enable_dynamic_configuration": *val,
                }),
                Config { enable_dynamic_configuration: *val },
            );
        }
    }

    #[test]
    fn test_load_errors_on_unknown_field() {
        assert_matches!(
            Config::load(
                json!({
                    "enable_dynamic_configuration": false,
                    "unknown_field": 3
                })
                .to_string()
                .as_bytes()
            ),
            Err(ConfigLoadError::Parse(_))
        );
    }

    #[test]
    fn test_no_config_data_is_default() {
        assert_eq!(Config::load_from_config_data_or_default(), Config::default());
    }

    #[test]
    fn test_default_disables_dynamic_configuraiton() {
        assert_eq!(Config::default().enable_dynamic_configuration, false);
    }
}
