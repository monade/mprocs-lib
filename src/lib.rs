mod app;
mod client;
mod clipboard;
mod config;
mod ctl;
mod ctl_server;
#[cfg(test)]
mod ctl_test;
mod encode_term;
mod error;
mod event;
mod key;
mod keymap;
mod package_json;
mod proc;
mod protocol;
mod settings;
mod state;
mod theme;
mod ui_add_proc;
mod ui_confirm_quit;
mod ui_keymap;
mod ui_procs;
mod ui_remove_proc;
mod ui_term;
mod ui_zoom_tip;
mod yaml_val;

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use app::server_main;
use clap::{arg, command, ArgMatches};
use client::client_main;
use config::{CmdConfig, Config, ConfigContext, ProcConfig, ServerConfig};
use ctl::run_ctl;
use ctl_server::{bind_ctl_socket, ctl_server_main, CtlMessage};
use keymap::Keymap;
use package_json::load_npm_procs;
use proc::StopSignal;
use protocol::{CltToSrv, SrvToClt};
use serde_yaml::Value;
use settings::Settings;
use yaml_val::Val;

/// How to run mprocs as a library.
///
/// Built field by field with `..Default::default()`, so that new options do
/// not break the callers.
#[derive(Debug, Clone, Default)]
pub struct RunOptions {
    /// Path of the mprocs.yaml to load.
    pub yaml_path: PathBuf,
    /// When set, opens the JSON control socket on this path. Unix only.
    pub ctl_socket: Option<PathBuf>,
    /// When set, every process tees its output to `<log_dir>/<name>.log`.
    pub log_dir: Option<PathBuf>,
    /// Size beyond which a log file is truncated when the process restarts.
    /// `None` uses the default (8 MiB).
    pub log_max_bytes: Option<u64>,
}

/// Runs mprocs on a config file. Unchanged since 0.3.0: no control socket, no
/// log files. New capabilities live in [`run_mprocs_with`].
pub async fn run_mprocs(yaml_path: &str) -> anyhow::Result<()> {
    run_mprocs_with(RunOptions {
        yaml_path: yaml_path.into(),
        ..Default::default()
    })
    .await
}

/// Runs mprocs with the given options.
pub async fn run_mprocs_with(opts: RunOptions) -> anyhow::Result<()> {
    let (config, keymap) = load_config(&opts)?;
    run_client_and_server(config, keymap).await
}

/// Loads the config file named by `opts` and applies the host decisions on top
/// of it.
fn load_config(opts: &RunOptions) -> anyhow::Result<(Config, Keymap)> {
    let yaml_path = opts.yaml_path.to_str().ok_or_else(|| {
        anyhow::Error::msg(format!(
            "Config path is not valid UTF-8: {}",
            opts.yaml_path.display()
        ))
    })?;

    let config_value = Some((
            read_value(&yaml_path)?,
            ConfigContext { path: yaml_path.into() },
        ));

    let mut settings = Settings::default();

    if let Some((value, _)) = &config_value {
        settings
            .merge_value(Val::new(value)?)
            .map_err(|e| anyhow::Error::msg(format!("[{}] {}", "local config", e)))?;
    }



    let mut keymap = Keymap::new();
    settings.add_to_keymap(&mut keymap).unwrap();


    let mut config = if let Some((v, ctx)) = config_value {
        Config::from_value(&v, &ctx, &settings)?
    } else {
        Config::make_default(&settings)
    };

    // These are decisions of the host, not of the stack: they come from the
    // code, never from the yaml.
    config.ctl_socket = opts.ctl_socket.clone();
    config.log_dir = opts.log_dir.clone();
    if let Some(log_max_bytes) = opts.log_max_bytes {
        config.log_max_bytes = log_max_bytes;
    }

    Ok((config, keymap))
}
pub async fn run_app() -> anyhow::Result<()> {
    let matches = command!()
        .arg(arg!(-c --config [PATH] "Config path [default: mprocs.yaml]"))
        .arg(arg!(-s --server [PATH] "Remote control server address. Example: 127.0.0.1:4050."))
        .arg(arg!(--ctl [YAML] "Send yaml/json encoded command to running mprocs"))
        .arg(arg!(--names [NAMES] "Names for processes provided by cli arguments. Separated by comma."))
        .arg(arg!(--npm "Run scripts from package.json. Scripts are not started by default."))
        .arg(arg!(--"ctl-socket" [PATH] "Path of the JSON control socket (unix only)"))
        .arg(arg!(--"log-dir" [PATH] "Directory where each process tees its output"))
        .arg(arg!([COMMANDS]... "Commands to run (if omitted, commands from config will be run)"))
        .get_matches();

    let config_value = load_config_value(&matches)
        .map_err(|e| anyhow::Error::msg(format!("[{}] {}", "config", e)))?;

    let mut settings = Settings::default();

    // merge ~/.config/mprocs/mprocs.yaml
    settings.merge_from_xdg().map_err(|e| {
        anyhow::Error::msg(format!("[{}] {}", "global settings", e))
    })?;
    // merge ./mprocs.yaml
    if let Some((value, _)) = &config_value {
        settings
            .merge_value(Val::new(value)?)
            .map_err(|e| anyhow::Error::msg(format!("[{}] {}", "local config", e)))?;
    }

    let mut keymap = Keymap::new();
    settings.add_to_keymap(&mut keymap)?;

    let config = {
        let mut config = if let Some((v, ctx)) = config_value {
            Config::from_value(&v, &ctx, &settings)?
        } else {
            Config::make_default(&settings)
        };

        if let Some(server_addr) = matches.value_of("server") {
            config.server = Some(ServerConfig::from_str(server_addr)?);
        }

        if let Some(path) = matches.value_of("ctl-socket") {
            config.ctl_socket = Some(PathBuf::from(path));
        }

        if let Some(path) = matches.value_of("log-dir") {
            config.log_dir = Some(PathBuf::from(path));
        }

        if matches.occurrences_of("ctl") > 0 {
            return run_ctl(matches.value_of("ctl").unwrap(), &config).await;
        }

        if let Some(cmds) = matches.values_of("COMMANDS") {
            let names = matches
                .value_of("names")
                .map_or_else(|| Vec::new(), |arg| arg.split(",").collect::<Vec<_>>());
            let procs = cmds
                .into_iter()
                .enumerate()
                .map(|(i, cmd)| ProcConfig {
                    name: names
                        .get(i)
                        .map_or_else(|| cmd.to_string(), |s| s.to_string()),
                    cmd: CmdConfig::Shell {
                        shell: cmd.to_owned(),
                    },
                    env: None,
                    cwd: None,
                    autostart: true,
                    stop: StopSignal::default(),
                })
                .collect::<Vec<_>>();

            config.procs = procs;
        } else if matches.is_present("npm") {
            let procs = load_npm_procs()?;
            config.procs = procs;
        }

        config
    };

    run_client_and_server(config, keymap).await
}

async fn run_client_and_server(config: Config, keymap: Keymap) -> Result<()> {
    let (clt_tx, srv_rx) = tokio::sync::mpsc::channel::<CltToSrv>(64);
    let (srv_tx, clt_rx) = tokio::sync::mpsc::unbounded_channel::<SrvToClt>();
    let (ctl_tx, ctl_rx) = tokio::sync::mpsc::unbounded_channel::<CtlMessage>();

    // Bind before starting anything else: a socket owned by a live instance
    // has to fail loudly here, not degrade in silence.
    let ctl_socket = match &config.ctl_socket {
        Some(path) => Some(bind_ctl_socket(path)?),
        None => None,
    };

    let (exit_trigger, exit_listener) = triggered::trigger();

    let client = tokio::spawn(async { client_main(clt_tx, clt_rx).await });
    let server = tokio::spawn(async {
        server_main(config, keymap, srv_tx, srv_rx, ctl_rx).await
    });
    let ctl_server = ctl_socket.map(|socket| {
        tokio::spawn(
            async move { ctl_server_main(socket, ctl_tx, exit_listener).await },
        )
    });

    let r1 = server
        .await
        .unwrap_or_else(|err| Err(anyhow::Error::from(err)));

    // The app is gone: stop the ctl server so that it removes the socket file.
    exit_trigger.trigger();
    if let Some(ctl_server) = ctl_server {
        let _ = ctl_server.await;
    }

    let r2 = client
        .await
        .unwrap_or_else(|err| Err(anyhow::Error::from(err)));

    r1.and(r2)
        .map_err(|err| anyhow::Error::msg(err.to_string()))
}

fn load_config_value(
    matches: &ArgMatches,
) -> Result<Option<(Value, ConfigContext)>> {
    if let Some(path) = matches.value_of("config") {
        return Ok(Some((
            read_value(path)?,
            ConfigContext { path: path.into() },
        )));
    }


    {
        let path = "mprocs.yaml";
        if Path::new(path).is_file() {
            return Ok(Some((
                read_value(path)?,
                ConfigContext { path: path.into() },
            )));
        }
    }

    {
        let path = "mprocs.json";
        if Path::new(path).is_file() {
            return Ok(Some((
                read_value(path)?,
                ConfigContext { path: path.into() },
            )));
        }
    }

    Ok(None)
}

fn read_value(path: &str) -> Result<Value> {
    // Open the file in read-only mode with buffer.
    let file = match std::fs::File::open(&path) {
        Ok(file) => file,
        Err(err) => match err.kind() {
            std::io::ErrorKind::NotFound => {
                bail!("Config file '{}' not found.", path);
            }
            _kind => return Err(err.into()),
        },
    };
    let mut reader = std::io::BufReader::new(file);
    let ext = std::path::Path::new(path)
        .extension()
        .map_or_else(|| "".to_string(), |ext| ext.to_string_lossy().to_string());
    let value: Value = match ext.as_str() {
        "yaml" | "yml" => serde_yaml::from_reader(reader)?,
        _ => bail!("Supported config extensions: yaml, yml."),
    };
    Ok(value)
}
