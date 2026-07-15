use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RattyHostPreferences {
    pub executable_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RattyPathSource {
    Configured,
    Environment,
    Sibling,
    Path,
}

impl RattyPathSource {
    pub fn label(self) -> &'static str {
        match self {
            Self::Configured => "configured",
            Self::Environment => "environment",
            Self::Sibling => "sibling",
            Self::Path => "PATH",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedRatty {
    pub path: PathBuf,
    pub source: RattyPathSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveryResult {
    pub resolved: Option<ResolvedRatty>,
    pub warning: Option<String>,
}

#[derive(Debug)]
pub enum LaunchOutcome {
    RunHere { warning: Option<String> },
    HostedExited(ExitStatus),
}

pub fn preferences_path(app_dir: &Path) -> PathBuf {
    app_dir.join("ratty-host.json")
}

pub fn managed_ratty_config_path(app_dir: &Path) -> PathBuf {
    app_dir.join("ratty").join("ratty.toml")
}

pub fn load_preferences(app_dir: &Path) -> Result<RattyHostPreferences> {
    let path = preferences_path(app_dir);
    if !path.is_file() {
        return Ok(RattyHostPreferences::default());
    }
    let bytes = fs::read(&path).with_context(|| {
        format!(
            "failed to read Ratty host preferences at {}",
            path.display()
        )
    })?;
    serde_json::from_slice(&bytes).with_context(|| {
        format!(
            "failed to parse Ratty host preferences at {}",
            path.display()
        )
    })
}

pub fn save_preferences(app_dir: &Path, preferences: &RattyHostPreferences) -> Result<()> {
    fs::create_dir_all(app_dir).with_context(|| {
        format!(
            "failed to create application data directory {}",
            app_dir.display()
        )
    })?;
    let path = preferences_path(app_dir);
    let bytes = serde_json::to_vec_pretty(preferences)?;
    fs::write(&path, bytes).with_context(|| {
        format!(
            "failed to write Ratty host preferences at {}",
            path.display()
        )
    })
}

fn ratty_file_name() -> &'static str {
    if cfg!(windows) {
        "ratty.exe"
    } else {
        "ratty"
    }
}

fn file_candidate(path: PathBuf, source: RattyPathSource) -> Option<ResolvedRatty> {
    path.is_file().then_some(ResolvedRatty { path, source })
}

fn resolve_ratty_executable(
    configured: Option<&Path>,
    environment: Option<&OsStr>,
    current_exe: &Path,
    path: Option<&OsStr>,
) -> DiscoveryResult {
    let mut warning = None;
    if let Some(configured) = configured.filter(|path| !path.as_os_str().is_empty()) {
        if let Some(resolved) =
            file_candidate(configured.to_path_buf(), RattyPathSource::Configured)
        {
            return DiscoveryResult {
                resolved: Some(resolved),
                warning,
            };
        }
        warning = Some(format!(
            "configured Ratty executable was not found: {}",
            configured.display()
        ));
    }

    if let Some(environment) = environment.filter(|value| !value.is_empty()) {
        if let Some(resolved) =
            file_candidate(PathBuf::from(environment), RattyPathSource::Environment)
        {
            return DiscoveryResult {
                resolved: Some(resolved),
                warning,
            };
        }
    }

    if let Some(parent) = current_exe.parent() {
        if let Some(resolved) =
            file_candidate(parent.join(ratty_file_name()), RattyPathSource::Sibling)
        {
            return DiscoveryResult {
                resolved: Some(resolved),
                warning,
            };
        }
    }

    if let Some(path) = path {
        for directory in env::split_paths(path) {
            if let Some(resolved) =
                file_candidate(directory.join(ratty_file_name()), RattyPathSource::Path)
            {
                return DiscoveryResult {
                    resolved: Some(resolved),
                    warning,
                };
            }
        }
    }

    DiscoveryResult {
        resolved: None,
        warning,
    }
}

pub fn resolved_ratty(app_dir: &Path) -> Result<DiscoveryResult> {
    let preferences = load_preferences(app_dir)?;
    let current_exe =
        env::current_exe().context("failed to resolve the current RChat executable")?;
    Ok(resolve_ratty_executable(
        preferences.executable_path.as_deref(),
        env::var_os("RCHAT_RATTY_PATH").as_deref(),
        &current_exe,
        env::var_os("PATH").as_deref(),
    ))
}

fn should_run_here(ratty_session: Option<&str>, no_ratty_arg: bool, no_ratty_env: bool) -> bool {
    ratty_session == Some("1") || no_ratty_arg || no_ratty_env
}

pub fn build_ratty_args(
    current_exe: &Path,
    child_args: &[OsString],
    config: Option<&Path>,
) -> Vec<OsString> {
    let mut args = vec![OsString::from("--title"), OsString::from("RChat")];
    if let Some(config) = config {
        args.extend([
            OsString::from("--config-file"),
            config.as_os_str().to_owned(),
        ]);
    }
    args.extend([
        OsString::from("--command"),
        current_exe.as_os_str().to_owned(),
    ]);
    args.extend_from_slice(child_args);
    args
}

fn launch_resolved_with<F>(
    resolved: &ResolvedRatty,
    current_exe: &Path,
    child_args: &[OsString],
    config: Option<&Path>,
    current_dir: &Path,
    runner: F,
) -> LaunchOutcome
where
    F: FnOnce(&Path, &[OsString], &Path) -> io::Result<ExitStatus>,
{
    let args = build_ratty_args(current_exe, child_args, config);
    match runner(&resolved.path, &args, current_dir) {
        Ok(status) => LaunchOutcome::HostedExited(status),
        Err(error) => LaunchOutcome::RunHere {
            warning: Some(format!(
                "failed to start Ratty at {}: {error}; using the current terminal with Kitty fallback",
                resolved.path.display()
            )),
        },
    }
}

pub fn launch_if_needed(args: &[OsString]) -> Result<LaunchOutcome> {
    let no_ratty_arg = args.iter().any(|arg| arg == OsStr::new("--no-ratty"));
    if should_run_here(
        env::var("RATTY_SESSION").ok().as_deref(),
        no_ratty_arg,
        env::var("RCHAT_NO_RATTY").ok().as_deref() == Some("1"),
    ) {
        return Ok(LaunchOutcome::RunHere { warning: None });
    }

    let app_dir = rchat_core::runtime::default_app_data_dir()?;
    let discovery = resolved_ratty(&app_dir)?;
    let Some(resolved) = discovery.resolved else {
        let fallback = "Ratty was not found; using the current terminal with Kitty fallback";
        return Ok(LaunchOutcome::RunHere {
            warning: Some(match discovery.warning {
                Some(warning) => format!("{warning}; {fallback}"),
                None => fallback.to_string(),
            }),
        });
    };
    if let Some(warning) = discovery.warning {
        eprintln!("{warning}");
    }

    let current_exe =
        env::current_exe().context("failed to resolve the current RChat executable")?;
    let current_dir = env::current_dir().context("failed to resolve the current directory")?;
    let child_args = args.iter().skip(1).cloned().collect::<Vec<_>>();
    let managed_config = managed_ratty_config_path(&app_dir);
    let config = managed_config.is_file().then_some(managed_config.as_path());
    Ok(launch_resolved_with(
        &resolved,
        &current_exe,
        &child_args,
        config,
        &current_dir,
        |program, ratty_args, directory| {
            Command::new(program)
                .args(ratty_args)
                .current_dir(directory)
                .env("RATTY_SESSION", "1")
                .status()
        },
    ))
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;
    use std::io;
    use std::path::{Path, PathBuf};

    use super::*;

    fn touch(path: PathBuf) -> PathBuf {
        fs::write(&path, b"").unwrap();
        path
    }

    #[test]
    fn ratty_host_discovery_prefers_configured_then_environment_then_sibling_then_path() {
        let temp = tempfile::tempdir().unwrap();
        let configured = touch(temp.path().join("configured-ratty"));
        let env_override = touch(temp.path().join("env-ratty"));
        let sibling = touch(temp.path().join(ratty_file_name()));
        let path_dir = temp.path().join("bin");
        fs::create_dir(&path_dir).unwrap();
        let path_ratty = touch(path_dir.join(ratty_file_name()));
        let current = temp.path().join("rchat-tui");

        let configured_result = resolve_ratty_executable(
            Some(&configured),
            Some(env_override.as_os_str()),
            &current,
            Some(path_dir.as_os_str()),
        );
        assert_eq!(
            configured_result.resolved,
            Some(ResolvedRatty {
                path: configured,
                source: RattyPathSource::Configured,
            })
        );

        let environment_result = resolve_ratty_executable(
            None,
            Some(env_override.as_os_str()),
            &current,
            Some(path_dir.as_os_str()),
        );
        assert_eq!(
            environment_result.resolved.unwrap().source,
            RattyPathSource::Environment
        );

        let sibling_result =
            resolve_ratty_executable(None, None, &current, Some(path_dir.as_os_str()));
        assert_eq!(sibling_result.resolved.unwrap().path, sibling);

        fs::remove_file(sibling).unwrap();
        let path_result =
            resolve_ratty_executable(None, None, &current, Some(path_dir.as_os_str()));
        assert_eq!(path_result.resolved.unwrap().path, path_ratty);
    }

    #[test]
    fn ratty_host_invalid_configured_path_warns_while_using_fallback() {
        let temp = tempfile::tempdir().unwrap();
        let fallback = touch(temp.path().join("fallback-ratty"));
        let current = temp.path().join("rchat-tui");
        let missing = temp.path().join("missing-ratty");

        let result =
            resolve_ratty_executable(Some(&missing), Some(fallback.as_os_str()), &current, None);

        assert_eq!(result.resolved.unwrap().path, fallback);
        assert!(result
            .warning
            .unwrap()
            .contains(&missing.display().to_string()));
    }

    #[test]
    fn ratty_host_hosted_or_bypassed_process_runs_in_current_terminal() {
        assert!(should_run_here(Some("1"), false, false));
        assert!(should_run_here(None, true, false));
        assert!(should_run_here(None, false, true));
        assert!(!should_run_here(None, false, false));
    }

    #[test]
    fn ratty_host_command_forwards_title_config_executable_and_arguments() {
        let command = build_ratty_args(
            Path::new("/tmp/rchat-tui"),
            &[
                OsString::from("media-smoke"),
                OsString::from("--fps"),
                OsString::from("10"),
            ],
            Some(Path::new("/tmp/ratty.toml")),
        );
        assert_eq!(
            command,
            [
                "--title",
                "RChat",
                "--config-file",
                "/tmp/ratty.toml",
                "--command",
                "/tmp/rchat-tui",
                "media-smoke",
                "--fps",
                "10",
            ]
            .map(OsString::from)
        );
    }

    #[test]
    fn ratty_host_launch_failure_falls_back_with_readable_warning() {
        let resolved = ResolvedRatty {
            path: PathBuf::from("/missing/ratty"),
            source: RattyPathSource::Configured,
        };

        let outcome = launch_resolved_with(
            &resolved,
            Path::new("/tmp/rchat-tui"),
            &[],
            None,
            Path::new("/tmp"),
            |_, _, _| Err(io::Error::from(io::ErrorKind::NotFound)),
        );

        let LaunchOutcome::RunHere { warning } = outcome else {
            panic!("failed Ratty launch must fall back to the current terminal");
        };
        let warning = warning.unwrap();
        assert!(warning.contains("failed to start Ratty"));
        assert!(warning.contains("Kitty fallback"));
    }

    #[test]
    fn ratty_host_preferences_round_trip_and_managed_path_is_stable() {
        let temp = tempfile::tempdir().unwrap();
        let preferences = RattyHostPreferences {
            executable_path: Some(PathBuf::from("/tmp/ratty")),
        };

        save_preferences(temp.path(), &preferences).unwrap();

        assert_eq!(load_preferences(temp.path()).unwrap(), preferences);
        assert_eq!(
            managed_ratty_config_path(temp.path()),
            temp.path().join("ratty").join("ratty.toml")
        );
        assert_eq!(
            preferences_path(temp.path()),
            temp.path().join("ratty-host.json")
        );
    }
}
