// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::*,
    errors::ffx_bail,
    ffx_daemon::is_daemon_running_at_path,
    fuchsia_async::TimeoutExt,
    once_cell::sync::OnceCell,
    serde::Serialize,
    serde_json::Value,
    std::borrow::Cow,
    std::process::{Child, ExitStatus, Stdio},
    std::time::Instant,
    std::{env, io::Write, path::PathBuf, process::Command, time::Duration},
    std::{future::Future, pin::Pin},
    tempfile::TempDir,
    termion::is_tty,
};

pub mod asserts;

const FUCHSIA_SSH_KEY: &'static str = "FUCHSIA_SSH_KEY";
static SSH_KEY_PATH: OnceCell<String> = OnceCell::new();

/// Get the target nodename we're expected to interact with in this test, or
/// pick the first discovered target. If nodename is set via $FUCHSIA_NODENAME
/// that is returned, if the nodename is not given, and zero targets are found,
/// this is also an error.
pub async fn get_target_nodename() -> Result<String> {
    if let Ok(nodename) = std::env::var("FUCHSIA_NODENAME") {
        return Ok(nodename);
    }

    let isolate = Isolate::new("initial-target-discovery")?;

    // ensure a daemon is spun up first, so we have a moment to discover targets.
    let start = Instant::now();
    loop {
        let out = isolate.ffx(&["ffx", "target", "list"]).await?;
        if out.stdout.len() > 10 {
            break;
        }
        if start.elapsed() > Duration::from_secs(5) {
            bail!("No targets found after 5s")
        }
    }

    let out = isolate.ffx(&["target", "list", "-f", "j"]).await.context("getting target list")?;

    ensure!(out.status.success(), "Looking up a target name failed: {:?}", out);

    let targets: Value =
        serde_json::from_str(&out.stdout).context("parsing output from target list")?;

    let targets = targets.as_array().ok_or(anyhow!("expected target list ot return an array"))?;

    let target = targets
        .iter()
        .find(|target| {
            target["nodename"] != ""
                && target["target_state"]
                    .as_str()
                    .map(|s| s.to_lowercase().contains("product"))
                    .unwrap_or(false)
        })
        .ok_or(anyhow!("did not find any named targets in a product state"))?;
    target["nodename"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or(anyhow!("expected product state target to have a nodename"))
}

#[derive(Debug)]
pub struct CommandOutput {
    pub status: ExitStatus,
    pub stdout: String,
    pub stderr: String,
}

pub struct Isolate {
    _tmpdir: TempDir,

    own_path: PathBuf,
    home_dir: PathBuf,
    xdg_config_home: PathBuf,
    pub ascendd_path: PathBuf,
}

impl Isolate {
    /// new creates a new isolated environment for ffx to run in, including a
    /// user level configuration that isolates the ascendd socket into a temporary
    /// directory. If $FUCHSIA_TEST_OUTDIR is set, then it is used as the log
    /// directory. The isolated environment is torn down when the Isolate is
    /// dropped, which will attempt to terminate any running daemon and then
    /// remove all isolate files.
    pub fn new(name: &str) -> Result<Isolate> {
        let tmpdir = tempfile::Builder::new().prefix(name).tempdir()?;
        let home_dir = tmpdir.path().join("user-home");
        let tmp_dir = tmpdir.path().join("tmp");

        let log_dir = if let Ok(d) = std::env::var("FUCHSIA_TEST_OUTDIR") {
            PathBuf::from(d)
        } else {
            tmpdir.path().join("log")
        };

        for dir in [&home_dir, &tmp_dir, &log_dir].iter() {
            std::fs::create_dir_all(dir)?;
        }

        let ascendd_path = tmp_dir.join("ascendd");

        let metrics_path = home_dir.join(".fuchsia/metrics");
        std::fs::create_dir_all(&metrics_path)?;
        // Mark that analytics are disabled
        std::fs::write(metrics_path.join("analytics-status"), "0")?;
        // Mark that the notice has been given
        std::fs::write(metrics_path.join("ffx"), "1")?;

        let xdg_config_home = if cfg!(target_os = "macos") {
            home_dir.join("Library/Preferences")
        } else {
            home_dir.join(".local/share")
        };

        let user_config_dir = xdg_config_home.join("Fuchsia/ffx/config");
        std::fs::create_dir_all(&user_config_dir)?;

        std::fs::write(
            user_config_dir.join("config.json"),
            serde_json::to_string(&UserConfig::for_test(
                log_dir.to_string_lossy(),
                ascendd_path.to_string_lossy(),
            ))?,
        )?;

        std::fs::write(
            user_config_dir.join(".ffx_env"),
            serde_json::to_string(&FfxEnvConfig::for_test(
                user_config_dir.join("config.json").to_string_lossy(),
            ))?,
        )?;

        let own_path = Isolate::get_own_path();

        Ok(Isolate { _tmpdir: tmpdir, own_path, home_dir, xdg_config_home, ascendd_path })
    }

    fn get_own_path() -> PathBuf {
        let ffx_path = env::current_exe().expect("could not determine own path");
        // when we daemonize, our path will change to /, so get the canonical path before that occurs.
        std::fs::canonicalize(ffx_path).expect("could not canonicalize own path")
    }

    fn ffx_cmd(&self, args: &[&str]) -> std::process::Command {
        let mut cmd = Command::new(&self.own_path);
        cmd.args(args);
        cmd.env_clear();

        // Pass along all temp related variables, so as to avoid anything
        // falling back to writing into /tmp. In our CI environment /tmp is
        // extremely limited, whereas invocations of tests are provided
        // dedicated temporary areas.
        for (var, val) in std::env::vars() {
            if var.contains("TEMP") || var.contains("TMP") {
                cmd.env(var, val);
            } else if var == FUCHSIA_SSH_KEY {
                cmd.env(var, val);
            }
        }

        if let Some(key) = SSH_KEY_PATH.get() {
            cmd.env(FUCHSIA_SSH_KEY, key);
        }

        cmd.env("HOME", &*self.home_dir);
        cmd.env("XDG_CONFIG_HOME", &*self.xdg_config_home);
        cmd.env("ASCENDD", &*self.ascendd_path);
        cmd
    }

    pub fn ffx_spawn(&self, args: &[&str]) -> Result<Child> {
        let mut cmd = self.ffx_cmd(args);
        let child = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
        Ok(child)
    }

    pub async fn ffx(&self, args: &[&str]) -> Result<CommandOutput> {
        let mut cmd = self.ffx_cmd(args);
        fuchsia_async::unblock(move || {
            let out = cmd.output().context("failed to execute")?;
            let stdout = String::from_utf8(out.stdout).context("convert from utf8")?;
            let stderr = String::from_utf8(out.stderr).context("convert from utf8")?;
            Ok::<_, anyhow::Error>(CommandOutput { status: out.status, stdout, stderr })
        })
        .await
    }
}

impl Drop for Isolate {
    fn drop(&mut self) {
        let path = self.ascendd_path.to_string_lossy().to_string();
        if is_daemon_running_at_path(path) {
            let mut cmd = self.ffx_cmd(&["daemon", "stop"]);
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::null());
            cmd.stderr(Stdio::null());
            match cmd.spawn().map(|mut child| child.wait()) {
                Ok(_) => {}
                Err(e) => log::info!("Failure calling daemon stop: {:#?}", e),
            }
        }
    }
}

/// cleanup tries to give the daemon a chance to exit but eventually runs pkill to ensure nothing
/// is left running.
async fn cleanup() -> Result<()> {
    let max_attempts = 4;
    let daemon_start_arg = "(^|/)ffx (-.* )?daemon start$";

    for _ in 0..max_attempts {
        let did_find_daemon = fuchsia_async::unblock(move || {
            let status = Command::new("pgrep").arg("-f").arg(daemon_start_arg).status()?;
            Ok::<_, anyhow::Error>(status.success())
        })
        .await?;

        if did_find_daemon {
            // Daemon stop waits 20ms before exiting so we try to avoid a race by waiting 50ms here
            fuchsia_async::Timer::new(Duration::from_millis(50)).await;
            continue;
        } else {
            return Ok(());
        }
    }

    // Success here means that pkill was able to find something to kill so that means a daemon
    // was still running that we did not expect to be running. We return an error here to make
    // this a failure of the test suite as a whole to find out when this is happening.
    let did_kill_daemon = Command::new("pkill").arg("-f").arg(daemon_start_arg).status()?.success();
    if did_kill_daemon {
        ffx_bail!("A daemon was killed that was not supposed to be running")
    } else {
        Ok(())
    }
}

/// run runs the given set of tests printing results to stdout and exiting
/// with 0 or 1 if the tests passed or failed, respectively.
pub async fn run(
    tests: Vec<TestCase>,
    timeout: Duration,
    case_timeout: Duration,
    ssh_key_path: Option<String>,
) -> Result<()> {
    let mut writer = std::io::stdout();
    let color = is_tty(&writer);

    if let Some(path) = ssh_key_path {
        SSH_KEY_PATH.set(path).map_err(|_| anyhow!("Attempted to set SSH_KEY_PATH twice"))?;
    }

    let test_result = async {
        let num_tests = tests.len();

        writeln!(&mut writer, "1..{}", num_tests)?;

        let mut num_errs: usize = 0;
        for (i, tc) in tests.iter().enumerate().map(|(i, tc)| (i + 1, tc)) {
            match (tc.f)()
                .on_timeout(case_timeout, || ffx_bail!("timed out after {:?}", case_timeout))
                .await
            {
                Ok(()) => {
                    writeln!(
                        &mut writer,
                        "{}ok {}{} - {}",
                        green(color),
                        i,
                        nocol(color),
                        tc.name
                    )?;
                }
                Err(err) => {
                    writeln!(
                        &mut writer,
                        "{}not ok {}{} - {} {:?}",
                        red(color),
                        i,
                        nocol(color),
                        tc.name,
                        err
                    )?;
                    num_errs = num_errs + 1;
                }
            }
        }

        if num_errs != 0 {
            ffx_bail!("{}{}/{} failed{}", red(color), num_errs, num_tests, nocol(color));
        } else {
            writeln!(
                &mut writer,
                "{}{}/{} passed{}",
                green(color),
                num_tests,
                num_tests,
                nocol(color)
            )?;
        }

        Ok(())
    }
    .on_timeout(timeout, || ffx_bail!("timed out after {:?}", timeout))
    .await;

    let cleanup_result = cleanup().await;

    test_result.and(cleanup_result)
}

fn green(color: bool) -> &'static str {
    if color {
        termion::color::Green.fg_str()
    } else {
        ""
    }
}
fn red(color: bool) -> &'static str {
    if color {
        termion::color::Red.fg_str()
    } else {
        ""
    }
}
fn nocol(color: bool) -> &'static str {
    if color {
        termion::color::Reset.fg_str()
    } else {
        ""
    }
}

#[macro_export]
macro_rules! tests {
    ( $( $x:expr ),* $(,)* ) => {
        {
            let mut temp_vec = Vec::new();
            $(
                temp_vec.push($crate::test::TestCase::new(stringify!($x), move || Box::pin($x())));
            )*
            temp_vec
        }
    };
}

pub type TestFn = fn() -> Pin<Box<dyn Future<Output = Result<()>>>>;

pub struct TestCase {
    name: &'static str,
    f: TestFn,
}

impl TestCase {
    pub fn new(name: &'static str, f: TestFn) -> Self {
        Self { name, f }
    }
}

#[derive(Serialize)]
struct UserConfig<'a> {
    log: UserConfigLog<'a>,
    overnet: UserConfigOvernet<'a>,
    test: UserConfigTest,
}

#[derive(Serialize)]
struct UserConfigLog<'a> {
    enabled: bool,
    dir: Cow<'a, str>,
}

#[derive(Serialize)]
struct UserConfigOvernet<'a> {
    socket: Cow<'a, str>,
}

#[derive(Serialize)]
struct UserConfigTest {
    #[serde(rename(serialize = "is-isolated"))]
    is_isolated: bool,
}

impl<'a> UserConfig<'a> {
    fn for_test(dir: Cow<'a, str>, socket: Cow<'a, str>) -> Self {
        Self {
            log: UserConfigLog { enabled: true, dir },
            overnet: UserConfigOvernet { socket },
            test: UserConfigTest { is_isolated: true },
        }
    }
}

#[derive(Serialize)]
struct FfxEnvConfig<'a> {
    user: Cow<'a, str>,
    build: Option<&'static str>,
    global: Option<&'static str>,
}

impl<'a> FfxEnvConfig<'a> {
    fn for_test(user: Cow<'a, str>) -> Self {
        Self { user, build: None, global: None }
    }
}
