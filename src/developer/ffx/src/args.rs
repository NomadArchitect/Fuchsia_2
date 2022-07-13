// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use argh::EarlyExit;

use {
    argh::FromArgs, ffx_config::FfxConfigBacked, ffx_core::ffx_command,
    ffx_lib_sub_command::SubCommand, ffx_writer::Format,
};

/// The environment variable name used for overriding the command name in help
/// output.
const FFX_WRAPPER_INVOKE: &'static str = "FFX_WRAPPER_INVOKE";

#[ffx_command()]
#[derive(FfxConfigBacked, FromArgs, Debug, PartialEq)]
/// Fuchsia's developer tool
pub struct Ffx {
    #[argh(option, short = 'c')]
    /// override configuration values (key=value or json)
    pub config: Vec<String>,

    #[argh(option, short = 'e')]
    /// override the path to the environment configuration file (file path)
    pub env: Option<String>,

    #[argh(option)]
    /// produce output for a machine in the specified format; available formats: "json",
    /// "json-pretty"
    pub machine: Option<Format>,

    #[argh(option)]
    /// create a stamp file at the given path containing the exit code
    pub stamp: Option<String>,

    #[argh(option, short = 't')]
    #[ffx_config_default("target.default")]
    /// apply operations across single or multiple targets
    pub target: Option<String>,

    #[argh(option, short = 'T')]
    #[ffx_config_default(key = "proxy.timeout_secs", default = "1.0")]
    /// override default proxy timeout
    pub timeout: Option<f64>,

    #[argh(option, short = 'l', long = "log-level")]
    #[ffx_config_default(key = "log.level", default = "Debug")]
    /// sets the log level for ffx output (default = Debug). Other possible values are Info, Error,
    /// Warn, and Trace. Can be persisted via log.level config setting.
    pub log_level: Option<String>,

    #[argh(switch, short = 'v', long = "verbose")]
    /// logs ffx output to stdio according to log level
    pub verbose: bool,

    #[argh(subcommand)]
    pub subcommand: Option<SubCommand>,
}

impl Ffx {
    /// Extract the base cmd from a path
    fn base_cmd<'a>(path: &'a str) -> &'a str {
        std::path::Path::new(path).file_name().map(|s| s.to_str()).flatten().unwrap_or(path)
    }

    /// Extract the command name from the given argument list, allowing for an overridden command name
    /// from a wrapper invocation so we provide useful information to the user. If the override has spaces, it will
    /// be split into multiple commands.
    ///
    /// Returns a tuple of the command and the remaining arguments
    fn prepare_args<'a>(
        wrapper_name: Option<&'a str>,
        argv: &'a Vec<String>,
    ) -> (Vec<&'a str>, Vec<&'a str>) {
        let mut args = argv.iter().map(String::as_str);
        let arg0 = args.next().expect("No first argument in argument vector");
        let remain = Vec::from_iter(args);
        let cmd =
            wrapper_name.map_or_else(|| vec![Self::base_cmd(arg0)], |s| s.split(" ").collect());
        (cmd, remain)
    }

    /// Create a `FromArgs` type from the current process's `env::args`, potentially
    /// getting an overridden command name from the environment from FFX_WRAPPER_INVOKE
    /// by wrapper scripts.
    pub fn from_env() -> Result<Self, EarlyExit> {
        let argv = std::env::args().collect();
        let wrapper_name = std::env::var(FFX_WRAPPER_INVOKE).ok();
        let (cmd, remain) = Ffx::prepare_args(wrapper_name.as_deref(), &argv);
        Ffx::from_args(
            &Vec::from_iter(cmd.clone().into_iter()),
            &Vec::from_iter(remain.into_iter()),
        )
        .map_err(|early_exit| {
            println!("{}", early_exit.output);
            println!(
                "See '{cmd} help <command>' for more information on a specific command.",
                cmd = cmd.join(" ")
            );
            early_exit
        })
    }
}

/// Create a `FromArgs` type from the current process's `env::args`, potentially
/// getting an overridden command name from the environment from FFX_WRAPPER_INVOKE
/// by wrapper scripts.
///
/// This function will exit early from the current process if argument parsing
/// was unsuccessful or if information like `--help` was requested.
pub fn from_env() -> Ffx {
    Ffx::from_env().unwrap_or_else(|early_exit| {
        std::process::exit(match early_exit.status {
            Ok(()) => 0,
            Err(()) => 1,
        })
    })
}

/// Create a string of the current process's `env::args` that replaces user-supplied parameter
/// values with the parameter name to enable safe analytics data collection.
///
/// This function will exit early from the current process if argument parsing
/// was unsuccessful or if information like `--help` was requested.
pub fn redact_arg_values() -> String {
    let argv = std::env::args().collect();
    let wrapper_name = std::env::var(FFX_WRAPPER_INVOKE).ok();
    let (cmd, remain) = Ffx::prepare_args(wrapper_name.as_deref(), &argv);
    let x = Ffx::redact_arg_values(
        &Vec::from_iter(cmd.into_iter()),
        &Vec::from_iter(remain.into_iter()),
    );
    match x {
        Ok(s) => s[1..].join(" "),
        Err(e) => e.output,
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn cmd_only_last_component() {
        let args = vec!["test/things/ffx", "--help"].map(String::from);
        let (cmd, remain) = Ffx::prepare_args(args, None);
        assert_eq!(cmd, vec!["ffx"]);
        assert_eq!(remain, vec!["--help"]);
    }

    #[test]
    fn cmd_override_invoke() {
        let args = vec!["test/things/ffx", "--help"].map(String::from);
        let (cmd, remain) = Ffx::prepare_args(args, Some("tools/ffx".to_owned()));
        assert_eq!(cmd, vec!["tools/ffx"]);
        assert_eq!(remain, vec!["--help"]);
    }

    #[test]
    fn cmd_override_multiple_terms_invoke() {
        let args = vec!["test/things/ffx", "--help"].map(String::from);
        let (cmd, remain) = Ffx::prepare_args(args, Some("fx ffx".to_owned()));
        assert_eq!(cmd, vec!["fx", "ffx"]);
        assert_eq!(remain, vec!["--help"]);
    }
}
