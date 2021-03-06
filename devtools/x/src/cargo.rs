// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

use crate::{
    config::CargoConfig,
    utils::{apply_sccache_if_possible, project_root},
    Result,
};
use anyhow::anyhow;
use indexmap::map::IndexMap;
use log::{info, warn};
use std::{
    env,
    ffi::{OsStr, OsString},
    path::Path,
    process::{Command, Output, Stdio},
    time::Instant,
};

const RUST_TOOLCHAIN_VERSION: &str = include_str!("../../../rust-toolchain");
const RUSTUP_TOOLCHAIN: &str = "RUSTUP_TOOLCHAIN";
const CARGO: &str = "CARGO";
const SECRET_ENVS: &[&str] = &["AWS_ACCESS_KEY_ID", "AWS_SECRET_ACCESS_KEY"];
pub struct Cargo {
    inner: Command,
    pass_through_args: Vec<OsString>,
    env_additions: IndexMap<OsString, Option<OsString>>,
}

impl Cargo {
    pub fn new<S: AsRef<OsStr>>(
        cargo_config: &CargoConfig,
        command: S,
        attempt_sccache: bool,
    ) -> Self {
        // run rustup to find correct toolchain
        let output = Command::new("rustup")
            .arg("which")
            .arg("--toolchain")
            .arg(&cargo_config.toolchain)
            .arg("cargo")
            .output()
            .expect("failed to execute rustup which");
        let (cargo_binary, cargo_flags) = if output.status.success() {
            (
                String::from_utf8(output.stdout)
                    .expect("error parsing rustup which output into utf8"),
                &cargo_config.flags,
            )
        } else {
            println!(
                "WARN: Rust toolchain {} not installed; falling back to legacy Cargo resolver.",
                cargo_config.toolchain,
            );
            println!(
                "WARN: Run `rustup toolchain install {}` to use the new resolver.",
                cargo_config.toolchain,
            );
            ("cargo".to_string(), &None)
        };

        let mut inner = Command::new(str::trim(&cargo_binary));
        if let Some(flags) = &cargo_flags {
            inner.arg(&flags);
        }

        // The environment is inherited for child processes so we only need to set RUSTUP_TOOLCHAIN
        // if it isn't already present in the environment
        if env::var_os(RUSTUP_TOOLCHAIN).is_none() {
            inner.env(RUSTUP_TOOLCHAIN, RUST_TOOLCHAIN_VERSION.trim());
        }

        // Set the `CARGO` envvar with the path to the cargo binary being used
        inner.env(CARGO, cargo_binary.trim());

        //sccache apply
        let envs: IndexMap<OsString, Option<OsString>> = if attempt_sccache {
            let result = apply_sccache_if_possible(cargo_config);
            match result {
                Ok(env) => env
                    .iter()
                    .map(|(key, option)| {
                        if let Some(val) = option {
                            (
                                OsString::from(key.to_owned()),
                                Some(OsString::from(val.to_owned())),
                            )
                        } else {
                            (OsString::from(key.to_owned()), None)
                        }
                    })
                    .collect(),
                Err(hmm) => {
                    warn!("Could not install sccache: {}", hmm);
                    IndexMap::new()
                }
            }
        } else {
            IndexMap::new()
        };

        inner.arg(command);
        Self {
            inner,
            pass_through_args: Vec::new(),
            env_additions: envs,
        }
    }

    pub fn all(&mut self) -> &mut Self {
        self.inner.arg("--all");
        self
    }

    pub fn workspace(&mut self) -> &mut Self {
        self.inner.arg("--workspace");
        self
    }

    pub fn all_targets(&mut self) -> &mut Self {
        self.inner.arg("--all-targets");
        self
    }

    pub fn current_dir<P: AsRef<Path>>(&mut self, dir: P) -> &mut Self {
        self.inner.current_dir(dir);
        self
    }

    pub fn packages<I, S>(&mut self, packages: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for p in packages {
            self.inner.arg("--package");
            self.inner.arg(p);
        }
        self
    }

    pub fn exclusions<I, S>(&mut self, exclusions: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for e in exclusions {
            self.inner.arg("--exclude");
            self.inner.arg(e);
        }
        self
    }

    /// Adds a series of arguments to x's target command.
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner.args(args);
        self
    }

    /// Adds an argument to x's target command.
    #[allow(dead_code)]
    pub fn arg<S: AsRef<OsStr>>(&mut self, arg: S) -> &mut Self {
        self.inner.arg(arg);
        self
    }

    /// Adds "Pass Through" arguments to x's target command.
    /// Pass through arguments appear after a double dash " -- " and may
    /// not be handled/checked by x's target command itself, but an underlying executable.
    pub fn pass_through<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        for a in args {
            self.pass_through_args.push(a.as_ref().to_owned());
        }
        self
    }

    /// Passes extra environment variables to x's target command.
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, Option<V>)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        for (key, val) in vars {
            self.env(key, val);
        }
        self
    }

    /// Passes an extra environment variable to x's target command.
    pub fn env<K, V>(&mut self, key: K, val: Option<V>) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        let converted_val = if let Some(s) = val {
            Some(s.as_ref().to_owned())
        } else {
            None
        };

        self.env_additions
            .insert(key.as_ref().to_owned(), converted_val);
        self
    }

    pub fn run(&mut self) -> Result<()> {
        self.inner.stdout(Stdio::inherit()).stderr(Stdio::inherit());
        self.do_run(true).map(|_| ())
    }

    /// Runs this command, capturing the standard output into a `Vec<u8>`.
    /// No logging/timing will be displayed as the result of this call from x.
    pub fn run_with_output(&mut self) -> Result<Vec<u8>> {
        self.inner.stderr(Stdio::inherit());
        // Since system out hijacked don't log for this command
        self.do_run(false).map(|o| o.stdout)
    }

    /// Internal run command, where the magic happens.
    /// If log is true, any environment variable overrides will be logged, the full command will be logged,
    /// and after the command's output reaches stdout, the command will be printed again along with the time took
    /// to process the command (wallclock) in ms.
    fn do_run(&mut self, log: bool) -> Result<Output> {
        // these arguments are passed through cargo/x to underlying executable (test, clippy, etc)
        if !self.pass_through_args.is_empty() {
            self.inner.arg("--").args(&self.pass_through_args);
        }

        // once all the arguments are added to the command we can log it.
        if log {
            self.env_additions.iter().for_each(|t| {
                if let Some(env_val) = t.1 {
                    if SECRET_ENVS.contains(&t.0.to_str().unwrap_or_default()) && t.1.is_some() {
                        info!("export {:?}=********", env_val);
                    } else {
                        info!("export {:?}={:?}", t.0, env_val);
                    }
                } else {
                    info!("unset {:?}", t.0);
                }
            });
            info!("Executing: {:?}", &self.inner);
        }
        // process enviroment additions, removing Options that are none...
        for (key, option_value) in &self.env_additions {
            if let Some(value) = option_value {
                self.inner.env(key, value);
            } else {
                self.inner.env_remove(key);
            }
        }

        let now = Instant::now();
        let output = self.inner.output()?;
        // once the command has been executed we log it's success or failure.
        if log {
            if output.status.success() {
                info!(
                    "Completed in {}ms: {:?}",
                    now.elapsed().as_millis(),
                    &self.inner
                );
            } else {
                warn!(
                    "Failed in {}ms: {:?}",
                    now.elapsed().as_millis(),
                    &self.inner
                );
            }
        }
        if !output.status.success() {
            return Err(anyhow!("failed to run cargo command"));
        }
        Ok(output)
    }
}

#[derive(Debug, Default)]
pub struct CargoArgs {
    pub all_targets: bool,
}

/// Represents an invocations of cargo that will call multiple other invocations of
/// cargo based on groupings implied by the contents of <workspace-root>/x.toml.
pub enum CargoCommand<'a> {
    Bench {
        cargo_config: &'a CargoConfig,
        direct_args: &'a [OsString],
        args: &'a [OsString],
        env: &'a [(&'a str, Option<&'a str>)],
    },
    Check(&'a CargoConfig),
    Clippy(&'a CargoConfig, &'a [OsString]),
    Fix(&'a CargoConfig, &'a [OsString]),
    Test {
        cargo_config: &'a CargoConfig,
        direct_args: &'a [OsString],
        args: &'a [OsString],
        env: &'a [(&'a str, Option<&'a str>)],
    },
    Build {
        cargo_config: &'a CargoConfig,
        direct_args: &'a [OsString],
        args: &'a [OsString],
        env: &'a [(&'a str, Option<&'a str>)],
    },
}

impl<'a> CargoCommand<'a> {
    pub fn cargo_config(&self) -> &CargoConfig {
        match self {
            CargoCommand::Bench { cargo_config, .. } => cargo_config,
            CargoCommand::Check(config) => config,
            CargoCommand::Clippy(config, _) => config,
            CargoCommand::Fix(config, _) => config,
            CargoCommand::Test { cargo_config, .. } => cargo_config,
            CargoCommand::Build { cargo_config, .. } => cargo_config,
        }
    }

    pub fn run_on_local_package(&self, args: &CargoArgs) -> Result<()> {
        let mut cargo = Cargo::new(self.cargo_config(), self.as_str(), false);
        Self::apply_args(&mut cargo, args);
        cargo
            .args(self.direct_args())
            .pass_through(self.pass_through_args())
            .envs(self.get_extra_env().to_owned())
            .run()
    }

    pub fn run_on_packages<I, S>(&self, packages: I, args: &CargoArgs) -> Result<()>
    where
        I: IntoIterator<Item = S> + Clone,
        S: AsRef<OsStr>,
    {
        // Early return if we have no packages to run
        if packages.clone().into_iter().count() == 0 {
            return Ok(());
        }

        let mut cargo = Cargo::new(self.cargo_config(), self.as_str(), true);
        Self::apply_args(&mut cargo, args);
        cargo
            .current_dir(project_root())
            .args(self.direct_args())
            .packages(packages)
            .pass_through(self.pass_through_args())
            .envs(self.get_extra_env().to_owned())
            .run()
    }

    pub fn run_on_all_packages(&self, args: &CargoArgs) -> Result<()> {
        let mut cargo = Cargo::new(self.cargo_config(), self.as_str(), true);
        Self::apply_args(&mut cargo, args);
        cargo
            .current_dir(project_root())
            .workspace()
            .args(self.direct_args())
            .pass_through(self.pass_through_args())
            .envs(self.get_extra_env().to_owned())
            .run()
    }

    pub fn run_with_exclusions<I, S>(&self, exclusions: I, args: &CargoArgs) -> Result<()>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let mut cargo = Cargo::new(self.cargo_config(), self.as_str(), true);
        Self::apply_args(&mut cargo, args);
        cargo
            .current_dir(project_root())
            .workspace()
            .args(self.direct_args())
            .exclusions(exclusions)
            .pass_through(self.pass_through_args())
            .envs(self.get_extra_env().to_owned())
            .run()
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            CargoCommand::Bench { .. } => "bench",
            CargoCommand::Check(_) => "check",
            CargoCommand::Clippy(_, _) => "clippy",
            CargoCommand::Fix(_, _) => "fix",
            CargoCommand::Test { .. } => "test",
            CargoCommand::Build { .. } => "build",
        }
    }

    fn pass_through_args(&self) -> &[OsString] {
        match self {
            CargoCommand::Bench { args, .. } => args,
            CargoCommand::Check(_) => &[],
            CargoCommand::Clippy(_, args) => args,
            CargoCommand::Fix(_, args) => args,
            CargoCommand::Test { args, .. } => args,
            CargoCommand::Build { args, .. } => args,
        }
    }

    fn direct_args(&self) -> &[OsString] {
        match self {
            CargoCommand::Bench { direct_args, .. } => direct_args,
            CargoCommand::Check(_) => &[],
            CargoCommand::Clippy(_, _) => &[],
            CargoCommand::Fix(_, _) => &[],
            CargoCommand::Test { direct_args, .. } => direct_args,
            CargoCommand::Build { direct_args, .. } => direct_args,
        }
    }

    pub fn get_extra_env(&self) -> &[(&str, Option<&str>)] {
        match self {
            CargoCommand::Bench { env, .. } => env,
            CargoCommand::Check(_) => &[],
            CargoCommand::Clippy(_, _) => &[],
            CargoCommand::Fix(_, _) => &[],
            CargoCommand::Test { env, .. } => env,
            CargoCommand::Build { env, .. } => env,
        }
    }

    fn apply_args(cargo: &mut Cargo, args: &CargoArgs) {
        if args.all_targets {
            cargo.all_targets();
        }
    }
}
