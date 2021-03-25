// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use {
    crate::constants::{SSH_PORT, SSH_PRIV},
    crate::target::TargetAddr,
    anyhow::{anyhow, Result},
    ffx_config::{file, get},
    std::process::Command,
};

static DEFAULT_SSH_OPTIONS: &'static [&str] = &[
    "-o CheckHostIP=no",
    "-o StrictHostKeyChecking=no",
    "-o UserKnownHostsFile=/dev/null",
    "-o ServerAliveInterval=1",
    "-o ServerAliveCountMax=10",
    "-o LogLevel=ERROR",
];

pub async fn build_ssh_command(
    addrs: Vec<TargetAddr>,
    ssh_port: Option<u16>,
    command: Vec<&str>,
) -> Result<Command> {
    if command.is_empty() {
        return Err(anyhow!("missing SSH command"));
    }

    let mut port: Option<String> = ssh_port.map(|u| u.to_string());
    if port.is_none() {
        get(SSH_PORT).await.ok().map(|p| port.replace(p));
    }
    let key: String = file(SSH_PRIV).await?;

    let mut c = Command::new("ssh");

    if let Some(p) = port {
        c.arg("-p").arg(p);
    }

    c.arg("-i").arg(key);

    let addr = addrs.iter().next().ok_or(anyhow!("no IP's for chosen target"))?;

    c.args(DEFAULT_SSH_OPTIONS).arg(format!("{}", addr)).args(&command);

    return Ok(c);
}

#[cfg(test)]
mod test {
    use {
        super::build_ssh_command,
        crate::target::TargetAddr,
        serial_test::serial,
        std::net::{IpAddr, Ipv4Addr},
    };

    #[fuchsia_async::run_singlethreaded(test)]
    #[serial]
    async fn test_empty_command_vec_produces_error() {
        let result = build_ssh_command(Vec::new(), None, vec![]).await;
        assert!(result.is_err(), "empty command vec should produce an error");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    #[serial]
    async fn test_no_ips_produces_error() {
        let result = build_ssh_command(Vec::new(), None, vec!["ls"]).await;
        assert!(result.is_err(), "target with no IP's should produce an error");
    }

    #[fuchsia_async::run_singlethreaded(test)]
    #[serial]
    async fn test_ssh_default_port() {
        let key_path = std::env::current_exe().unwrap();
        let key_path = key_path.to_str().take().unwrap();
        std::env::remove_var("FUCHSIA_SSH_PORT");
        std::env::set_var("FUCHSIA_SSH_KEY", key_path);

        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1));
        let mut addrs = Vec::new();
        addrs.push(TargetAddr::from((ip, 0)));

        // Port falls back to config:
        let result = build_ssh_command(addrs, None, vec!["ls"]).await.unwrap();
        let dbgstr = format!("{:?}", result);
        let search_string = &format!("\"-i\" \"{}\"", key_path);

        assert!(dbgstr.contains(search_string), "`{}` not found in `{}`", search_string, dbgstr);
        assert!(dbgstr.contains(&ip.to_string()), "{}", dbgstr);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    #[serial]
    async fn test_ssh_with_env_port() {
        let key_path = std::env::current_exe().unwrap();
        let key_path = key_path.to_str().take().unwrap();
        std::env::set_var("FUCHSIA_SSH_PORT", "1234");
        std::env::set_var("FUCHSIA_SSH_KEY", key_path);

        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1));
        let mut addrs = Vec::new();
        addrs.push(TargetAddr::from((ip, 0)));

        // Port falls back to config:
        let result = build_ssh_command(addrs, None, vec!["ls"]).await.unwrap();
        let dbgstr = format!("{:?}", result);
        let search_string = &format!("\"-p\" \"1234\" \"-i\" \"{}\"", key_path);
        assert!(dbgstr.contains(search_string), "`{}` not found in `{}`", search_string, dbgstr);
        assert!(dbgstr.contains(&ip.to_string()), "{}", dbgstr);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    #[serial]
    async fn test_ssh_with_target_configured_port() {
        let key_path = std::env::current_exe().unwrap();
        let key_path = key_path.to_str().take().unwrap();
        std::env::set_var("FUCHSIA_SSH_PORT", "1234");
        std::env::set_var("FUCHSIA_SSH_KEY", key_path);

        let ip = IpAddr::V4(Ipv4Addr::new(192, 168, 0, 1));
        let mut addrs = Vec::new();
        addrs.push(TargetAddr::from((ip, 0)));

        // Port, if given, takes precedence:
        let result = build_ssh_command(addrs, Some(456), vec!["ls"]).await.unwrap();
        let dbgstr = format!("{:?}", result);

        let search_string = &format!("\"-p\" \"456\" \"-i\" \"{}\"", key_path);
        assert!(dbgstr.contains(search_string), "`{}` not found in `{}`", search_string, dbgstr);
        assert!(dbgstr.contains(&ip.to_string()), "{}", dbgstr);
    }
}
