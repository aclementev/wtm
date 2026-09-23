use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::error::{Error, Result};

/// Which layer a value came from. `wtm config` prints it, which is why every
/// setting carries one rather than the merge collapsing to plain values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Origin {
    Flag,
    Env(String),
    Project(PathBuf),
    Global(PathBuf),
    Default,
}

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Origin::Flag => f.write_str("flag"),
            Origin::Env(key) => write!(f, "env {key}"),
            Origin::Project(path) => write!(f, "project {}", path.display()),
            Origin::Global(path) => write!(f, "global {}", path.display()),
            Origin::Default => f.write_str("default"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Setting<T> {
    pub value: T,
    pub origin: Origin,
}

/// Every setting is scoped to whoever owns the decision. `dir`,
/// `branch_prefix` and `fetch` describe this machine and this person, so a
/// repository cannot set them. Cloning a repo must never relocate your
/// worktrees, rename your branches or add a network round-trip. `base` and
/// `init` describe the repository, so only its own file may set them. A
/// global `init` would also outrank the repository's `wtm-init.sh`, which
/// is the default rather than a setting.
#[derive(Debug)]
pub struct Config {
    pub dir: Setting<PathBuf>,
    pub base: Setting<String>,
    pub branch_prefix: Setting<String>,
    pub fetch: Setting<bool>,
    /// Always absolute: `load` has already applied the relative-path rule,
    /// so nothing downstream repeats it.
    pub init: Setting<PathBuf>,
}

impl Config {
    /// Key, value and origin for every setting, in one order shared by the
    /// text and JSON renderings of `wtm config`.
    pub fn entries(&self) -> Vec<(&'static str, String, String)> {
        vec![
            entry("dir", self.dir.value.display(), &self.dir.origin),
            entry("base", &self.base.value, &self.base.origin),
            entry(
                "branch_prefix",
                &self.branch_prefix.value,
                &self.branch_prefix.origin,
            ),
            entry("fetch", self.fetch.value, &self.fetch.origin),
            entry("init", self.init.value.display(), &self.init.origin),
        ]
    }
}

fn entry(
    key: &'static str,
    value: impl fmt::Display,
    origin: &Origin,
) -> (&'static str, String, String) {
    (key, value.to_string(), origin.to_string())
}

/// Values a command parsed from the command line. Absent means the flag was
/// not given, not that it was given empty.
#[derive(Default)]
pub struct FlagOverrides {
    pub dir: Option<PathBuf>,
    pub base: Option<String>,
    pub fetch: Option<bool>,
    pub init: Option<PathBuf>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    dir: Option<String>,
    base: Option<String>,
    branch_prefix: Option<String>,
    fetch: Option<bool>,
    init: Option<String>,
}

type EnvFn<'a> = &'a dyn Fn(&str) -> Option<String>;

/// A relative `init` resolves against `cwd` when it was typed this
/// invocation and against `source`, the source worktree's root, when it came
/// from a file. Each layer joins its own base below, so the two cannot drift
/// apart.
pub fn load(
    flags: &FlagOverrides,
    env: EnvFn,
    project_file: Option<&Path>,
    global_file: Option<&Path>,
    cwd: &Path,
    source: &Path,
) -> Result<Config> {
    let (project, project_path) = read(project_file)?;
    let (global, global_path) = read(global_file)?;

    if let Some(path) = &project_path {
        reject(
            &project,
            path,
            &["dir", "branch_prefix", "fetch"],
            "is a personal setting and cannot be set by a project",
        )?;
    }
    if let Some(path) = &global_path {
        reject(
            &global,
            path,
            &["base", "init"],
            "describes a repository and belongs in its .wtm/config.toml",
        )?;
    }

    let project_origin = || Origin::Project(project_path.clone().unwrap_or_default());
    let global_origin = || Origin::Global(global_path.clone().unwrap_or_default());

    Ok(Config {
        dir: choose(
            vec![
                (flags.dir.clone(), Origin::Flag),
                (
                    env("WTM_DIR").as_deref().map(expand_tilde),
                    Origin::Env("WTM_DIR".into()),
                ),
                (global.dir.as_deref().map(expand_tilde), global_origin()),
            ],
            default_data_dir(),
        ),
        base: choose(
            vec![
                (flags.base.clone(), Origin::Flag),
                (env("WTM_BASE"), Origin::Env("WTM_BASE".into())),
                (project.base.clone(), project_origin()),
            ],
            "origin/HEAD".to_string(),
        ),
        branch_prefix: choose(
            vec![
                (
                    env("WTM_BRANCH_PREFIX"),
                    Origin::Env("WTM_BRANCH_PREFIX".into()),
                ),
                (global.branch_prefix.clone(), global_origin()),
            ],
            String::new(),
        ),
        fetch: choose(
            vec![
                (flags.fetch, Origin::Flag),
                (env_bool(env, "WTM_FETCH")?, Origin::Env("WTM_FETCH".into())),
                (global.fetch, global_origin()),
            ],
            false,
        ),
        init: choose(
            vec![
                (flags.init.clone().map(|path| cwd.join(path)), Origin::Flag),
                (
                    env("WTM_INIT").map(|path| cwd.join(expand_tilde(&path))),
                    Origin::Env("WTM_INIT".into()),
                ),
                (
                    project
                        .init
                        .as_deref()
                        .map(|p| source.join(expand_tilde(p))),
                    project_origin(),
                ),
            ],
            source.join(DEFAULT_HOOK),
        ),
    })
}

const DEFAULT_HOOK: &str = "wtm-init.sh";

/// The first layer that has a value wins; layers are given highest first.
fn choose<T>(candidates: Vec<(Option<T>, Origin)>, default: T) -> Setting<T> {
    for (value, origin) in candidates {
        if let Some(value) = value {
            return Setting { value, origin };
        }
    }
    Setting {
        value: default,
        origin: Origin::Default,
    }
}

/// A file that sets a key it does not own is an error naming the file and
/// the key. Ignoring the value would hide a mistake its author wants to hear
/// about.
fn reject(raw: &RawConfig, file: &Path, keys: &[&str], why: &str) -> Result<()> {
    let present = [
        ("dir", raw.dir.is_some()),
        ("base", raw.base.is_some()),
        ("branch_prefix", raw.branch_prefix.is_some()),
        ("fetch", raw.fetch.is_some()),
        ("init", raw.init.is_some()),
    ];
    let offender = present
        .into_iter()
        .find_map(|(key, set)| (set && keys.contains(&key)).then_some(key));

    match offender {
        None => Ok(()),
        Some(key) => Err(Error::Config {
            file: file.to_path_buf(),
            message: format!("{key} {why}"),
        }),
    }
}

fn read(file: Option<&Path>) -> Result<(RawConfig, Option<PathBuf>)> {
    let Some(file) = file else {
        return Ok((RawConfig::default(), None));
    };
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((RawConfig::default(), None));
        }
        Err(e) => return Err(Error::io(file, e)),
    };
    let raw = toml::from_str(&text).map_err(|e| Error::Config {
        file: file.to_path_buf(),
        message: e.message().to_string(),
    })?;
    Ok((raw, Some(file.to_path_buf())))
}

fn env_bool(env: EnvFn, key: &str) -> Result<Option<bool>> {
    match env(key).as_deref() {
        None => Ok(None),
        Some("1" | "true" | "yes") => Ok(Some(true)),
        Some("0" | "false" | "no") => Ok(Some(false)),
        Some(other) => Err(Error::usage(format!(
            "{key} is {other:?}, expected a boolean"
        ))),
    }
}

fn expand_tilde(text: &str) -> PathBuf {
    match text.strip_prefix("~/") {
        Some(rest) => home().join(rest),
        None => PathBuf::from(text),
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn xdg(var: &str, fallback: &str) -> PathBuf {
    match std::env::var_os(var) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => home().join(fallback),
    }
}

pub fn default_data_dir() -> PathBuf {
    xdg("XDG_DATA_HOME", ".local/share").join("wtm/worktrees")
}

/// Where a reaper writes when `WTM_DEBUG` is set. The only file wtm creates
/// outside the data root, which the zero-state test allows for that reason.
pub fn reaper_log_file() -> PathBuf {
    xdg("XDG_CACHE_HOME", ".cache").join("wtm/reaper.log")
}

pub fn global_config_file() -> PathBuf {
    xdg("XDG_CONFIG_HOME", ".config").join("wtm/config.toml")
}

/// The project layer is read from the main worktree, so every worktree of a
/// repository sees the same configuration.
pub fn project_config_file(main: &Path) -> PathBuf {
    main.join(".wtm/config.toml")
}
