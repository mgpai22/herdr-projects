//! OMP profiles, read from OMP's directory layout (no `omp` binary is run):
//! the default profile lives in `~/.omp/agent` and `~/.omp/plugins`, a named
//! profile in `~/.omp/profiles/<name>/{agent,plugins}`. The empty string is
//! the default profile everywhere. XDG-migrated layouts and `PI_CONFIG_DIR`
//! are not supported.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use yaml_rust2::parser::{Event, MarkedEventReceiver, Parser};
use yaml_rust2::scanner::{Marker, TScalarStyle};
use yaml_rust2::{ScanError, Yaml};

use crate::paths::Env;

/// OMP's profile name rule (`normalizeProfileName`): trimmed; `""` and
/// `default` are the default profile (`""`); otherwise `[a-z0-9][a-z0-9._-]*`,
/// at most 64 characters, not ending in a dot.
pub fn normalize_profile(raw: &str) -> Result<String> {
    let name = raw.trim();
    if name.is_empty() || name == "default" {
        return Ok(String::new());
    }
    let valid = name.len() <= 64
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        && !name.ends_with('.');
    if !valid {
        bail!("invalid OMP profile `{name}`: use lowercase letters, digits, `.`, `_` and `-`, starting with a letter or digit, at most 64 characters, not ending in `.`");
    }
    Ok(name.to_string())
}

fn profile_root(env: &Env, profile: &str) -> PathBuf {
    match profile {
        "" => env.home.join(".omp"),
        name => env.home.join(".omp/profiles").join(name),
    }
}

/// A profile's agent directory. For the default profile, an absolute
/// `PI_CODING_AGENT_DIR` wins, unless it is the agent dir of the profile named
/// by `OMP_PROFILE` or `PI_PROFILE`: OMP exports that into every child of a
/// named-profile session, and ignores it for the default profile.
pub fn agent_dir(env: &Env, profile: &str) -> PathBuf {
    if profile.is_empty()
        && let Some(dir) = env.var("PI_CODING_AGENT_DIR")
    {
        let dir = std::path::absolute(dir).unwrap_or_else(|_| PathBuf::from(dir));
        let derived = ["OMP_PROFILE", "PI_PROFILE"]
            .iter()
            .filter_map(|key| normalize_profile(env.var(key)?).ok().filter(|p| !p.is_empty()))
            .any(|p| profile_root(env, &p).join("agent") == dir);
        if !derived {
            return dir;
        }
    }
    profile_root(env, profile).join("agent")
}

/// A profile's plugins directory: plugins are installed per profile.
pub fn plugins_dir(env: &Env, profile: &str) -> PathBuf {
    profile_root(env, profile).join("plugins")
}

/// Every profile with an agent directory: the default first, then named
/// profiles by name. Entries of `~/.omp/profiles` that are not valid profile
/// names are skipped.
pub fn profile_agent_dirs(env: &Env) -> Vec<(String, PathBuf)> {
    let mut dirs = Vec::new();
    let default = agent_dir(env, "");
    if default.is_dir() {
        dirs.push((String::new(), default));
    }
    let mut named: Vec<(String, PathBuf)> = std::fs::read_dir(env.home.join(".omp/profiles"))
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let dir = agent_dir(env, &name);
            (normalize_profile(&name).ok()? == name && dir.is_dir()).then_some((name, dir))
        })
        .collect();
    named.sort();
    dirs.extend(named);
    dirs
}

pub const MSTACK: &str = "@mgpai22/mstack";

/// mstack's version when it is installed and enabled for the profile, as OMP's
/// plugin manager decides: a lock-file entry counts only with `enabled: true`;
/// a package with no entry counts when it is a dependency of the plugins
/// `package.json`. Project-level plugin overrides are not read.
pub fn mstack_version(env: &Env, profile: &str) -> Option<(u64, u64, u64)> {
    let plugins = plugins_dir(env, profile);
    let json = |path: &Path| std::fs::read_to_string(path).ok().and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok());
    let enabled = match json(&plugins.join("omp-plugins.lock.json")).as_ref().and_then(|lock| lock["plugins"].get(MSTACK)) {
        Some(entry) => entry["enabled"].as_bool() == Some(true),
        None => json(&plugins.join("package.json")).is_some_and(|p| p["dependencies"].get(MSTACK).is_some()),
    };
    if !enabled {
        return None;
    }
    let package = json(&plugins.join("node_modules").join(MSTACK).join("package.json"))?;
    let mut parts = package["version"].as_str()?.splitn(3, '.').map(|part| part.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u64>().ok());
    Some((parts.next()??, parts.next()??, parts.next()??))
}

#[derive(Debug, Clone, PartialEq)]
pub struct PatternRule {
    pub matcher: String,
    pub approval: String,
}

/// The profile's OMP config file: the first of `config.yml` and `config.yaml`
/// in its agent dir that exists.
pub fn config_path(env: &Env, profile: &str) -> Option<PathBuf> {
    let dir = agent_dir(env, profile);
    ["config.yml", "config.yaml"].map(|name| dir.join(name)).into_iter().find(|p| p.exists())
}

/// Builds the first document like `YamlLoader`, but as Bun.YAML (which OMP
/// parses with) reads it: a duplicate key keeps its last value, and a `<<`
/// key merges mappings.
#[derive(Default)]
struct Loader {
    doc: Option<Yaml>,
    /// Open collections with their anchor ids.
    stack: Vec<(Yaml, usize)>,
    /// The pending key of each open mapping.
    keys: Vec<Option<Yaml>>,
    anchors: std::collections::HashMap<usize, Yaml>,
    error: Option<ScanError>,
}

impl MarkedEventReceiver for Loader {
    fn on_event(&mut self, event: Event, mark: Marker) {
        if self.error.is_some() {
            return;
        }
        let (node, anchor) = match event {
            Event::SequenceStart(anchor, _) => {
                self.stack.push((Yaml::Array(Vec::new()), anchor));
                return;
            }
            Event::MappingStart(anchor, _) => {
                self.stack.push((Yaml::Hash(yaml_rust2::yaml::Hash::new()), anchor));
                self.keys.push(None);
                return;
            }
            Event::SequenceEnd => self.stack.pop().expect("a sequence was started"),
            Event::MappingEnd => {
                self.keys.pop();
                let (node, anchor) = self.stack.pop().expect("a mapping was started");
                match merge(node) {
                    Ok(node) => (node, anchor),
                    Err(info) => {
                        self.error = Some(ScanError::new(mark, info));
                        return;
                    }
                }
            }
            Event::Scalar(value, TScalarStyle::Plain, anchor, None) => (Yaml::from_str(&value), anchor),
            Event::Scalar(value, _, anchor, _) => (Yaml::String(value), anchor),
            Event::Alias(id) => (self.anchors.get(&id).cloned().unwrap_or(Yaml::BadValue), 0),
            _ => return,
        };
        if anchor > 0 {
            self.anchors.insert(anchor, node.clone());
        }
        match self.stack.last_mut() {
            None => self.doc = Some(node),
            Some((Yaml::Array(items), _)) => items.push(node),
            Some((Yaml::Hash(map), _)) => {
                let key = self.keys.last_mut().expect("an open mapping has a key slot");
                match key.take() {
                    None => *key = Some(node),
                    Some(key) => {
                        map.insert(key, node);
                    }
                }
            }
            Some(_) => unreachable!("only collections are pushed"),
        }
    }
}

const MERGE_ERROR: &str = "unsupported merge key `<<`: it merges only a mapping or a list of mappings";

/// Resolves a mapping's `<<` key: its own keys win, then earlier sources.
fn merge(node: Yaml) -> Result<Yaml, &'static str> {
    let Yaml::Hash(mut map) = node else {
        return Ok(node);
    };
    let sources = match map.remove(&Yaml::String("<<".into())) {
        None => Vec::new(),
        Some(Yaml::Hash(source)) => vec![source],
        Some(Yaml::Array(items)) => items.into_iter().map(|item| if let Yaml::Hash(source) = item { Ok(source) } else { Err(MERGE_ERROR) }).collect::<Result<_, _>>()?,
        Some(_) => return Err(MERGE_ERROR),
    };
    for (key, value) in sources.into_iter().flatten() {
        if !map.contains_key(&key) {
            map.insert(key, value);
        }
    }
    Ok(Yaml::Hash(map))
}

/// A profile's global `bash.patterns`, from its `config_path`. A missing file
/// has none; an unreadable or invalid one is an error. Entries OMP would
/// ignore are dropped, and the rest normalized as OMP does (whitespace runs in
/// the match collapse; the approval is trimmed and lowercased).
pub fn bash_patterns(env: &Env, profile: &str) -> Result<Vec<PatternRule>> {
    let Some(path) = config_path(env, profile) else {
        return Ok(Vec::new());
    };
    let text = std::fs::read_to_string(&path).map_err(|e| anyhow::anyhow!("{} cannot be read: {e}", path.display()))?;
    let mut loader = Loader::default();
    let parsed = Parser::new_from_str(&text).load(&mut loader, false);
    if let Some(e) = parsed.err().or(loader.error) {
        bail!("{} is not valid YAML: {e}", path.display());
    }
    let Some(patterns) = loader.doc.as_ref().and_then(|doc| doc["bash"]["patterns"].as_vec()) else {
        return Ok(Vec::new());
    };
    Ok(patterns
        .iter()
        .filter_map(|entry| {
            let matcher = entry["match"].as_str()?.split_whitespace().collect::<Vec<_>>().join(" ");
            let approval = entry["approval"].as_str()?.trim().to_lowercase();
            (!matcher.is_empty() && matches!(approval.as_str(), "allow" | "deny" | "prompt")).then_some(PatternRule { matcher, approval })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_names_follow_omps_rule() {
        for (raw, want) in [("", ""), ("  ", ""), ("default", ""), (" neurable ", "neurable"), ("a.b_c-9", "a.b_c-9"), ("0", "0")] {
            assert_eq!(normalize_profile(raw).unwrap(), want, "{raw:?}");
        }
        let long = "a".repeat(65);
        for bad in ["Work", ".", "..", "a.", "-a", ".a", "a/b", "a b", long.as_str()] {
            assert!(normalize_profile(bad).is_err(), "{bad:?}");
        }
        assert!(normalize_profile(&long[1..]).is_ok());
    }

    #[test]
    fn the_agent_dir_override_applies_to_the_default_profile_only_and_not_when_a_profile_exported_it() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let env = Env::for_test(h, &[]);
        assert_eq!(agent_dir(&env, ""), h.join(".omp/agent"));
        assert_eq!(agent_dir(&env, "neurable"), h.join(".omp/profiles/neurable/agent"));
        assert_eq!(plugins_dir(&env, ""), h.join(".omp/plugins"));
        assert_eq!(plugins_dir(&env, "neurable"), h.join(".omp/profiles/neurable/plugins"));

        let custom = h.join("custom");
        let env = Env::for_test(h, &[("PI_CODING_AGENT_DIR", custom.to_str().unwrap())]);
        assert_eq!(agent_dir(&env, ""), custom);
        assert_eq!(agent_dir(&env, "neurable"), h.join(".omp/profiles/neurable/agent"));
        let env = Env::for_test(h, &[("PI_CODING_AGENT_DIR", "rel")]);
        assert_eq!(agent_dir(&env, ""), std::env::current_dir().unwrap().join("rel"));

        // Inside a `neurable` session OMP exports its agent dir; the default stays ~/.omp/agent.
        let exported = h.join(".omp/profiles/neurable/agent");
        for key in ["OMP_PROFILE", "PI_PROFILE"] {
            let env = Env::for_test(h, &[("PI_CODING_AGENT_DIR", exported.to_str().unwrap()), (key, "neurable")]);
            assert_eq!(agent_dir(&env, ""), h.join(".omp/agent"), "{key}");
        }
        let env = Env::for_test(h, &[("PI_CODING_AGENT_DIR", exported.to_str().unwrap()), ("OMP_PROFILE", "other")]);
        assert_eq!(agent_dir(&env, ""), exported);
    }

    #[test]
    fn profiles_are_listed_default_first_then_valid_named_ones_with_an_agent_dir() {
        let home = tempfile::tempdir().unwrap();
        let h = home.path();
        let env = Env::for_test(h, &[]);
        assert!(profile_agent_dirs(&env).is_empty());
        for dir in ["profiles/zed/agent", "profiles/neurable/agent", "profiles/Bad/agent", "profiles/noagent/plugins"] {
            std::fs::create_dir_all(h.join(".omp").join(dir)).unwrap();
        }
        std::fs::write(h.join(".omp/profiles/file"), "").unwrap();
        let names = |env: &Env| profile_agent_dirs(env).into_iter().map(|(n, _)| n).collect::<Vec<_>>();
        assert_eq!(names(&env), ["neurable", "zed"]);
        std::fs::create_dir_all(h.join(".omp/agent")).unwrap();
        assert_eq!(profile_agent_dirs(&env)[0], (String::new(), h.join(".omp/agent")));
        assert_eq!(names(&env), ["", "neurable", "zed"]);
    }

    #[test]
    fn mstack_counts_only_when_installed_and_enabled_for_that_profile() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let plugins = plugins_dir(&env, "neurable");
        let package = plugins.join("node_modules").join(MSTACK);
        let lock = |enabled: &str| std::fs::write(plugins.join("omp-plugins.lock.json"), format!(r#"{{"plugins":{{"{MSTACK}":{{"version":"0.4.0","enabled":{enabled}}}}}}}"#)).unwrap();
        std::fs::create_dir_all(&package).unwrap();
        assert_eq!(mstack_version(&env, "neurable"), None, "no package.json");
        std::fs::write(package.join("package.json"), r#"{"name":"@mgpai22/mstack","version":"0.4.1-beta.2"}"#).unwrap();
        assert_eq!(mstack_version(&env, "neurable"), None, "neither locked nor a dependency");
        lock("true");
        assert_eq!(mstack_version(&env, "neurable"), Some((0, 4, 1)));
        assert_eq!(mstack_version(&env, ""), None, "plugins are per profile");
        lock("false");
        assert_eq!(mstack_version(&env, "neurable"), None, "disabled");
        std::fs::write(plugins.join("omp-plugins.lock.json"), format!(r#"{{"plugins":{{"{MSTACK}":{{"version":"0.4.0"}}}}}}"#)).unwrap();
        assert_eq!(mstack_version(&env, "neurable"), None, "an entry without `enabled` is off, as in OMP");
        std::fs::remove_file(plugins.join("omp-plugins.lock.json")).unwrap();
        std::fs::write(plugins.join("package.json"), format!(r#"{{"dependencies":{{"{MSTACK}":"^0.4"}}}}"#)).unwrap();
        assert_eq!(mstack_version(&env, "neurable"), Some((0, 4, 1)), "a dependency with no lock entry is enabled");
        std::fs::write(package.join("package.json"), r#"{"version":"x"}"#).unwrap();
        assert_eq!(mstack_version(&env, "neurable"), None);
    }

    #[test]
    fn bash_patterns_are_read_from_the_profiles_first_config_file() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let dir = agent_dir(&env, "neurable");
        assert!(bash_patterns(&env, "neurable").unwrap().is_empty(), "missing file");
        std::fs::create_dir_all(&dir).unwrap();
        let yaml = "theme: dark\nbash:\n  patterns:\n    - match: \"  rm   -rf * \"\n      approval: Deny\n    - match: 'git push*'\n      approval: prompt\n    - match: 3\n      approval: deny\n    - match: ls\n      approval: maybe\n    - match: \"  \"\n      approval: allow\n    - match: ls *\n      approval: allow\n";
        std::fs::write(dir.join("config.yaml"), yaml).unwrap();
        let rule = |m: &str, a: &str| PatternRule { matcher: m.into(), approval: a.into() };
        assert_eq!(bash_patterns(&env, "neurable").unwrap(), vec![rule("rm -rf *", "deny"), rule("git push*", "prompt"), rule("ls *", "allow")]);
        // `config.yml` wins when both exist.
        std::fs::write(dir.join("config.yml"), "theme: dark\n").unwrap();
        assert!(bash_patterns(&env, "neurable").unwrap().is_empty());
        assert!(bash_patterns(&env, "").unwrap().is_empty(), "another profile's file");
        std::fs::write(dir.join("config.yml"), "bash: [unclosed\n").unwrap();
        assert!(bash_patterns(&env, "neurable").is_err());
        std::fs::write(dir.join("config.yml"), b"\xff\xfe").unwrap();
        assert!(bash_patterns(&env, "neurable").is_err());
    }

    #[test]
    fn bash_patterns_read_duplicate_and_merge_keys_as_bun_does() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let dir = agent_dir(&env, "");
        std::fs::create_dir_all(&dir).unwrap();
        let rule = |m: &str, a: &str| PatternRule { matcher: m.into(), approval: a.into() };
        let read = |yaml: &str| {
            std::fs::write(dir.join("config.yml"), yaml).unwrap();
            bash_patterns(&env, "")
        };
        // A duplicate key keeps its last value, wherever it is.
        let deny_merge = "    - match: \"*gh *pr merge*\"\n      approval: deny\n";
        assert_eq!(read(&format!("theme: dark\ntheme: light\nbash:\n  patterns: []\nbash:\n  patterns:\n{deny_merge}")).unwrap(), vec![rule("*gh *pr merge*", "deny")]);
        // `<<` merges an anchored mapping; the mapping's own keys win.
        let base = format!("base: &b\n  patterns:\n{deny_merge}  other: 1\n");
        assert_eq!(read(&format!("{base}bash:\n  <<: *b\n")).unwrap(), vec![rule("*gh *pr merge*", "deny")]);
        assert!(read(&format!("{base}bash:\n  <<: *b\n  patterns: []\n")).unwrap().is_empty());
        assert_eq!(read(&format!("{base}empty: &e\n  patterns: []\nbash:\n  <<: [*b, *e]\n")).unwrap(), vec![rule("*gh *pr merge*", "deny")], "an earlier source wins");
        // So does a list entry built from an anchor.
        assert_eq!(read("r: &r\n  match: ls *\nbash:\n  patterns:\n    - <<: *r\n      approval: allow\n").unwrap(), vec![rule("ls *", "allow")]);
        // A merge OMP could not resolve either is an error doctor names.
        let error = read("bash:\n  <<: [1]\n").unwrap_err().to_string();
        assert!(error.contains("merge key `<<`"), "{error}");
    }
}
