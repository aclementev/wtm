mod common;

use std::path::{Path, PathBuf};

use wtm::config::{self, FlagOverrides, Origin};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Layer {
    Flag,
    Env,
    Project,
    Global,
}

/// One configuration key with the layers it accepts. A key is absent from a
/// layer on purpose: `dir`, `branch_prefix` and `fetch` are personal settings
/// a repository must not reach, and `base` describes a repository, so a
/// global default for it would be meaningless.
struct Key {
    name: &'static str,
    env: &'static str,
    layers: &'static [Layer],
    set_flag: Option<fn(&mut FlagOverrides, &str)>,
    /// A distinct value per layer where the type allows it. For booleans every
    /// layer carries the same value and the origin proves which one won.
    value: fn(Layer) -> &'static str,
    default: &'static str,
}

/// The default data root is derived from the environment's XDG variables, so
/// only its origin is worth asserting.
const UNCHECKED: &str = "*";

fn keys() -> Vec<Key> {
    vec![
        Key {
            name: "dir",
            env: "WTM_DIR",
            layers: &[Layer::Flag, Layer::Env, Layer::Global],
            set_flag: Some(|f, v| f.dir = Some(PathBuf::from(v))),
            value: |layer| match layer {
                Layer::Flag => "/flag/dir",
                Layer::Env => "/env/dir",
                _ => "/global/dir",
            },
            default: UNCHECKED,
        },
        Key {
            name: "base",
            env: "WTM_BASE",
            layers: &[Layer::Flag, Layer::Env, Layer::Project],
            set_flag: Some(|f, v| f.base = Some(v.to_string())),
            value: |layer| match layer {
                Layer::Flag => "flag-base",
                Layer::Env => "env-base",
                _ => "project-base",
            },
            default: "origin/HEAD",
        },
        Key {
            name: "branch_prefix",
            env: "WTM_BRANCH_PREFIX",
            layers: &[Layer::Env, Layer::Global],
            set_flag: None,
            value: |layer| match layer {
                Layer::Env => "env/",
                _ => "global/",
            },
            default: "",
        },
        Key {
            name: "fetch",
            env: "WTM_FETCH",
            layers: &[Layer::Flag, Layer::Env, Layer::Global],
            set_flag: Some(|f, _| f.fetch = Some(true)),
            value: |_| "true",
            default: "false",
        },
        // The only key both files may set. Absolute values, so precedence is
        // tested apart from the relative-path rule below.
        Key {
            name: "init",
            env: "WTM_INIT",
            layers: &[Layer::Flag, Layer::Env, Layer::Project, Layer::Global],
            set_flag: Some(|f, v| f.init = Some(PathBuf::from(v))),
            value: |layer| match layer {
                Layer::Flag => "/flag/init.sh",
                Layer::Env => "/env/init.sh",
                Layer::Project => "/project/init.sh",
                Layer::Global => "/global/init.sh",
            },
            default: UNCHECKED,
        },
    ]
}

fn toml_for(key: &str, value: &str) -> String {
    match key {
        "fetch" => format!("fetch = {value}\n"),
        other => format!("{other} = \"{value}\"\n"),
    }
}

#[test]
fn every_key_takes_its_value_from_the_highest_layer_it_accepts() {
    let dir = common::repo::scratch("config-precedence");
    let project_file = dir.join("project.toml");
    let global_file = dir.join("global.toml");

    for key in keys() {
        for mask in 0..(1u8 << key.layers.len()) {
            let present: Vec<Layer> = key
                .layers
                .iter()
                .enumerate()
                .filter(|(bit, _)| mask & (1 << bit) != 0)
                .map(|(_, layer)| *layer)
                .collect();

            let mut flags = FlagOverrides::default();
            if present.contains(&Layer::Flag) {
                key.set_flag.unwrap()(&mut flags, (key.value)(Layer::Flag));
            }
            let env_value = present
                .contains(&Layer::Env)
                .then(|| (key.value)(Layer::Env).to_string());
            let env = |name: &str| (name == key.env).then(|| env_value.clone()).flatten();

            write_layer(
                &project_file,
                &key,
                present.contains(&Layer::Project),
                Layer::Project,
            );
            write_layer(
                &global_file,
                &key,
                present.contains(&Layer::Global),
                Layer::Global,
            );

            let config = config::load(
                &flags,
                &env,
                Some(&project_file),
                Some(&global_file),
                &dir,
                &dir,
            )
            .unwrap_or_else(|e| panic!("{} with layers {present:?}: {e}", key.name));
            let (_, value, origin) = config
                .entries()
                .into_iter()
                .find(|(name, _, _)| *name == key.name)
                .expect("the key is reported by wtm config");

            let (expected_value, expected_origin) = match present.first() {
                Some(layer) => ((key.value)(*layer), name_of(*layer)),
                None => (key.default, "default"),
            };
            assert_eq!(
                origin.split_whitespace().next().unwrap_or(&origin),
                expected_origin,
                "{} with layers {present:?}",
                key.name
            );
            if expected_value != UNCHECKED {
                assert_eq!(
                    value, expected_value,
                    "{} with layers {present:?}",
                    key.name
                );
            }
        }
    }
}

fn name_of(layer: Layer) -> &'static str {
    match layer {
        Layer::Flag => "flag",
        Layer::Env => "env",
        Layer::Project => "project",
        Layer::Global => "global",
    }
}

fn write_layer(file: &Path, key: &Key, present: bool, layer: Layer) {
    if present {
        std::fs::write(file, toml_for(key.name, (key.value)(layer))).unwrap();
    } else {
        let _ = std::fs::remove_file(file);
    }
}

/// Cloning a repository must not be able to relocate your worktrees, rename
/// your branches or add a network round-trip to every creation.
#[test]
fn a_project_cannot_set_a_personal_key() {
    let dir = common::repo::scratch("config-scope");

    for key in [
        "dir = \"/elsewhere\"",
        "branch_prefix = \"theirs/\"",
        "fetch = true",
    ] {
        let file = dir.join("project.toml");
        std::fs::write(&file, format!("{key}\n")).unwrap();

        let message = config::load(
            &FlagOverrides::default(),
            &|_| None,
            Some(&file),
            None,
            &dir,
            &dir,
        )
        .expect_err("a personal key in a project file must fail")
        .to_string();

        assert!(message.contains(&file.display().to_string()), "{message}");
        assert!(message.contains("personal setting"), "{message}");
    }
}

#[test]
fn an_unknown_key_is_rejected_naming_the_file_and_the_key() {
    let dir = common::repo::scratch("config-unknown");
    let file = dir.join("config.toml");
    std::fs::write(&file, "base = \"main\"\nnot_a_key = 3\n").unwrap();

    let message = config::load(
        &FlagOverrides::default(),
        &|_| None,
        Some(&file),
        None,
        &dir,
        &dir,
    )
    .expect_err("an unknown key must fail")
    .to_string();

    assert!(message.contains(&file.display().to_string()), "{message}");
    assert!(message.contains("not_a_key"), "{message}");
}

#[test]
fn a_missing_configuration_file_is_not_an_error() {
    let config = config::load(
        &FlagOverrides::default(),
        &|_| None,
        Some(Path::new("/nowhere/project.toml")),
        Some(Path::new("/nowhere/global.toml")),
        Path::new("/cwd"),
        Path::new("/source"),
    )
    .expect("absent files simply contribute nothing");

    assert_eq!(config.base.value, "origin/HEAD");
    assert_eq!(config.base.origin, Origin::Default);
}

/// Getting a layer's base wrong runs the wrong file without complaining, so
/// this pins each one separately.
#[test]
fn a_relative_init_path_resolves_against_the_base_its_layer_implies() {
    let cwd = Path::new("/cwd");
    let source = Path::new("/source");

    let resolved = |flags: FlagOverrides, env: Option<&'static str>, file: Option<&str>| {
        let dir = common::repo::scratch("config-init-relative");
        let project_file = dir.join("project.toml");
        if let Some(text) = file {
            std::fs::write(&project_file, format!("init = \"{text}\"\n")).unwrap();
        }
        config::load(
            &flags,
            &|name| {
                (name == "WTM_INIT")
                    .then(|| env.map(str::to_string))
                    .flatten()
            },
            Some(&project_file),
            None,
            cwd,
            source,
        )
        .expect("the layers are all valid")
        .init
        .value
    };

    let flag = FlagOverrides {
        init: Some(PathBuf::from("setup.sh")),
        ..Default::default()
    };
    assert_eq!(resolved(flag, None, None), cwd.join("setup.sh"));
    assert_eq!(
        resolved(FlagOverrides::default(), Some("setup.sh"), None),
        cwd.join("setup.sh")
    );
    assert_eq!(
        resolved(FlagOverrides::default(), None, Some("setup.sh")),
        source.join("setup.sh")
    );
    assert_eq!(
        resolved(FlagOverrides::default(), None, None),
        source.join("wtm-init.sh"),
        "the default hook lives at the root of the source worktree"
    );
    assert_eq!(
        resolved(FlagOverrides::default(), None, Some("/absolute/setup.sh")),
        Path::new("/absolute/setup.sh"),
        "an absolute path is used as it is"
    );
}
