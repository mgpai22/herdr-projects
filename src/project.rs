//! Project folders under the root: slugs, settings, status, the per-project
//! lock and the coordinator record.

use std::fs::File;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::paths::Env;

pub const MAX_SLUG: usize = 40;
pub const BODY_WARN_CHARS: usize = 16_000;

/// A slug matches `[a-z0-9][a-z0-9-]*` and is at most 40 characters. Every
/// subcommand validates the slug it is given before building any path from it.
pub fn validate_slug(slug: &str) -> Result<()> {
    let mut chars = slug.chars();
    let first_ok = chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
    let rest_ok = chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !first_ok || !rest_ok || slug.len() > MAX_SLUG {
        bail!("`{slug}` is not a valid slug (lower-case letters, digits and hyphens, at most {MAX_SLUG} characters)");
    }
    Ok(())
}

/// Lower-cases and turns each run of other characters into one hyphen. Used for
/// project names and for thread titles in branch names.
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for c in text.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_lowercase() || c.is_ascii_digit() {
            slug.push(c);
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let mut slug: String = slug.chars().take(MAX_SLUG).collect();
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

/// The slug `new` gives a project name, refusing names that look like paths.
pub fn slug_from_name(name: &str) -> Result<String> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        bail!("a project name may not contain `/`, `\\` or `..`");
    }
    let slug = slugify(name);
    if slug.is_empty() {
        bail!("`{name}` has no letters or digits to make a slug from");
    }
    validate_slug(&slug)?;
    Ok(slug)
}

/// Words split on `-` and `_`, each with its first letter upper-cased:
/// `herdr-projects` becomes `Herdr Projects`. Plain title case, so `gtm-ai`
/// becomes `Gtm Ai`; a user who wants `GTM AI` sets `name` in PROJECT.md.
pub fn humanize(slug: &str) -> String {
    slug.split(['-', '_'])
        .filter(|word| !word.is_empty())
        .map(|word| {
            let mut chars = word.chars();
            chars.next().map(|first| first.to_uppercase().chain(chars).collect::<String>()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The name a project shows: `name` as given unless it is empty or looks like
/// a slug (lower-case letters, digits, `-` and `_` only), else the humanized
/// form of it or of `slug`. A herdr workspace never shows a bare slug, which
/// would read the same as a repository's own workspace.
pub fn display_name(name: &str, slug: &str) -> String {
    let name = name.trim();
    let base = if name.is_empty() { slug } else { name };
    let slug_like = base.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
    if slug_like { humanize(base) } else { base.to_string() }
}

/// Writes through a temporary file in the same directory plus a rename. It never
/// creates parent directories: only `new` creates a project's directories.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    let name = path.file_name().context("path has no file name")?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        name.to_string_lossy(),
        std::process::id()
    ));
    let result = (|| -> Result<()> {
        let mut file = File::create(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result.with_context(|| format!("could not write {}", path.display()))
}

pub fn now() -> String {
    jiff::Timestamp::now()
        .round(jiff::Unit::Second)
        .map(|t| t.to_string())
        .unwrap_or_default()
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Repo {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<String>,
}

/// `PROJECT.md` front matter. `repos` is last so the TOML tables follow the
/// plain keys when `new` serializes it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Settings {
    pub name: String,
    pub goal: String,
    pub coordinator_agent: String,
    pub thread_agent: String,
    /// The OMP profile coordinators and threads start with; empty = default.
    pub omp_profile: String,
    pub max_parallel_threads: u32,
    pub auto_resolve_days: u32,
    pub nudge: bool,
    /// Silences every notification for the project except errors.
    pub mute: bool,
    pub repos: Vec<Repo>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            name: String::new(),
            goal: String::new(),
            coordinator_agent: "claude".into(),
            thread_agent: "claude".into(),
            omp_profile: String::new(),
            max_parallel_threads: 3,
            auto_resolve_days: 7,
            // On by default (W15): the ticker prompts only a coordinator that
            // has been idle for a minute, because on herdr 0.9.1 a prompt
            // merges with half-typed text (docs/herdr-notes.md, stage 2).
            nudge: true,
            mute: false,
            repos: Vec::new(),
        }
    }
}

/// Splits `+++` TOML front matter from the body.
pub fn parse_project_md(text: &str) -> Result<(Settings, String)> {
    let rest = text
        .strip_prefix("+++\n")
        .context("PROJECT.md must start with a `+++` line")?;
    let (front, body) = match rest.split_once("\n+++\n") {
        Some(parts) => parts,
        None => rest
            .strip_suffix("\n+++")
            .map(|front| (front, ""))
            .context("PROJECT.md front matter has no closing `+++` line")?,
    };
    let settings: Settings = toml::from_str(front).context("PROJECT.md front matter does not parse")?;
    Ok((settings, body.trim_start_matches('\n').to_string()))
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    #[default]
    Active,
    Paused,
    Archived,
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Status::Active => "active",
            Status::Paused => "paused",
            Status::Archived => "archived",
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
struct ProjectState {
    status: Status,
}

/// The session and workspace the project belongs to, and the coordinator pane
/// `open` last started or focused. Any agent whose working directory is `cwd`
/// is a coordinator; the ticker lists them in `.state/coordinators.json`.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Coordinator {
    pub socket: String,
    /// Empty when the session was chosen by socket path alone.
    pub session: String,
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: String,
    pub agent_name: String,
    /// The canonical project folder.
    pub cwd: String,
    /// The Herdr agent kind `open` last started.
    pub agent: String,
    /// The OMP profile `open` last started; empty = default.
    pub omp_profile: String,
    /// The last native session id Herdr reported for that kind, for resume.
    pub agent_session: String,
    pub updated: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Safety {
    pub start_threads: String,
    pub coordinator_agent_args: Vec<String>,
    pub thread_agent_args: Vec<String>,
    pub routine_commands: bool,
}

impl Default for Safety {
    fn default() -> Self {
        Safety {
            start_threads: "propose".into(),
            coordinator_agent_args: Vec::new(),
            thread_agent_args: Vec::new(),
            routine_commands: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Project {
    pub root: PathBuf,
    pub slug: String,
}

/// Held while reading and rewriting anything under `threads/`, `inbox/` or
/// `.state/`. Never held across a herdr, git, gh, ssh or scp call.
pub struct ProjectLock {
    _file: File,
}

impl Project {
    /// An existing project. Validates the slug before building any path.
    pub fn load(root: &Path, slug: &str) -> Result<Project> {
        validate_slug(slug)?;
        let project = Project {
            root: root.to_path_buf(),
            slug: slug.to_string(),
        };
        if !project.project_md().is_file() {
            bail!("no project `{slug}` in {}", root.display());
        }
        Ok(project)
    }

    pub fn dir(&self) -> PathBuf {
        self.root.join(&self.slug)
    }

    pub fn project_md(&self) -> PathBuf {
        self.dir().join("PROJECT.md")
    }

    pub fn state_dir(&self) -> PathBuf {
        self.dir().join(".state")
    }

    /// The canonical folder (symlinks resolved): the key of the project's
    /// `[safety]` table and of its routine approvals.
    pub fn canonical_dir(&self) -> PathBuf {
        std::fs::canonicalize(self.dir()).unwrap_or_else(|_| self.dir())
    }

    /// Takes the per-project lock. The lock file is opened without creating
    /// parent directories, and the project is re-checked afterwards, so a
    /// `delete` that lands mid-operation cannot be resurrected by a writer.
    pub fn lock(&self) -> Result<ProjectLock> {
        let path = self.state_dir().join("lock");
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("project `{}` is gone ({})", self.slug, path.display()))?;
        file.lock()?;
        if !self.project_md().is_file() {
            bail!("project `{}` is gone", self.slug);
        }
        Ok(ProjectLock { _file: file })
    }

    pub fn read_project_md(&self) -> Result<(Settings, String)> {
        let text = std::fs::read_to_string(self.project_md())
            .with_context(|| format!("could not read {}", self.project_md().display()))?;
        parse_project_md(&text)
    }

    pub fn status(&self) -> Status {
        read_json::<ProjectState>(&self.state_dir().join("project.json"))
            .unwrap_or_default()
            .status
    }

    pub fn set_status(&self, status: Status) -> Result<()> {
        let _lock = self.lock()?;
        write_json(&self.state_dir().join("project.json"), &ProjectState { status })
    }

    pub fn coordinator(&self) -> Option<Coordinator> {
        read_json(&self.state_dir().join("coordinator.json"))
    }

    /// Read-modify-write of `coordinator.json` under the lock: re-reads the
    /// file, lets `change` touch only the fields its step owns, writes.
    pub fn update_coordinator(&self, change: impl FnOnce(&mut Coordinator)) -> Result<Coordinator> {
        let _lock = self.lock()?;
        let mut record = self.coordinator().unwrap_or_default();
        change(&mut record);
        record.updated = now();
        write_json(&self.state_dir().join("coordinator.json"), &record)?;
        Ok(record)
    }

    /// Removes `coordinator.json` under the lock: back to never opened.
    pub fn remove_coordinator(&self) -> Result<()> {
        let _lock = self.lock()?;
        match std::fs::remove_file(self.state_dir().join("coordinator.json")) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => Err(error.into()),
            _ => Ok(()),
        }
    }

    pub fn safety(&self, config_dir: &Path) -> Result<Safety> {
        load_safety(config_dir, &self.canonical_dir())
    }
}

pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let mut text = serde_json::to_string_pretty(value)?;
    text.push('\n');
    write_atomic(path, text.as_bytes())
}

/// The effective safety settings: `[safety."<canonical project path>"]` in
/// `<config_dir>/config.toml`, with defaults for an absent table or key.
pub fn load_safety(config_dir: &Path, canonical_project_dir: &Path) -> Result<Safety> {
    #[derive(Deserialize, Default)]
    struct Config {
        #[serde(default)]
        safety: std::collections::BTreeMap<String, Safety>,
    }
    let file = config_dir.join("config.toml");
    let Ok(text) = std::fs::read_to_string(&file) else {
        return Ok(Safety::default());
    };
    let mut config: Config =
        toml::from_str(&text).with_context(|| format!("{} does not parse", file.display()))?;
    let safety = config
        .safety
        .remove(&*canonical_project_dir.to_string_lossy())
        .unwrap_or_default();
    if !matches!(safety.start_threads.as_str(), "propose" | "auto") {
        bail!(
            "{}: start_threads must be \"propose\" or \"auto\", not {:?}",
            file.display(),
            safety.start_threads
        );
    }
    Ok(safety)
}

/// Slugs of the projects in `root`: folders that contain `PROJECT.md`. Entries
/// whose names start with a dot are ignored. A missing root has no projects.
pub fn list_slugs(root: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    let mut slugs: Vec<String> = entries
        .flatten()
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| !name.starts_with('.') && validate_slug(name).is_ok())
        .filter(|name| root.join(name).join("PROJECT.md").is_file())
        .collect();
    slugs.sort();
    slugs
}

/// `PATH[@MACHINE]` as given to `new --repo`.
pub fn parse_repo_arg(arg: &str) -> Repo {
    if let Some((path, machine)) = arg.rsplit_once('@') {
        let label_like = !machine.is_empty()
            && machine
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if label_like && !path.is_empty() {
            return Repo {
                path: path.to_string(),
                machine: Some(machine.to_string()),
            };
        }
    }
    Repo {
        path: arg.to_string(),
        machine: None,
    }
}

const TASKS_TEMPLATE: &str = "# Tasks\n\n## Backlog\n";

const INSTRUCTIONS_TEMPLATE: &str = "\
# Instructions

Standing instructions for this project. Every thread starts from this text and
from the project's memory. Replace this paragraph with how you want work done:
conventions, what to check before finishing, what never to do. Ask the
coordinator to change it, or edit it here.

The settings above, between the `+++` lines, are changed from the projects
popup or by asking the coordinator.
";

/// The folders every project has. `uploads/` is yours (files for threads),
/// `library/` holds what threads produced.
pub const SUBDIRS: [&str; 9] = ["memory", "scratch", "routines", "threads", "inbox", "inbox/done", "library", "uploads", ".state"];

/// The text of `AGENTS.md`. Harnesses load it from every ancestor of their
/// working directory, and tab threads run under `threads/<id>/`, so it says
/// who is who by working directory. `prefix` is `<absolute binary> --root
/// <root>`: bare `hp` is on no harness's `PATH`.
pub fn agents_md(name: &str, slug: &str, prefix: &str) -> String {
    format!(
        "# {name}\n\n\
         This folder is the home of the Herdr project \"{name}\" (`{slug}`). Written by herdr-projects; `doctor --fix` refreshes it.\n\n\
         If your working directory is exactly this folder, you are the coordinator of {name}: run `{prefix} skill` now and follow what it prints, and run `{prefix} context {slug}` now and whenever you need project state.\n\n\
         If your working directory is under `threads/`, you are a thread: your brief is in your own folder (`.herdr-project/{slug}-<id>/brief.md`); ignore the rest of this file.\n"
    )
}

/// The command prefix `AGENTS.md` carries, so `doctor` can check that its
/// binary still exists.
pub fn prefix_in_agents_md(text: &str) -> Option<String> {
    let line = text.lines().find(|l| l.starts_with("If your working directory is exactly this folder"))?;
    let start = line.find('`')? + 1;
    let rest = &line[start..];
    let end = rest.find(" skill`")?;
    Some(rest[..end].to_string())
}

pub const PR_FOLLOWUP: &str = "routines/pr-followup.md";

const PR_FOLLOWUP_TEMPLATE: &str = "+++\non = \"pr\"\nevents = [\"checks-failed\", \"review\"]\nenabled = true\n+++\n\nFix the failing checks and address the new review comments on your pull request. Read them with `gh`, push the fixes, reply where a reviewer asked something, and then rewrite your report. If a comment asks for something outside your task, say so in the report instead of doing it.\n\nAuthorized: push to this thread's branch and comment on this pull request only (replies to review comments; no merge, no new pull requests, no other branches).\n";

/// Every default `routines/pr-followup.md` an earlier version wrote. A file
/// with exactly one of these texts, its `enabled` line aside, was never
/// edited, so `doctor --fix` replaces it.
const PR_FOLLOWUP_SHIPPED: [&str; 1] = [
    "+++\non = \"pr\"\nevents = [\"checks-failed\", \"review\"]\nenabled = true\n+++\n\nFix the failing checks and address the new review comments on your pull request. Read them with `gh`, push the fixes, reply where a reviewer asked something, and then rewrite your report. If a comment asks for something outside your task, say so in the report instead of doing it.\n",
];

/// The `enabled = …` line of an earlier default, when `text` is one: turning
/// the routine off (`routine toggle`, the popup) is not an edit.
fn earlier_pr_followup(text: &str) -> Option<&str> {
    let line = text.lines().find(|l| l.starts_with("enabled = "))?;
    PR_FOLLOWUP_SHIPPED.contains(&text.replacen(line, "enabled = true", 1).as_str()).then_some(line)
}

/// The ready-made `pr` routine (enabled by default; the popup or the
/// coordinator turns it off). Written by `new`, and by `doctor --fix` when
/// missing or an unedited earlier default, whose `enabled` value it keeps.
pub fn write_default_routine(project: &Project) -> Result<bool> {
    let path = project.dir().join(PR_FOLLOWUP);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => match earlier_pr_followup(&text) {
            Some(line) => PR_FOLLOWUP_TEMPLATE.replacen("enabled = true", line, 1),
            None => return Ok(false),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => PR_FOLLOWUP_TEMPLATE.to_string(),
        Err(_) => return Ok(false),
    };
    write_atomic(&path, text.as_bytes())?;
    Ok(true)
}

/// A note for `doctor` when an enabled `routines/pr-followup.md` was edited
/// and has no `Authorized:` line: `doctor --fix` leaves it, and without the
/// line an mstack thread will not push its fixes. An unedited earlier default
/// is a `priming_problems` entry instead, which `--fix` repairs.
pub fn pr_followup_note(project: &Project) -> Option<String> {
    let text = std::fs::read_to_string(project.dir().join(PR_FOLLOWUP)).ok()?;
    let line = PR_FOLLOWUP_TEMPLATE.lines().last().unwrap_or_default();
    let disabled = crate::routine::parse("pr-followup", &text).is_ok_and(|routine| !routine.enabled);
    (!text.contains("Authorized:") && earlier_pr_followup(&text).is_none() && !disabled)
        .then(|| format!("routines/pr-followup.md is edited and has no `Authorized:` line, so `doctor --fix` leaves it; add `{line}` if its threads should push"))
}

/// Writes `AGENTS.md`, `CLAUDE.md` (a relative symbolic link to it),
/// `.omp/config.yml`, `.mstack/config.yml` when mstack reads it, and creates
/// `uploads/`. Idempotent; used by `new` and by `doctor --fix`.
pub fn write_priming(project: &Project, prefix: &str, env: &Env) -> Result<()> {
    let dir = project.dir();
    let (settings, _) = project.read_project_md()?;
    let name = display_name(&settings.name, &project.slug);
    let agents = dir.join("AGENTS.md");
    if let Ok(existing) = std::fs::read_to_string(&agents)
        && !existing.contains("Written by herdr-projects")
    {
        // Someone else's AGENTS.md: keep its text beside ours, once.
        let kept = dir.join("AGENTS.md.before-herdr-projects");
        if !kept.exists() {
            std::fs::rename(&agents, &kept)?;
        }
    }
    write_atomic(&agents, agents_md(&name, &project.slug, prefix).as_bytes())?;
    let claude = dir.join("CLAUDE.md");
    let link_ok = std::fs::read_link(&claude).is_ok_and(|target| target == Path::new("AGENTS.md"));
    if !link_ok {
        if std::fs::symlink_metadata(&claude).is_ok() {
            // A regular file or a link elsewhere: keep its text beside it, once.
            let kept = dir.join("CLAUDE.md.before-herdr-projects");
            if !kept.exists() {
                std::fs::rename(&claude, &kept)?;
            } else {
                std::fs::remove_file(&claude)?;
            }
        }
        std::os::unix::fs::symlink("AGENTS.md", &claude).with_context(|| format!("could not link {}", claude.display()))?;
    }
    if !dir.join("uploads").is_dir() {
        std::fs::create_dir(dir.join("uploads"))?;
    }
    write_default_routine(project)?;
    // Only OMP reads these files: a failure here must not stop `open` or
    // `doctor --fix` for other harnesses; `priming_problems` reports it.
    let _ = write_omp_config(project, prefix, env);
    let mstack = dir.join(MSTACK_CONFIG);
    if mstack_wanted(project, env) {
        let _ = write_managed(&mstack, &mstack_config());
    } else if is_managed(&mstack) {
        // An mstack before 0.4.0 refuses the whole file over `mode`.
        let _ = std::fs::remove_file(&mstack);
    }
    Ok(())
}

pub const OMP_CONFIG: &str = ".omp/config.yml";
pub const OMP_CONFIG_MARKER: &str = "# herdr-projects: managed";

/// OMP layers `<cwd>/.omp/config.yml` (no walk-up) over the user's config, and
/// only the coordinator runs in exactly this folder. A
/// `deny` or `prompt` pattern holds even under `approvalMode: yolo`, but only
/// for the bash tool: `eval` and `debug` can reach a shell too, hence their own
/// `prompt`. OMP replaces arrays whole, so this list would replace the global
/// `bash.patterns` of the coordinator's profile: `extra` copies them after
/// ours (None: that profile's config could not be read). OMP takes the first
/// rule that matches the command or any part of it, so the profile's `deny`
/// rules also come before our `prompt` rules: confirming `thread resolve` in
/// `… thread resolve x && gh pr merge 1` must not let a denied merge run.
/// Every rule starts with the exact
/// prefix the coordinator is told to use, so a heredoc body or a slug such as
/// `configure-ci` never matches. Advice-level, like COORDINATOR.md: a wrapped
/// or renamed binary evades it.
// ponytail: a quoted prefix (a path with spaces) misses OMP's per-segment
// check, which strips quotes; `cd x && <prefix> configure` then passes.
fn omp_config(prefix: &str, profile: &str, extra: Option<&[crate::omp::PatternRule]>) -> String {
    let deny = ["routine approve *", "configure", "configure *", "unconfigure", "unconfigure *"];
    let prompt = ["thread resolve *", "sweep", "sweep *", "archive *", "delete *"];
    let mut text = format!(
        "{OMP_CONFIG_MARKER}. `doctor --fix` rewrites this file; delete this line to keep your own edits.\n\
         # The coordinator never runs these itself; resolve, sweep, archive and delete wait for you to confirm.\n"
    );
    if extra.is_none() {
        text.push_str("# profile config unreadable: only the herdr-projects rules apply; `doctor` names the error.\n");
    }
    text.push_str("tools:\n  approval:\n    eval: prompt\n    debug: prompt\nbash:\n  patterns:\n");
    // A JSON string is a valid YAML double-quoted scalar.
    let rule = |text: &mut String, matcher: &str, approval: &str| {
        text.push_str(&format!("    - match: {}\n      approval: {approval}\n", serde_json::to_string(matcher).expect("a string serializes")));
    };
    for sub in deny {
        rule(&mut text, &format!("{prefix} {sub}"), "deny");
    }
    let extra = extra.unwrap_or_default();
    let name = if profile.is_empty() { "default" } else { profile };
    let denied: Vec<_> = extra.iter().filter(|r| r.approval == "deny").collect();
    if !denied.is_empty() {
        text.push_str(&format!("    # The deny rules of OMP profile {name}, again before the confirmations below.\n"));
    }
    for r in denied {
        rule(&mut text, &r.matcher, "deny");
    }
    for sub in prompt {
        rule(&mut text, &format!("{prefix} {sub}"), "prompt");
    }
    if !extra.is_empty() {
        text.push_str(&format!("    # The global bash.patterns of OMP profile {name}, which this list would otherwise replace.\n"));
    }
    for r in extra {
        rule(&mut text, &r.matcher, &r.approval);
    }
    text
}

/// The coordinator's OMP profile: the one the recorded OMP coordinator runs
/// under (`open` and the ticker keep it current), else the project setting;
/// normalized.
fn coordinator_profile(project: &Project) -> Result<String> {
    if let Some(record) = project.coordinator().filter(|record| record.agent == "omp") {
        return crate::omp::normalize_profile(&record.omp_profile);
    }
    let (settings, _) = project.read_project_md()?;
    crate::omp::normalize_profile(&settings.omp_profile)
}

/// How every `priming_problems` note about the coordinator's profile starts.
pub const PROFILE_PROBLEM: &str = "the coordinator's OMP profile";

/// What `.omp/config.yml` should hold, and why the profile's rules are
/// missing from it, if they are.
fn wanted_omp_config(project: &Project, prefix: &str, env: &Env) -> (String, Option<String>) {
    // A recorded OMP coordinator's profile wins over the setting, so while
    // one is recorded only a coordinator started under another profile helps.
    let other = if project.coordinator().is_some_and(|record| record.agent == "omp") {
        format!("start the coordinator under another profile with `open {} --profile <name>`", project.slug)
    } else {
        format!("run `set {} omp_profile <name>` to use another profile", project.slug)
    };
    let only_ours = ".omp/config.yml has only the herdr-projects rules";
    let profile = match coordinator_profile(project) {
        Ok(profile) => profile,
        Err(error) => return (omp_config(prefix, "", None), Some(format!("{PROFILE_PROBLEM} is invalid ({error:#}); {only_ours}; {other}"))),
    };
    match crate::omp::bash_patterns(env, &profile) {
        Ok(rules) => (omp_config(prefix, &profile, Some(rules.as_slice())), None),
        Err(error) => {
            let path = crate::omp::config_path(env, &profile).map(|p| p.display().to_string()).unwrap_or_default();
            (omp_config(prefix, "", None), Some(format!("{PROFILE_PROBLEM} config is not usable ({error:#}); {only_ours}; fix {path}, or {other}")))
        }
    }
}

/// Writes `.omp/config.yml` when it is missing or an out-of-date managed file.
/// Returns whether it wrote.
pub fn write_omp_config(project: &Project, prefix: &str, env: &Env) -> Result<bool> {
    write_managed(&project.dir().join(OMP_CONFIG), &wanted_omp_config(project, prefix, env).0)
}

pub const MSTACK_CONFIG: &str = ".mstack/config.yml";

/// mstack layers `<cwd>/.mstack/config.yml` over its user file. The
/// coordinator gets everything as queued messages, which never run `/mstack
/// on`, so this file starts its sessions with mstack mode on. mstack skills
/// promote their records under `promotion.directory`, which would be
/// `.mstack/`; the coordinator may write only `scratch/`.
fn mstack_config() -> String {
    format!("{OMP_CONFIG_MARKER}. `doctor --fix` rewrites this file; delete this line to keep your own edits.\nmode: true\npromotion:\n  directory: scratch/mstack\n")
}

/// Whether the coordinator's OMP profile has an enabled mstack that reads
/// `mode` (0.4.0 and later).
fn mstack_wanted(project: &Project, env: &Env) -> bool {
    coordinator_profile(project).is_ok_and(|profile| crate::omp::mstack_version(env, &profile).is_some_and(|version| version >= (0, 4, 0)))
}

/// True when `path` is a file we manage (our marker on line 1).
fn is_managed(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|text| text.starts_with(OMP_CONFIG_MARKER))
}

/// Writes a managed file when it is missing or an out-of-date managed copy.
/// A file without our marker, or one that cannot be read, is left alone (the
/// user's own config is never overwritten). Returns whether it wrote.
fn write_managed(path: &Path, wanted: &str) -> Result<bool> {
    match std::fs::read_to_string(path) {
        Ok(text) if text == wanted || !text.starts_with(OMP_CONFIG_MARKER) => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Ok(false),
    }
    std::fs::create_dir_all(path.parent().context("path has no parent")?)?;
    write_atomic(path, wanted.as_bytes())?;
    Ok(true)
}

/// What is wrong with a managed file `rel` in `dir`, if anything.
fn managed_problem(dir: &Path, rel: &str, wanted: &str) -> Option<String> {
    match std::fs::read_to_string(dir.join(rel)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(format!("{rel} is missing")),
        Err(error) => Some(format!("{rel} cannot be read ({error})")),
        Ok(text) if text.starts_with(OMP_CONFIG_MARKER) && text != wanted => Some(format!("{rel} is out of date")),
        // The user's own file (no marker) is theirs to keep.
        Ok(_) => None,
    }
}

/// What `doctor` finds wrong with a project's priming files, as short notes.
pub fn priming_problems(project: &Project, prefix: &str, env: &Env) -> Vec<String> {
    let dir = project.dir();
    let mut problems = Vec::new();
    match std::fs::read_to_string(dir.join("AGENTS.md")) {
        Err(_) => problems.push("AGENTS.md is missing".into()),
        Ok(text) => match prefix_in_agents_md(&text) {
            None => problems.push("AGENTS.md does not name the binary".into()),
            Some(found) => {
                let binary = found.split(" --root ").next().unwrap_or("").trim_matches('\'');
                if !Path::new(binary).is_file() {
                    problems.push(format!("AGENTS.md points at a binary that does not exist ({binary})"));
                } else if found != prefix {
                    problems.push("AGENTS.md names another binary or root than this one".into());
                }
            }
        },
    }
    if !std::fs::read_link(dir.join("CLAUDE.md")).is_ok_and(|t| t == Path::new("AGENTS.md")) {
        problems.push("CLAUDE.md is not a link to AGENTS.md".into());
    }
    if !dir.join("uploads").is_dir() {
        problems.push("uploads/ is missing".into());
    }
    match std::fs::read_to_string(dir.join(PR_FOLLOWUP)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => problems.push("routines/pr-followup.md is missing".into()),
        Ok(text) if earlier_pr_followup(&text).is_some() => problems.push("routines/pr-followup.md is an earlier default, without the `Authorized:` line".into()),
        _ => {}
    }
    let (wanted, profile_problem) = wanted_omp_config(project, prefix, env);
    problems.extend(managed_problem(&dir, OMP_CONFIG, &wanted));
    if let Some(problem) = profile_problem
        && is_managed(&dir.join(OMP_CONFIG))
    {
        problems.push(problem);
    }
    if mstack_wanted(project, env) {
        problems.extend(managed_problem(&dir, MSTACK_CONFIG, &mstack_config()));
    } else if is_managed(&dir.join(MSTACK_CONFIG)) {
        problems.push(format!("{MSTACK_CONFIG} is no longer wanted"));
    }
    problems
}

/// Creates the folder and skeleton files. The only code path that creates a
/// project's directories. Fails if the slug exists.
pub fn create(root: &Path, name: &str, goal: &str, repos: Vec<Repo>) -> Result<Project> {
    let slug = slug_from_name(name)?;
    let project = Project {
        root: root.to_path_buf(),
        slug: slug.clone(),
    };
    let dir = project.dir();
    if dir.exists() {
        bail!("`{slug}` already exists in {}", root.display());
    }
    let repos = repos
        .into_iter()
        .map(|repo| match repo.machine {
            // A remote path is stored as it is on its own machine.
            Some(_) => repo,
            None => Repo {
                path: std::fs::canonicalize(&repo.path)
                    .or_else(|_| std::path::absolute(&repo.path))
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or(repo.path),
                machine: None,
            },
        })
        .collect();
    let settings = Settings {
        name: display_name(name, &slug),
        goal: goal.to_string(),
        repos,
        ..Settings::default()
    };
    let front = toml::to_string(&settings)?;

    std::fs::create_dir_all(root)?;
    std::fs::create_dir(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    for sub in SUBDIRS {
        std::fs::create_dir_all(dir.join(sub))?;
    }
    write_atomic(
        &dir.join("MEMORY.md"),
        b"# Memory\n\nOne line per memory file: `- [title](memory/file.md): what it holds`.\n",
    )?;
    write_atomic(&dir.join("TASKS.md"), TASKS_TEMPLATE.as_bytes())?;
    write_atomic(&dir.join(PR_FOLLOWUP), PR_FOLLOWUP_TEMPLATE.as_bytes())?;
    // The same prefix AGENTS.md and COORDINATOR.md tell the coordinator to use.
    // PROJECT.md does not exist yet: `write_priming`, which runs before any
    // coordinator starts, adds the profile's rules and `.mstack/config.yml`.
    write_managed(&dir.join(OMP_CONFIG), &omp_config(&crate::coordinator::current_prefix(root)?, "", Some(&[])))?;
    write_json(&project.state_dir().join("project.json"), &ProjectState::default())?;
    // PROJECT.md last: a folder without it is not a project, so a half-made
    // skeleton is never picked up by `list` or the ticker.
    write_atomic(
        &project.project_md(),
        format!("+++\n{front}+++\n\n{INSTRUCTIONS_TEMPLATE}").as_bytes(),
    )?;
    Ok(project)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_folders_with_project_md_count() {
        let root = tempfile::tempdir().unwrap();
        for name in ["b", "a", ".trash", "empty", "Not_A_Slug"] {
            std::fs::create_dir(root.path().join(name)).unwrap();
        }
        for name in ["b", "a", ".trash", "Not_A_Slug"] {
            std::fs::write(root.path().join(name).join("PROJECT.md"), "").unwrap();
        }
        assert_eq!(list_slugs(root.path()), ["a", "b"]);
        assert!(list_slugs(&root.path().join("missing")).is_empty());
    }

    #[test]
    fn slug_validation() {
        for good in ["a", "demo", "demo-2", "0x", &"a".repeat(40)] {
            assert!(validate_slug(good).is_ok(), "{good}");
        }
        for bad in ["", "-a", "A", "a_b", "a/b", "../x", "a b", ".", "..", &"a".repeat(41)] {
            assert!(validate_slug(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_slug_like_name_is_humanized_and_a_typed_name_is_kept() {
        assert_eq!(humanize("herdr-projects"), "Herdr Projects");
        assert_eq!(humanize("gtm_ai"), "Gtm Ai");
        assert_eq!(humanize("-v2--api-"), "V2 Api");
        assert_eq!(display_name("herdr-projects", "herdr-projects"), "Herdr Projects");
        assert_eq!(display_name("", "herdr-projects"), "Herdr Projects");
        assert_eq!(display_name("  ", "demo"), "Demo");
        for typed in ["GTM AI", "my project", "Demo", "herdr-Projects"] {
            assert_eq!(display_name(typed, "x"), typed);
        }
    }

    #[test]
    fn create_stores_a_display_name_and_keeps_the_slug() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "herdr-projects", "", vec![]).unwrap();
        assert_eq!(project.slug, "herdr-projects");
        assert_eq!(project.read_project_md().unwrap().0.name, "Herdr Projects");
        let project = create(root.path(), "GTM AI", "", vec![]).unwrap();
        assert_eq!(project.slug, "gtm-ai");
        assert_eq!(project.read_project_md().unwrap().0.name, "GTM AI");
    }

    #[test]
    fn slug_derivation_and_name_refusals() {
        assert_eq!(slug_from_name("My Demo  Project!").unwrap(), "my-demo-project");
        assert_eq!(slug_from_name("  Ünï 42 ").unwrap(), "n-42");
        assert_eq!(slug_from_name(&"x".repeat(60)).unwrap().len(), 40);
        for bad in ["../x", "a/b", "a\\b", "..", "!!!", ""] {
            assert!(slug_from_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn create_writes_the_skeleton_and_refuses_a_second_time() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().join("root");
        let project = create(
            &root,
            "Demo",
            "Ship \"it\"",
            vec![parse_repo_arg("/srv/app@box"), parse_repo_arg("/no/such/repo")],
        )
        .unwrap();
        assert_eq!(project.slug, "demo");
        for sub in ["memory", "scratch", "routines", "threads", "inbox/done", "library", "uploads", ".state"] {
            assert!(project.dir().join(sub).is_dir(), "{sub}");
        }
        assert!(project.dir().join("MEMORY.md").is_file());
        assert!(project.dir().join("TASKS.md").is_file());
        let (settings, body) = project.read_project_md().unwrap();
        assert_eq!(settings.name, "Demo");
        assert_eq!(settings.goal, "Ship \"it\"");
        assert_eq!(settings.coordinator_agent, "claude");
        assert_eq!(settings.max_parallel_threads, 3);
        assert_eq!(settings.auto_resolve_days, 7);
        assert!(settings.nudge);
        assert!(project.dir().join(PR_FOLLOWUP).is_file());
        assert!(crate::routine::load_all(&project).1.is_empty(), "the default routine parses");
        assert_eq!(
            settings.repos,
            vec![
                Repo { path: "/srv/app".into(), machine: Some("box".into()) },
                Repo { path: "/no/such/repo".into(), machine: None },
            ]
        );
        assert!(body.starts_with("# Instructions"));
        assert_eq!(project.status(), Status::Active);
        assert!(create(&root, "demo", "", vec![]).is_err());
    }

    #[test]
    fn priming_files_are_written_linked_and_checked() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let project = create(root.path(), "Demo Project", "", vec![]).unwrap();
        let prefix = format!("{} --root {}", std::env::current_exe().unwrap().display(), root.path().display());
        write_priming(&project, &prefix, &env).unwrap();
        let text = std::fs::read_to_string(project.dir().join("AGENTS.md")).unwrap();
        assert!(text.contains("you are the coordinator of Demo Project"));
        assert!(text.contains(&format!("`{prefix} skill`")));
        assert!(text.contains(&format!("`{prefix} context demo-project`")));
        assert!(text.contains("under `threads/`, you are a thread"));
        assert_eq!(prefix_in_agents_md(&text).as_deref(), Some(prefix.as_str()));
        assert_eq!(std::fs::read_link(project.dir().join("CLAUDE.md")).unwrap(), Path::new("AGENTS.md"));
        assert!(project.dir().join("uploads").is_dir());
        assert!(priming_problems(&project, &prefix, &env).is_empty());

        // Idempotent, and a stale binary path is reported.
        write_priming(&project, &prefix, &env).unwrap();
        let stale = agents_md("Demo Project", "demo-project", "/no/such/binary --root /r");
        std::fs::write(project.dir().join("AGENTS.md"), stale).unwrap();
        let problems = priming_problems(&project, &prefix, &env);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("does not exist"));
        write_priming(&project, &prefix, &env).unwrap();
        assert!(priming_problems(&project, &prefix, &env).is_empty());

        // A foreign AGENTS.md is kept beside ours.
        std::fs::write(project.dir().join("AGENTS.md"), "codex notes").unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(project.dir().join("AGENTS.md.before-herdr-projects")).unwrap(), "codex notes");
        // A hand-written CLAUDE.md is kept beside the link, not lost.
        std::fs::remove_file(project.dir().join("CLAUDE.md")).unwrap();
        std::fs::write(project.dir().join("CLAUDE.md"), "mine").unwrap();
        assert!(priming_problems(&project, &prefix, &env).iter().any(|p| p.contains("CLAUDE.md")));
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(project.dir().join("CLAUDE.md.before-herdr-projects")).unwrap(), "mine");
        assert!(priming_problems(&project, &prefix, &env).is_empty());
    }

    #[test]
    fn omp_config_is_written_refreshed_and_never_replaces_a_foreign_file() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let project = create(root.path(), "demo", "", vec![]).unwrap();
        let prefix = crate::coordinator::current_prefix(root.path()).unwrap();
        let path = project.dir().join(OMP_CONFIG);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), omp_config(&prefix, "", Some(&[])), "`create` writes it for the coordinator's prefix");
        std::fs::remove_file(&path).unwrap();
        assert!(priming_problems(&project, &prefix, &env).iter().any(|p| p.contains(".omp/config.yml is missing")));
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), omp_config(&prefix, "", Some(&[])));
        assert!(priming_problems(&project, &prefix, &env).is_empty());
        assert!(!write_omp_config(&project, &prefix, &env).unwrap(), "a current file is left alone");

        // An older managed file, or one for another binary or root, is reported and rewritten.
        std::fs::write(&path, format!("{OMP_CONFIG_MARKER}\nbash: {{}}\n")).unwrap();
        assert!(priming_problems(&project, &prefix, &env).iter().any(|p| p.contains("out of date")));
        assert!(write_omp_config(&project, &prefix, &env).unwrap());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), omp_config(&prefix, "", Some(&[])));
        assert!(priming_problems(&project, "/other/hp --root /r", &env).iter().any(|p| p.contains("out of date")));

        // The user's own config (marker removed) is theirs: kept and not a problem.
        std::fs::write(&path, "tools:\n  approvalMode: yolo\n").unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tools:\n  approvalMode: yolo\n");
        assert!(priming_problems(&project, &prefix, &env).is_empty());

        // A file that cannot be read as text is never replaced.
        std::fs::write(&path, b"\xff\xfe").unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"\xff\xfe");
        assert!(priming_problems(&project, &prefix, &env).iter().any(|p| p.contains("cannot be read")));

        // A directory in the way is left alone too.
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert!(path.is_dir());

        // A failed write does not fail the other priming files (`open` for any harness).
        use std::os::unix::fs::PermissionsExt;
        let omp = project.dir().join(".omp");
        std::fs::remove_dir(&path).unwrap();
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o500)).unwrap();
        std::fs::remove_file(project.dir().join("AGENTS.md")).unwrap();
        let result = write_priming(&project, &prefix, &env);
        std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o700)).unwrap();
        result.unwrap();
        assert!(project.dir().join("AGENTS.md").is_file());
    }

    #[test]
    fn omp_config_keeps_the_coordinator_profiles_patterns_after_ours() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let project = create(root.path(), "demo", "", vec![]).unwrap();
        let prefix = crate::coordinator::current_prefix(root.path()).unwrap();
        let path = project.dir().join(OMP_CONFIG);
        let set_profile = |value: &str| {
            let (settings, _) = project.read_project_md().unwrap();
            let text = std::fs::read_to_string(project.project_md()).unwrap();
            let from = format!("omp_profile = {}", serde_json::to_string(&settings.omp_profile).unwrap());
            std::fs::write(project.project_md(), text.replacen(&from, &format!("omp_profile = {}", serde_json::to_string(value).unwrap()), 1)).unwrap();
            assert_eq!(project.read_project_md().unwrap().0.omp_profile, value);
        };
        set_profile("neurable");
        let agent = home.path().join(".omp/profiles/neurable/agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(home.path().join(".omp/agent")).unwrap();
        std::fs::write(home.path().join(".omp/agent/config.yml"), "bash:\n  patterns:\n    - match: \"*rm -rf*\"\n      approval: deny\n").unwrap();
        std::fs::write(agent.join("config.yml"), "bash:\n  patterns:\n    - match: \"*gh pr merge*\"\n      approval: deny\n    - match: \"*\"\n      approval: prompt\n").unwrap();
        assert!(priming_problems(&project, &prefix, &env).iter().any(|p| p.contains("out of date")), "`create` wrote ours only");
        write_priming(&project, &prefix, &env).unwrap();
        let config = std::fs::read_to_string(&path).unwrap();
        // Ours come first, so the profile's catch-all never shadows them.
        assert_eq!(omp_approval(&config, &format!("{prefix} configure")).as_deref(), Some("deny"));
        assert_eq!(omp_approval(&config, "gh pr merge 7").as_deref(), Some("deny"));
        assert_eq!(omp_approval(&config, "ls").as_deref(), Some("prompt"));
        assert_eq!(omp_approval(&config, "rm -rf /tmp/x").as_deref(), Some("prompt"), "another profile's rules stay out");
        // Confirming a resolve never lets a command the profile denies ride along.
        assert_eq!(omp_approval(&config, &format!("{prefix} thread resolve demo t-0001 && gh pr merge 42 --squash")).as_deref(), Some("deny"));
        assert_eq!(omp_approval(&config, &format!("{prefix} thread resolve demo t-0001")).as_deref(), Some("prompt"));
        assert!(priming_problems(&project, &prefix, &env).is_empty());

        // An edit to the profile's config makes the file out of date.
        std::fs::write(agent.join("config.yml"), "bash:\n  patterns:\n    - match: \"*gh pr merge*\"\n      approval: deny\n").unwrap();
        assert!(priming_problems(&project, &prefix, &env).iter().any(|p| p.contains("out of date")));
        write_priming(&project, &prefix, &env).unwrap();
        let config = std::fs::read_to_string(&path).unwrap();
        assert_eq!(omp_approval(&config, "ls"), None);
        assert_eq!(omp_approval(&config, "gh pr merge 7").as_deref(), Some("deny"));

        // An unreadable profile config leaves ours only, and doctor says why and where.
        std::fs::write(agent.join("config.yml"), b"\xff\xfe").unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), omp_config(&prefix, "", None));
        let problems = priming_problems(&project, &prefix, &env);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with(PROFILE_PROBLEM) && problems[0].contains(&format!("fix {}", agent.join("config.yml").display())), "{problems:?}");

        // So does a profile name OMP would refuse.
        std::fs::write(agent.join("config.yml"), "bash:\n  patterns: []\n").unwrap();
        set_profile("Bad Name");
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), omp_config(&prefix, "", None));
        assert!(priming_problems(&project, &prefix, &env).iter().any(|p| p.starts_with(PROFILE_PROBLEM) && p.contains("`set demo omp_profile <name>`")));

        // The user's own file is still theirs, and the profile error is not reported for it.
        std::fs::write(&path, "tools: {}\n").unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "tools: {}\n");
        assert!(priming_problems(&project, &prefix, &env).is_empty());
    }

    #[test]
    fn priming_follows_the_profile_the_recorded_omp_coordinator_runs_under() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let project = create(root.path(), "demo", "", vec![]).unwrap();
        let prefix = crate::coordinator::current_prefix(root.path()).unwrap();
        let omp = home.path().join(".omp");
        std::fs::create_dir_all(omp.join("agent")).unwrap();
        std::fs::create_dir_all(omp.join("profiles/neurable/agent")).unwrap();
        std::fs::write(omp.join("profiles/neurable/agent/config.yml"), "bash:\n  patterns:\n    - match: \"*gh *pr merge*\"\n      approval: deny\n").unwrap();
        // mstack 0.4.0 is enabled for neurable only.
        let plugins = omp.join("profiles/neurable/plugins");
        std::fs::create_dir_all(plugins.join("node_modules/@mgpai22/mstack")).unwrap();
        std::fs::write(plugins.join("node_modules/@mgpai22/mstack/package.json"), r#"{"version":"0.4.0"}"#).unwrap();
        std::fs::write(plugins.join("omp-plugins.lock.json"), r#"{"plugins":{"@mgpai22/mstack":{"enabled":true}}}"#).unwrap();
        let config = || std::fs::read_to_string(project.dir().join(OMP_CONFIG)).unwrap();
        let mstack = project.dir().join(MSTACK_CONFIG);

        // The project setting is the default profile, but the coordinator runs neurable.
        project.update_coordinator(|c| {
            c.agent = "omp".into();
            c.omp_profile = "neurable".into();
        })
        .unwrap();
        assert!(priming_problems(&project, &prefix, &env).iter().any(|p| p == ".omp/config.yml is out of date"));
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(omp_approval(&config(), "gh pr merge 7").as_deref(), Some("deny"));
        assert!(mstack.is_file());
        assert!(priming_problems(&project, &prefix, &env).is_empty());

        // Its config broken: the setting cannot help while the record wins, `open --profile` can.
        let neurable = omp.join("profiles/neurable/agent/config.yml");
        let good = std::fs::read(&neurable).unwrap();
        std::fs::write(&neurable, b"\xff\xfe").unwrap();
        let problems = priming_problems(&project, &prefix, &env);
        assert!(problems.iter().any(|p| p.starts_with(PROFILE_PROBLEM) && p.ends_with("or start the coordinator under another profile with `open demo --profile <name>`")), "{problems:?}");
        assert!(!problems.iter().any(|p| p.contains("omp_profile <name>")), "{problems:?}");
        std::fs::write(&neurable, good).unwrap();

        // Another harness: back to the project setting, which has neither.
        project.update_coordinator(|c| c.agent = "claude".into()).unwrap();
        let problems = priming_problems(&project, &prefix, &env);
        assert!(problems.contains(&".omp/config.yml is out of date".to_string()) && problems.contains(&".mstack/config.yml is no longer wanted".to_string()), "{problems:?}");
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(omp_approval(&config(), "gh pr merge 7"), None);
        assert!(!mstack.exists(), "a managed copy nobody wants is removed");
        assert!(priming_problems(&project, &prefix, &env).is_empty());
    }

    #[test]
    fn mstack_config_turns_mode_on_only_when_the_coordinators_mstack_reads_it() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let project = create(root.path(), "demo", "", vec![]).unwrap();
        let prefix = crate::coordinator::current_prefix(root.path()).unwrap();
        let path = project.dir().join(MSTACK_CONFIG);
        let plugins = home.path().join(".omp/plugins");
        let package = plugins.join("node_modules/@mgpai22/mstack");
        std::fs::create_dir_all(&package).unwrap();
        let lock = |enabled: bool| std::fs::write(plugins.join("omp-plugins.lock.json"), format!(r#"{{"plugins":{{"@mgpai22/mstack":{{"enabled":{enabled}}}}}}}"#)).unwrap();
        lock(true);
        std::fs::write(package.join("package.json"), r#"{"version":"0.3.0"}"#).unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert!(!path.exists(), "0.3.0 has no `mode` key");
        assert!(priming_problems(&project, &prefix, &env).is_empty());

        std::fs::write(package.join("package.json"), r#"{"version":"0.4.0"}"#).unwrap();
        assert!(priming_problems(&project, &prefix, &env).contains(&".mstack/config.yml is missing".to_string()));
        write_priming(&project, &prefix, &env).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with(OMP_CONFIG_MARKER) && text.lines().any(|l| l == "mode: true"), "{text}");
        assert!(text.contains("promotion:\n  directory: scratch/mstack\n"), "mstack records go where the coordinator may write: {text}");
        assert!(priming_problems(&project, &prefix, &env).is_empty());

        // A stale managed copy is rewritten; the user's own file is kept.
        std::fs::write(&path, format!("{OMP_CONFIG_MARKER}\nmode: false\n")).unwrap();
        assert!(priming_problems(&project, &prefix, &env).contains(&".mstack/config.yml is out of date".to_string()));
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        std::fs::write(&path, "mode: false\n").unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mode: false\n");
        assert!(priming_problems(&project, &prefix, &env).is_empty());

        // A disabled mstack, or one in another profile, wants no file: a
        // managed copy is removed, the user's own is kept.
        std::fs::write(&path, &text).unwrap();
        lock(false);
        assert!(priming_problems(&project, &prefix, &env).contains(&".mstack/config.yml is no longer wanted".to_string()));
        write_priming(&project, &prefix, &env).unwrap();
        assert!(!path.exists());
        assert!(priming_problems(&project, &prefix, &env).is_empty());
        std::fs::write(&path, "mode: true\n").unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "mode: true\n");
        assert!(priming_problems(&project, &prefix, &env).is_empty());
        std::fs::remove_file(&path).unwrap();
        lock(true);
        let text = std::fs::read_to_string(project.project_md()).unwrap();
        std::fs::write(project.project_md(), text.replacen("omp_profile = \"\"", "omp_profile = \"neurable\"", 1)).unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn an_unedited_earlier_pr_followup_default_is_replaced_and_an_edited_one_kept() {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let project = create(root.path(), "demo", "", vec![]).unwrap();
        let prefix = crate::coordinator::current_prefix(root.path()).unwrap();
        let path = project.dir().join(PR_FOLLOWUP);
        assert_eq!(pr_followup_note(&project), None);

        std::fs::write(&path, PR_FOLLOWUP_SHIPPED[0]).unwrap();
        assert!(priming_problems(&project, &prefix, &env).iter().any(|p| p.contains("earlier default")));
        assert_eq!(pr_followup_note(&project), None, "`--fix` repairs it");
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PR_FOLLOWUP_TEMPLATE);
        assert!(priming_problems(&project, &prefix, &env).is_empty());

        // Turned off, it is still unedited: refreshed with its `enabled` value kept.
        let off = PR_FOLLOWUP_SHIPPED[0].replace("enabled = true", "enabled = false");
        std::fs::write(&path, &off).unwrap();
        assert!(priming_problems(&project, &prefix, &env).iter().any(|p| p.contains("earlier default")));
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), PR_FOLLOWUP_TEMPLATE.replace("enabled = true", "enabled = false"));
        assert_eq!(pr_followup_note(&project), None);

        let edited = PR_FOLLOWUP_SHIPPED[0].replace("Fix the failing checks", "Fix the checks");
        std::fs::write(&path, &edited).unwrap();
        write_priming(&project, &prefix, &env).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), edited);
        assert!(priming_problems(&project, &prefix, &env).is_empty());
        assert!(pr_followup_note(&project).is_some_and(|note| note.contains("Authorized: push to this thread's branch")));
        // An edited routine that is off sends nothing: no note.
        std::fs::write(&path, edited.replace("enabled = true", "enabled = false")).unwrap();
        assert_eq!(pr_followup_note(&project), None);
        std::fs::write(&path, format!("{edited}\nAuthorized: push to this thread's branch only.\n")).unwrap();
        assert_eq!(pr_followup_note(&project), None);
    }

    /// OMP's `bash.patterns` semantics (tools/bash.ts @740f3e3154): whitespace
    /// runs, newlines included, become one space; `*` matches anything, the rest
    /// is literal; a `deny`/`prompt` rule fires on the whole command or on any
    /// segment split at newlines and `;&|()`; the first matching rule wins.
    fn omp_approval(config: &str, command: &str) -> Option<String> {
        fn glob(pattern: &str, text: &str) -> bool {
            let parts: Vec<&str> = pattern.split('*').collect();
            let (first, last) = (parts[0], parts[parts.len() - 1]);
            if parts.len() == 1 {
                return pattern == text;
            }
            if !text.starts_with(first) || text.len() < first.len() + last.len() || !text[first.len()..].ends_with(last) {
                return false;
            }
            let mut rest = &text[first.len()..text.len() - last.len()];
            for part in &parts[1..parts.len() - 1] {
                match rest.find(part) {
                    Some(at) => rest = &rest[at + part.len()..],
                    None => return false,
                }
            }
            true
        }
        let normalize = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut candidates = vec![normalize(command)];
        candidates.extend(command.split(['\n', ';', '&', '|', '(', ')']).map(normalize).filter(|s| !s.is_empty()));
        let lines: Vec<&str> = config.lines().collect();
        lines.windows(2).find_map(|pair| {
            let pattern: String = serde_json::from_str(pair[0].trim().strip_prefix("- match: ")?).ok()?;
            let approval = pair[1].trim().strip_prefix("approval: ")?;
            let pattern = normalize(&pattern);
            candidates.iter().any(|c| glob(&pattern, c)).then(|| approval.to_string())
        })
    }

    #[test]
    fn omp_config_gates_exactly_the_user_only_subcommands() {
        // The coordinator calls the absolute binary, and the default root has the name in it too.
        let hp = "/home/u/.local/bin/herdr-projects --root /home/u/.herdr-projects";
        let config = omp_config(hp, "", Some(&[]));
        let deny = Some("deny".to_string());
        let prompt = Some("prompt".to_string());
        for (command, want) in [
            (format!("{hp} routine approve demo watch"), deny.clone()),
            (format!("{hp} configure --clients omp"), deny.clone()),
            (format!("{hp} configure"), deny.clone()),
            (format!("{hp} unconfigure"), deny.clone()),
            (format!("cd /tmp && {hp} unconfigure --clients omp"), deny.clone()),
            (format!("{hp} thread resolve demo t-0001"), prompt.clone()),
            (format!("{hp} sweep"), prompt.clone()),
            (format!("{hp} sweep demo --yes"), prompt.clone()),
            (format!("{hp} archive demo"), prompt.clone()),
            (format!("{hp}  delete demo --force"), prompt.clone()),
            (format!("{hp} unarchive demo"), None),
            (format!("{hp} context demo"), None),
            (format!("{hp} routine list demo"), None),
            (format!("{hp} thread show demo t-0001"), None),
            // Slugs that start with a gated word.
            (format!("{hp} context configure-ci"), None),
            (format!("{hp} inbox done archive-sync 20260917T000001Z-routine-r-1"), None),
            (format!("{hp} thread resolve-later"), None),
            // Briefs and follow-ups that mention the gated words.
            (
                format!("{hp} thread start demo --title \"delete stale flags\" --task-file - <<'TASK'\nconfigure the lint job\narchive old logs, then sweep and delete the rest\nthread resolve nothing\nunconfigure it\nroutine approve nothing\nTASK"),
                None,
            ),
            (format!("{hp} thread prompt demo t-0001 --text-file - <<'EOF'\nPlease configure CI and delete dead code.\nEOF"), None),
        ] {
            assert_eq!(omp_approval(&config, &command), want, "{command}");
        }
        // Another binary or root is not this coordinator's command.
        assert_eq!(omp_approval(&config, "/opt/herdr-projects --root /home/u/.herdr-projects configure"), None);
        for tool in ["eval: prompt", "debug: prompt"] {
            assert!(config.lines().any(|l| l.trim() == tool), "{tool}");
        }
    }

    #[test]
    fn front_matter_parsing() {
        let (settings, body) =
            parse_project_md("+++\nname = \"X\"\nnudge = true\n+++\n\nBody\n+++\nmore\n").unwrap();
        assert_eq!(settings.name, "X");
        assert!(settings.nudge);
        assert_eq!(settings.thread_agent, "claude");
        assert_eq!(body, "Body\n+++\nmore\n");
        assert!(parse_project_md("no front matter").is_err());
        assert!(parse_project_md("+++\nname = \n+++\n").is_err());
        assert!(parse_project_md("+++\nname = \"X\"\n").is_err());
        let (_, body) = parse_project_md("+++\nname = \"X\"\n+++").unwrap();
        assert_eq!(body, "");
    }

    #[test]
    fn repo_arg_parsing() {
        assert_eq!(parse_repo_arg("/a/b").machine, None);
        assert_eq!(parse_repo_arg("/a/b@m1").machine.as_deref(), Some("m1"));
        assert_eq!(parse_repo_arg("/a/b@m1").path, "/a/b");
        // An `@` inside a path is not a machine.
        assert_eq!(parse_repo_arg("/a@b/c").machine, None);
        assert_eq!(parse_repo_arg("/a@b/c").path, "/a@b/c");
    }

    #[test]
    fn safety_defaults_and_overrides_keyed_by_canonical_path() {
        let config = tempfile::tempdir().unwrap();
        let here = Path::new("/projects/demo");
        assert_eq!(load_safety(config.path(), here).unwrap(), Safety::default());

        std::fs::write(
            config.path().join("config.toml"),
            "root = \"/projects\"\n\n[safety.\"/projects/demo\"]\nstart_threads = \"auto\"\nthread_agent_args = [\"--x\"]\n",
        )
        .unwrap();
        let safety = load_safety(config.path(), here).unwrap();
        assert_eq!(safety.start_threads, "auto");
        assert_eq!(safety.thread_agent_args, ["--x"]);
        assert!(!safety.routine_commands);
        assert!(safety.coordinator_agent_args.is_empty());
        assert_eq!(
            load_safety(config.path(), Path::new("/projects/other")).unwrap(),
            Safety::default()
        );

        std::fs::write(
            config.path().join("config.toml"),
            "[safety.\"/projects/demo\"]\nstart_threads = \"yolo\"\n",
        )
        .unwrap();
        assert!(load_safety(config.path(), here).is_err());
    }

    #[test]
    fn writers_drop_their_write_when_project_md_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "demo", "", vec![]).unwrap();
        std::fs::remove_file(project.project_md()).unwrap();
        assert!(project.update_coordinator(|c| c.pane_id = "w1:p1".into()).is_err());
        assert!(project.coordinator().is_none());

        // A deleted folder is not recreated by taking the lock.
        std::fs::remove_dir_all(project.dir()).unwrap();
        assert!(project.lock().is_err());
        assert!(!project.dir().exists());
    }

    #[test]
    fn coordinator_updates_keep_other_fields() {
        let root = tempfile::tempdir().unwrap();
        let project = create(root.path(), "demo", "", vec![]).unwrap();
        project.update_coordinator(|c| c.socket = "/s".into()).unwrap();
        project.update_coordinator(|c| c.agent_session = "sess".into()).unwrap();
        let record = project.coordinator().unwrap();
        assert_eq!(record.socket, "/s");
        assert_eq!(record.agent_session, "sess");
        assert!(std::fs::read_dir(project.state_dir())
            .unwrap()
            .flatten()
            .all(|e| !e.file_name().to_string_lossy().ends_with(".tmp")));
    }
}
