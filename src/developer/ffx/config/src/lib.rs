// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::api::{
    get_config, validate_type,
    value::{ConfigValue, ValueStrategy},
    ConfigError,
};
use crate::cache::load_config;
use crate::environment::Environment;
use crate::paths::get_default_user_file_path;
use crate::storage::Config;
use analytics::{is_opted_in, set_opt_in_status};
use anyhow::{anyhow, bail, Context, Result};
use std::{convert::From, fs::File, io::Write, path::PathBuf};

pub use config_macros::FfxConfigBacked;

pub mod api;
pub mod environment;
pub mod logging;
pub mod sdk;

mod cache;
mod mapping;
mod nested;
mod paths;
mod runtime;
mod storage;

pub use api::query::{ConfigQuery, SelectMode};

pub use cache::{env_file, init, test_env_file, test_init};

pub use paths::default_env_path;

const SDK_TYPE_IN_TREE: &str = "in-tree";
const SDK_NOT_FOUND_HELP: &str = "\
SDK directory could not be found. Please set with
`ffx sdk set root <PATH_TO_SDK_DIR>`\n
If you are developing in the fuchsia tree, ensure \
that you are running the `ffx` command (in $FUCHSIA_DIR/.jiri_root) or `fx ffx`, not a built binary.
Running the binary directly is not supported in the fuchsia tree.\n\n";

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum ConfigLevel {
    Default,
    Build,
    Global,
    User,
    Runtime,
}

impl argh::FromArgValue for ConfigLevel {
    fn from_arg_value(val: &str) -> Result<Self, String> {
        match val {
            "u" | "user" => Ok(ConfigLevel::User),
            "b" | "build" => Ok(ConfigLevel::Build),
            "g" | "global" => Ok(ConfigLevel::Global),
            _ => Err(String::from(
                "Unrecognized value. Possible values are \"user\",\"build\",\"global\".",
            )),
        }
    }
}

/// Creates a [`ConfigQuery`] against the global config cache and environment.
///
/// Example:
///
/// ```no_run
/// use ffx_config::ConfigLevel;
/// use ffx_config::SelectMode;
///
/// let query = ffx_config::build()
///     .name("testing")
///     .level(Some(ConfigLevel::Build))
///     .build(Some("/tmp/build.json"))
///     .select(SelectMode::All);
/// let value = query.get().await?;
/// ```
pub fn build<'a>() -> ConfigQuery<'a> {
    ConfigQuery::default()
}

/// Creates a [`ConfigQuery`] against the global config cache and environment,
/// using the provided value converted in to a base query.
///
/// Example:
///
/// ```no_run
/// ffx_config::query("a_key").get();
/// ffx_config::query(ffx_config::ConfigLevel::User).get();
/// ```
pub fn query<'a>(with: impl Into<ConfigQuery<'a>>) -> ConfigQuery<'a> {
    with.into()
}

/// A shorthand for the very common case of querying a value from the global config
/// cache and environment, using the provided value converted into a query.
pub async fn get<'a, T, U>(with: U) -> std::result::Result<T, T::Error>
where
    T: TryFrom<ConfigValue> + ValueStrategy,
    <T as std::convert::TryFrom<ConfigValue>>::Error: std::convert::From<ConfigError>,
    U: Into<ConfigQuery<'a>>,
{
    query(with).get().await
}

fn check_config_files(level: &ConfigLevel, build_dir: &Option<String>) -> Result<()> {
    let e = env_file().ok_or(anyhow!("Could not find environment file"))?;
    let mut environment = Environment::load(&e)?;
    match level {
        ConfigLevel::User => {
            if let None = environment.user {
                let default_path = get_default_user_file_path();
                // This will override the config file if it exists.  This would happen anyway
                // because of the cache.
                let mut file = File::create(&default_path).context("opening write buffer")?;
                file.write_all(b"{}").context("writing default user configuration file")?;
                file.sync_all().context("syncing default user configuration file to filesystem")?;
                environment.user = Some(
                    default_path
                        .to_str()
                        .map(|s| s.to_string())
                        .context("home path is not proper unicode")?,
                );
                environment.save(&e)?;
            }
        }
        ConfigLevel::Global => {
            if let None = environment.global {
                bail!(
                    "Global configuration not set. Use 'ffx config env set' command \
                     to setup the environment."
                );
            }
        }
        ConfigLevel::Build => match build_dir {
            Some(b_dir) => match environment.build {
                None => bail!(
                    "Build configuration not set for '{}'. Use 'ffx config env set' command \
                     to setup the environment.",
                    b_dir
                ),
                Some(b) => {
                    if let None = b.get(b_dir) {
                        bail!(
                            "Build configuration not set for '{}'. Use 'ffx config env \
                             set' command to setup the environment.",
                            b_dir
                        );
                    }
                }
            },
            None => bail!("Cannot set a build configuration without a build directory."),
        },
        _ => bail!("This config level is not writable."),
    }
    Ok(())
}

fn save_config(config: &mut Config, build_dir: &Option<String>) -> Result<()> {
    let e = env_file().ok_or(anyhow!("Could not find environment file"))?;
    let env = Environment::load(&e)?;
    let build = build_dir.as_ref().and_then(|b| env.build.as_ref().and_then(|c| c.get(b)));
    config.save(env.global.as_ref(), build, env.user.as_ref())
}

pub async fn print_config<W: Write>(mut writer: W, build_dir: &Option<String>) -> Result<()> {
    let config = load_config(build_dir).await?;
    let read_guard = config.read().await;
    writeln!(writer, "{}", *read_guard).context("displaying config")
}

pub async fn get_log_dirs() -> Result<Vec<String>> {
    match query("log.dir").get().await {
        Ok(log_dirs) => Ok(log_dirs),
        Err(e) => errors::ffx_bail!("Failed to load host log directories from ffx config: {:?}", e),
    }
}

pub async fn get_sdk() -> Result<sdk::Sdk> {
    // All gets in this function should declare that they don't want the build directory searched, because
    // if there is a build directory it *is* generally the sdk.
    let sdk_root = query("sdk.root").get().await;
    let sdk_type = query("sdk.type").get().await.unwrap_or("".to_string());
    match (sdk_root, sdk_type) {
        (Ok(manifest), sdk_type) => {
            if sdk_type == SDK_TYPE_IN_TREE {
                let module_manifest: Option<String> = query("sdk.module").get().await.ok();
                sdk::Sdk::from_build_dir(manifest, module_manifest)
            } else {
                sdk::Sdk::from_sdk_dir(manifest)
            }
        }
        (Err(e), sdk_type) => {
            if sdk_type != SDK_TYPE_IN_TREE {
                let path = std::env::current_exe().map_err(|e| {
                    errors::ffx_error!(
                        "{}Error was: failed to get current ffx exe path for SDK root: {:?}",
                        SDK_NOT_FOUND_HELP,
                        e
                    )
                })?;

                match find_sdk_root(path) {
                    Ok(Some(root)) => return sdk::Sdk::from_sdk_dir(root),
                    Ok(None) => {
                        errors::ffx_bail!(
                            "{}Could not find an SDK manifest in any parent of ffx's directory.",
                            SDK_NOT_FOUND_HELP,
                        );
                    }
                    Err(e) => {
                        errors::ffx_bail!("{}Error was: {:?}", SDK_NOT_FOUND_HELP, e);
                    }
                }
            }

            errors::ffx_bail!("{}Error was: {:?}", SDK_NOT_FOUND_HELP, e);
        }
    }
}

fn find_sdk_root(start_path: PathBuf) -> Result<Option<PathBuf>> {
    let mut path = std::fs::canonicalize(start_path.clone())
        .context(format!("canonicalizing ffx path {:?}", start_path))?;

    loop {
        path =
            if let Some(parent) = path.parent() { parent.to_path_buf() } else { return Ok(None) };

        if path.join("meta").join("manifest.json").exists() {
            return Ok(Some(path));
        }
    }
}

pub async fn set_metrics_status(value: bool) -> Result<()> {
    set_opt_in_status(value).await
}

pub async fn show_metrics_status<W: Write>(mut writer: W) -> Result<()> {
    let state = match is_opted_in().await {
        true => "enabled",
        false => "disabled",
    };
    writeln!(&mut writer, "Analytics data collection is {}", state)?;
    Ok(())
}

////////////////////////////////////////////////////////////////////////////////
// tests
#[cfg(test)]
mod test {
    use super::*;
    // This is to get the FfxConfigBacked derive to compile, as it
    // creates a token stream referencing `ffx_config` on the inside.
    use crate as ffx_config;
    use serde_json::{json, Value};
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[test]
    fn test_check_config_files_fails() {
        let levels = vec![
            ConfigLevel::Runtime,
            ConfigLevel::Default,
            ConfigLevel::Global,
            ConfigLevel::Build,
        ];
        let build_dir = None;
        for level in levels {
            let result = check_config_files(&level, &build_dir);
            assert!(result.is_err());
        }
    }

    #[test]
    fn test_validating_types() {
        assert!(validate_type::<String>(json!("test")).is_some());
        assert!(validate_type::<String>(json!(1)).is_none());
        assert!(validate_type::<String>(json!(false)).is_none());
        assert!(validate_type::<String>(json!(true)).is_none());
        assert!(validate_type::<String>(json!({"test": "whatever"})).is_none());
        assert!(validate_type::<String>(json!(["test", "test2"])).is_none());
        assert!(validate_type::<bool>(json!(true)).is_some());
        assert!(validate_type::<bool>(json!(false)).is_some());
        assert!(validate_type::<bool>(json!("true")).is_some());
        assert!(validate_type::<bool>(json!("false")).is_some());
        assert!(validate_type::<bool>(json!(1)).is_none());
        assert!(validate_type::<bool>(json!("test")).is_none());
        assert!(validate_type::<bool>(json!({"test": "whatever"})).is_none());
        assert!(validate_type::<bool>(json!(["test", "test2"])).is_none());
        assert!(validate_type::<u64>(json!(2)).is_some());
        assert!(validate_type::<u64>(json!(100)).is_some());
        assert!(validate_type::<u64>(json!("100")).is_some());
        assert!(validate_type::<u64>(json!("0")).is_some());
        assert!(validate_type::<u64>(json!(true)).is_none());
        assert!(validate_type::<u64>(json!("test")).is_none());
        assert!(validate_type::<u64>(json!({"test": "whatever"})).is_none());
        assert!(validate_type::<u64>(json!(["test", "test2"])).is_none());
        assert!(validate_type::<PathBuf>(json!("/")).is_some());
        assert!(validate_type::<PathBuf>(json!("test")).is_some());
        assert!(validate_type::<PathBuf>(json!(true)).is_none());
        assert!(validate_type::<PathBuf>(json!({"test": "whatever"})).is_none());
        assert!(validate_type::<PathBuf>(json!(["test", "test2"])).is_none());
    }

    #[test]
    fn test_converting_array() -> Result<()> {
        let c = |val: Value| -> ConfigValue { ConfigValue(Some(val)) };
        let conv_elem: Vec<String> = c(json!("test")).try_into()?;
        assert_eq!(1, conv_elem.len());
        let conv_string: Vec<String> = c(json!(["test", "test2"])).try_into()?;
        assert_eq!(2, conv_string.len());
        let conv_bool: Vec<bool> = c(json!([true, "false", false])).try_into()?;
        assert_eq!(3, conv_bool.len());
        let conv_bool_2: Vec<bool> = c(json!([36, "false", false])).try_into()?;
        assert_eq!(2, conv_bool_2.len());
        let conv_num: Vec<u64> = c(json!([3, "36", 1000])).try_into()?;
        assert_eq!(3, conv_num.len());
        let conv_num_2: Vec<u64> = c(json!([3, "false", 1000])).try_into()?;
        assert_eq!(2, conv_num_2.len());
        let bad_elem: std::result::Result<Vec<u64>, ConfigError> = c(json!("test")).try_into();
        assert!(bad_elem.is_err());
        let bad_elem_2: std::result::Result<Vec<u64>, ConfigError> = c(json!(["test"])).try_into();
        assert!(bad_elem_2.is_err());
        Ok(())
    }

    #[derive(FfxConfigBacked, Default)]
    struct TestConfigBackedStruct {
        #[ffx_config_default(key = "test.test.thing", default = "thing")]
        value: Option<String>,

        #[ffx_config_default(default = "what", key = "oops")]
        reverse_value: Option<String>,

        #[ffx_config_default(key = "other.test.thing")]
        other_value: Option<f64>,
    }

    #[derive(FfxConfigBacked, Default)] // This should just compile despite having no config.
    struct TestEmptyBackedStruct {}

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_config_backed_attribute() {
        ffx_config::test_init().expect("create test config");
        let mut empty_config_struct = TestConfigBackedStruct::default();
        assert!(empty_config_struct.value.is_none());
        assert_eq!(empty_config_struct.value().await.unwrap(), "thing");
        assert!(empty_config_struct.reverse_value.is_none());
        assert_eq!(empty_config_struct.reverse_value().await.unwrap(), "what");

        ffx_config::query("test.test.thing")
            .level(Some(ConfigLevel::User))
            .set(Value::String("config_value_thingy".to_owned()))
            .await
            .unwrap();
        ffx_config::query("other.test.thing")
            .level(Some(ConfigLevel::User))
            .set(Value::Number(serde_json::Number::from_f64(2f64).unwrap()))
            .await
            .unwrap();

        // If this is set, this should pop up before the config values.
        empty_config_struct.value = Some("wat".to_owned());
        assert_eq!(empty_config_struct.value().await.unwrap(), "wat");
        empty_config_struct.value = None;
        assert_eq!(empty_config_struct.value().await.unwrap(), "config_value_thingy");
        assert_eq!(empty_config_struct.other_value().await.unwrap().unwrap(), 2f64);
        ffx_config::query("other.test.thing")
            .level(Some(ConfigLevel::User))
            .set(Value::String("oaiwhfoiwh".to_owned()))
            .await
            .unwrap();

        // This should just compile and drop without panicking is all.
        let _ignore = TestEmptyBackedStruct {};
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_find_sdk_root_finds_root() {
        let temp = tempdir().unwrap();

        let start_path = temp.path().to_path_buf().join("test1").join("test2");
        std::fs::create_dir_all(start_path.clone()).unwrap();

        let meta_path = temp.path().to_path_buf().join("meta");
        std::fs::create_dir(meta_path.clone()).unwrap();

        std::fs::write(meta_path.join("manifest.json"), "").unwrap();

        assert_eq!(find_sdk_root(start_path).unwrap().unwrap(), temp.path().to_path_buf());
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_find_sdk_root_no_manifest() {
        let temp = tempdir().unwrap();

        let start_path = temp.path().to_path_buf().join("test1").join("test2");
        std::fs::create_dir_all(start_path.clone()).unwrap();

        let meta_path = temp.path().to_path_buf().join("meta");
        std::fs::create_dir(meta_path.clone()).unwrap();

        assert!(find_sdk_root(start_path).unwrap().is_none());
    }
}
