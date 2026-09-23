mod common;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use wtm::config::{self, Config, FlagOverrides};

#[derive(Clone, Copy, PartialEq, Debug)]
enum Layer {
    Global,
    Project,
    Env,
    Flag,
}

/// Each key with the layers it accepts, lowest first, and a value per layer.
/// A key is absent from a layer on purpose: `dir`, `branch_prefix` and
/// `fetch` are personal and a repository must not set them, while `base`
/// and `init` describe a repository, so only its own file may.
const KEYS: &[(&str, &[(Layer, &str)])] = &[
    (
        "dir",
        &[
            (Layer::Global, "/global"),
            (Layer::Env, "/env"),
            (Layer::Flag, "/flag"),
        ],
    ),
    (
        "base",
        &[
            (Layer::Project, "project"),
            (Layer::Env, "env"),
            (Layer::Flag, "flag"),
        ],
    ),
    (
        "branch_prefix",
        &[(Layer::Global, "global/"), (Layer::Env, "env/")],
    ),
    (
        "fetch",
        &[
            (Layer::Global, "true"),
            (Layer::Env, "true"),
            (Layer::Flag, "true"),
        ],
    ),
    (
        "init",
        &[
            (Layer::Project, "/project.sh"),
            (Layer::Env, "/env.sh"),
            (Layer::Flag, "/flag.sh"),
        ],
    ),
];

fn toml(key: &str, value: &str) -> String {
    match key {
        "fetch" => format!("{key} = {value}\n"),
        _ => format!("{key} = \"{value}\"\n"),
    }
}

/// Loads `key` with each of `layers` set, the files written under `dir`.
fn load(dir: &Path, key: &str, layers: &[(Layer, &str)]) -> wtm::error::Result<Config> {
    let (project, global) = (dir.join("project.toml"), dir.join("global.toml"));
    let _ = std::fs::remove_file(&project);
    let _ = std::fs::remove_file(&global);
    let mut flags = FlagOverrides::default();
    let mut env = HashMap::new();
    for &(layer, value) in layers {
        match layer {
            Layer::Global => std::fs::write(&global, toml(key, value)).unwrap(),
            Layer::Project => std::fs::write(&project, toml(key, value)).unwrap(),
            Layer::Env => {
                env.insert(format!("WTM_{}", key.to_uppercase()), value.to_string());
            }
            Layer::Flag => match key {
                "dir" => flags.dir = Some(PathBuf::from(value)),
                "base" => flags.base = Some(value.to_string()),
                "fetch" => flags.fetch = Some(true),
                "init" => flags.init = Some(PathBuf::from(value)),
                _ => unreachable!("{key} has no flag"),
            },
        }
    }
    config::load(
        &flags,
        &|name| env.get(name).cloned(),
        Some(&project),
        Some(&global),
        dir,
        dir,
    )
}

/// Adding the layers one at a time, lowest first, each new one must win.
/// That checks both that every layer is read for the key and that it beats
/// every layer below it.
#[test]
fn each_key_takes_its_value_from_the_highest_layer_that_sets_it() {
    let dir = common::repo::scratch("config-precedence");
    for (key, layers) in KEYS {
        for set in 0..=layers.len() {
            let config = load(&dir, key, &layers[..set]).unwrap();
            let (_, value, origin) = config
                .entries()
                .into_iter()
                .find(|(name, _, _)| name == key)
                .unwrap();
            let expected = match set {
                0 => "default".to_string(),
                _ => format!("{:?}", layers[set - 1].0).to_lowercase(),
            };
            assert!(
                origin.starts_with(&expected),
                "{key} with {:?}: came from {origin}",
                &layers[..set]
            );
            if set > 0 {
                assert_eq!(value, layers[set - 1].1, "{key} with {:?}", &layers[..set]);
            }
        }
    }
}

/// A file that sets a key it does not own, or a key that does not exist,
/// fails naming the file and the key rather than being ignored.
#[test]
fn a_file_refuses_every_key_it_does_not_own() {
    let dir = common::repo::scratch("config-scope");
    for (key, layers) in KEYS {
        for layer in [Layer::Project, Layer::Global] {
            if layers.iter().any(|(accepted, _)| *accepted == layer) {
                continue;
            }
            // Any value the key accepts elsewhere, so the only fault is where.
            let message = load(&dir, key, &[(layer, layers[0].1)])
                .err()
                .unwrap_or_else(|| panic!("{key} in the {layer:?} file must fail"))
                .to_string();
            assert!(message.contains(key), "{message}");
            assert!(message.contains(".toml"), "{message}");
        }
    }

    let file = dir.join("global.toml");
    std::fs::write(&file, "not_a_key = 3\n").unwrap();
    let message = config::load(
        &FlagOverrides::default(),
        &|_| None,
        None,
        Some(&file),
        &dir,
        &dir,
    )
    .expect_err("an unknown key must fail")
    .to_string();
    assert!(
        message.contains("not_a_key") && message.contains("global.toml"),
        "{message}"
    );
}

/// A relative hook path typed this invocation follows the caller, like any
/// path argument. One stored in the project file follows the repository, so
/// a gitignored hook in the main worktree runs for every new worktree.
/// Getting a base wrong runs the wrong file without complaining.
#[test]
fn a_relative_init_path_resolves_against_the_base_its_layer_implies() {
    let dir = common::repo::scratch("config-init-relative");
    let (cwd, source) = (Path::new("/cwd"), Path::new("/source"));
    let project = dir.join("project.toml");
    let resolved = |flag: Option<&str>, env: Option<&str>, file: Option<&str>| {
        let _ = std::fs::remove_file(&project);
        if let Some(path) = file {
            std::fs::write(&project, toml("init", path)).unwrap();
        }
        let flags = FlagOverrides {
            init: flag.map(PathBuf::from),
            ..Default::default()
        };
        let env = env.map(str::to_string);
        let env = |name: &str| (name == "WTM_INIT").then(|| env.clone()).flatten();
        config::load(&flags, &env, Some(&project), None, cwd, source)
            .unwrap()
            .init
            .value
    };

    assert_eq!(resolved(Some("setup.sh"), None, None), cwd.join("setup.sh"));
    assert_eq!(resolved(None, Some("setup.sh"), None), cwd.join("setup.sh"));
    assert_eq!(
        resolved(None, None, Some("setup.sh")),
        source.join("setup.sh")
    );
    assert_eq!(resolved(None, None, None), source.join("wtm-init.sh"));
    assert_eq!(
        resolved(None, None, Some("/absolute/setup.sh")),
        Path::new("/absolute/setup.sh")
    );
}
