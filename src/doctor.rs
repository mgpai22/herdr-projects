//! `doctor`: what is installed, where things resolve, and whether it fits.

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;

use crate::herdr::{self, Herdr};
use crate::paths::{self, Ctx, Env, SessionFlags};
use crate::project;
use crate::runner::{Cmd, Runner};

const TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// Prints the report and returns whether every required check passed. With
/// `fix`, repairs what the binary owns: priming files, `uploads/`, the
/// absolute binary path they carry, the OMP extension, and the skill link for a configured harness. Never edits another plugin's entries.
pub fn run(ctx: &Ctx, session: &SessionFlags, fix: bool) -> Result<bool> {
    let skill = crate::setup::skill_source();
    let (text, healthy) = report(ctx.env, &ctx.root, &ctx.config_dir, session, ctx.runner, fix, skill.as_deref());
    print!("{text}");
    Ok(healthy)
}

fn report(
    env: &Env,
    root: &Path,
    config_dir: &Path,
    session: &SessionFlags,
    runner: &dyn Runner,
    fix: bool,
    skill: Option<&Path>,
) -> (String, bool) {
    let mut out = String::new();
    let mut healthy = true;
    let mut check = |out: &mut String, ok: Option<bool>, label: &str, detail: String| {
        let mark = match ok {
            Some(true) => "ok  ",
            Some(false) => {
                healthy = false;
                "FAIL"
            }
            None => "warn",
        };
        let _ = writeln!(out, "[{mark}] {label}: {detail}");
    };

    let binary = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| format!("unknown ({e})"));
    let _ = writeln!(out, "binary:     {binary}");
    let _ = writeln!(out, "version:    {}", crate::VERSION);
    let _ = writeln!(out, "root:       {}", root.display());
    let _ = writeln!(out, "config dir: {}", config_dir.display());
    let _ = writeln!(out);

    if let Some(latest) = crate::update::newer_release(runner, crate::update::own_root().as_deref()) {
        check(&mut out, None, "update", format!("a newer version is available ({latest}): run `herdr-projects update`"));
    }

    let bin = env.herdr_bin();
    match herdr::version(&bin, runner) {
        Ok(version) if version >= herdr::MIN_VERSION => {
            check(&mut out, Some(true), "herdr", format!("{version} ({bin})"))
        }
        Ok(version) => check(
            &mut out,
            Some(false),
            "herdr",
            format!("{version} ({bin}); {} or later is required", herdr::MIN_VERSION),
        ),
        Err(error) => check(&mut out, Some(false), "herdr", format!("{error:#}")),
    }

    match paths::resolve_session(session, env, runner) {
        Ok(found) => {
            let reachable = Herdr::new(&bin, &found.socket, runner).reachable();
            let name = found.name.as_deref().unwrap_or("-");
            check(
                &mut out,
                if reachable { Some(true) } else { None },
                "session",
                format!(
                    "{} (name: {name}){}",
                    found.socket.display(),
                    if reachable { "" } else { "; not reachable" }
                ),
            );
        }
        Err(error) => check(&mut out, Some(false), "session", format!("{error:#}")),
    }

    for (tool, args, required) in [
        ("git", vec!["--version"], true),
        ("ssh", vec!["-V"], true),
        ("rsync", vec!["--version"], false),
        ("gh", vec!["--version"], false),
    ] {
        let result = runner.run(&Cmd::new(tool, TOOL_TIMEOUT).args(args));
        match result {
            Ok(o) if o.success() => {
                let text = if o.stdout.trim().is_empty() { &o.stderr } else { &o.stdout };
                let line = text.lines().next().unwrap_or("").trim().to_string();
                check(&mut out, Some(true), tool, line);
            }
            Ok(o) => check(&mut out, required.then_some(false), tool, o.error_text()),
            Err(error) => check(&mut out, required.then_some(false), tool, format!("{error:#}")),
        }
    }
    match runner.run(&Cmd::new("gh", TOOL_TIMEOUT).args(["auth", "status"])) {
        Ok(o) if o.success() => check(&mut out, Some(true), "gh auth", "logged in".into()),
        Ok(o) => check(
            &mut out,
            None,
            "gh auth",
            format!(
                "{}; pull request follow-up will not work",
                o.error_text().lines().next().unwrap_or("not logged in")
            ),
        ),
        Err(_) => check(&mut out, None, "gh auth", "gh is not installed".into()),
    }

    if root.is_dir() {
        let count = project::list_slugs(root).len();
        check(&mut out, Some(true), "root", format!("{count} project(s)"));
    } else {
        check(
            &mut out,
            None,
            "root",
            "does not exist yet; `new` creates it".into(),
        );
    }

    match crate::ticker::lock_state(root) {
        crate::ticker::LockState::Free => check(&mut out, None, "ticker", "not running".into()),
        crate::ticker::LockState::Held(info) => check(
            &mut out,
            Some(true),
            "ticker",
            format!(
                "running, version {} (this binary: {}), root {}",
                info.version,
                crate::VERSION,
                info.root
            ),
        ),
    }

    // Every project's priming files, and the binary path they carry: a
    // `plugin link` from another checkout or a moved plugin root breaks them
    // silently, and `--fix` rewrites them.
    let prefix = crate::coordinator::current_prefix(root).unwrap_or_default();
    let slugs = project::list_slugs(root);
    for slug in &slugs {
        let Ok(project) = project::Project::load(root, slug) else {
            continue;
        };
        let label = format!("files {slug}");
        let problems = project::priming_problems(&project, &prefix, env);
        if problems.is_empty() {
            check(&mut out, Some(true), &label, "AGENTS.md, CLAUDE.md link, .omp/config.yml and uploads/ are in place".into());
        } else {
            // Re-check after the write: `write_priming` leaves a file it
            // cannot read (or will not overwrite) alone, and ignores a failed
            // `.omp/config.yml` write, so only what is gone counts as fixed.
            let mut left = problems.clone();
            if fix {
                match project::write_priming(&project, &prefix, env) {
                    Ok(()) => {
                        left = project::priming_problems(&project, &prefix, env);
                        let fixed: Vec<&str> = problems.iter().filter(|p| !left.contains(p)).map(String::as_str).collect();
                        if !fixed.is_empty() {
                            check(&mut out, Some(true), &label, format!("fixed: {}", fixed.join("; ")));
                        }
                    }
                    Err(error) => {
                        check(&mut out, Some(false), &label, format!("could not fix ({error:#}): {}", problems.join("; ")));
                        left.clear();
                    }
                }
            }
            // The profile's config is the user's own OMP config: named with
            // its own hint, never "remove it".
            let (profile, left): (Vec<String>, Vec<String>) = left.into_iter().partition(|p| p.starts_with(project::PROFILE_PROBLEM));
            for problem in profile {
                check(&mut out, None, &label, problem);
            }
            let (kept, repairable): (Vec<String>, Vec<String>) = left.into_iter().partition(|p| p.contains("cannot be read"));
            if !kept.is_empty() {
                check(&mut out, None, &label, format!("{}; left alone; fix its permissions or remove it", kept.join("; ")));
            }
            if !repairable.is_empty() {
                let hint = if fix { "" } else { "; `doctor --fix` repairs this" };
                check(&mut out, None, &label, format!("{}{hint}", repairable.join("; ")));
            }
        }
        // An edited pull request routine is the user's: named, never rewritten.
        if let Some(note) = project::pr_followup_note(&project) {
            check(&mut out, None, &format!("routines {slug}"), note);
        }
        for other in &slugs {
            if other > slug && crate::names::collide(slug, other) {
                check(&mut out, Some(false), &format!("names {slug}"), format!("its agent names collide with `{other}` after truncation to 32 characters; rename one project"));
            }
        }
    }

    // Orphans, as `sweep --dry-run` would list them.
    {
        let ctx = Ctx { env, root: root.to_path_buf(), config_dir: config_dir.to_path_buf(), runner, detached_ticker: false };
        for slug in &slugs {
            let Ok(project) = project::Project::load(root, slug) else {
                continue;
            };
            let orphans = crate::sweep::find(&ctx, &project);
            if !orphans.is_empty() {
                let list: Vec<String> = orphans.iter().map(|o| o.describe()).collect();
                check(&mut out, None, &format!("sweep {slug}"), format!("{}; `sweep {slug}` removes them", list.join("; ")));
            }
        }
    }

    for slug in &slugs {
        let Ok(project) = project::Project::load(root, slug) else {
            continue;
        };
        let label = format!("project {slug}");
        let none = format!("no coordinator running (start any agent in {}, or `open {slug}`)", project.dir().display());
        // With no usable record, the agents the ticker would discover: the
        // ones working in the project folder in this session.
        let (record, mut notes) = match project.coordinator() {
            Some(record) if Path::new(&record.socket).exists() => (Some(record), Vec::new()),
            Some(record) => (None, vec![format!("recorded socket {} no longer exists", record.socket)]),
            None => (None, Vec::new()),
        };
        let socket = match (&record, paths::resolve_session(session, env, runner)) {
            (Some(record), _) => record.socket.clone(),
            (None, Ok(found)) => found.socket.to_string_lossy().into_owned(),
            (None, Err(_)) => {
                notes.push(none);
                check(&mut out, Some(true), &label, format!("{}; {}", project.status(), notes.join("; ")));
                continue;
            }
        };
        let herdr = Herdr::new(&bin, &socket, runner);
        match (herdr.pane_list(), herdr.agent_list()) {
            (Ok(panes), Ok(agents)) => {
                let dir = project.canonical_dir().to_string_lossy().into_owned();
                let coordinators: Vec<String> = agents
                    .iter()
                    .filter(|a| a.works_in(&dir))
                    .map(|a| if a.name.starts_with("hpc-") { format!("{} ({})", a.pane_id, a.agent) } else { format!("{} ({}, started by hand)", a.pane_id, a.agent) })
                    .collect();
                if let Some(record) = &record {
                    let workspace = crate::coordinator::workspace_open(record, &panes);
                    notes.push(format!("socket {}; workspace {} {}", record.socket, record.workspace_id, if workspace { "open" } else { "closed" }));
                }
                if coordinators.is_empty() {
                    notes.push(none);
                } else {
                    notes.push(format!("coordinator: {}", coordinators.join(", ")));
                    if record.is_none() {
                        notes.push("the ticker records it on its next tick".into());
                    }
                }
                check(
                    &mut out,
                    if coordinators.is_empty() && record.is_some() { None } else { Some(true) },
                    &label,
                    format!("{}; {}", project.status(), notes.join("; ")),
                );
            }
            (Err(error), _) | (_, Err(error)) => check(&mut out, None, &label, format!("session at {socket} unreachable: {error}")),
        }
    }

    // Scheduled routines whose last due run did nothing: no coordinator ran.
    for slug in &slugs {
        let Ok(project) = project::Project::load(root, slug) else {
            continue;
        };
        let states = crate::steps::load_state(&project).routines;
        let now = jiff::Zoned::now();
        let skipped: Vec<String> = crate::routine::load_all(&project)
            .0
            .iter()
            .filter(|r| r.enabled && states.get(&r.name).is_some_and(|s| s.no_coordinator > 0))
            .map(|r| format!("{}: {}", r.name, crate::routine::when_text(r, states.get(&r.name), &now)))
            .collect();
        if !skipped.is_empty() {
            check(&mut out, None, &format!("routines {slug}"), format!("{}; routines run only while a coordinator runs", skipped.join("; ")));
        }
    }

    // Hooks: ours in place and pointing at this binary; the standalone
    // agent-progress plugin's hooks gone (never edited by this plugin).
    let journal = crate::setup::load_journal(config_dir);
    for agent in ["claude", "codex"] {
        let file = crate::setup::hook_file(env, agent, None, None);
        let Ok(Some(text)) = crate::setup::read(&file) else {
            continue;
        };
        let label = format!("hooks {agent}");
        if crate::setup::has_agent_progress_hooks(&text) {
            let launcher = text
                .split('"')
                .find(|s| s.contains("herdr-progress") && s.contains(" hook --agent "))
                .and_then(|s| s.split(" hook --agent ").next())
                .unwrap_or("herdr-progress")
                .to_string();
            check(&mut out, None, &label, format!("{} still runs the standalone agent-progress hooks; run `{launcher} unconfigure`, then `herdr plugin disable agent-progress`", file.display()));
        }
        let key = file.to_string_lossy().into_owned();
        let binary = crate::paths::binary().unwrap_or_default();
        let expected = crate::setup::hook_command(&binary, root, agent);
        match journal.get(&key) {
            None => check(&mut out, None, &label, "not configured; `configure` installs the progress hooks".into()),
            Some(_) if text.contains(&expected) => check(&mut out, Some(true), &label, format!("{} runs this binary", file.display())),
            Some(_) if fix => {
                let options = crate::setup::ConfigureOptions { clients: vec![agent.to_string()], claude_home: None, codex_home: None, dry_run: false, hooks: true, sidebar: false, key: None, herdr_config: None, skill: crate::setup::skill_source() };
                let ctx = Ctx { env, root: root.to_path_buf(), config_dir: config_dir.to_path_buf(), runner, detached_ticker: false };
                match crate::setup::configure(&ctx, &options) {
                    Ok(_) => check(&mut out, Some(true), &label, format!("fixed: {} now runs this binary", file.display())),
                    Err(error) => check(&mut out, Some(false), &label, format!("could not fix: {error:#}")),
                }
            }
            Some(_) => check(&mut out, None, &label, format!("{} runs another binary or root; `doctor --fix` rewrites it", file.display())),
        }
    }

    // The OMP extension in each profile, OMP's stand-in for the hooks. `--fix`
    // touches a profile's copy only after `configure` (journaled), and
    // rewrites an older copy of ours only when it serves this root: each file
    // serves one root, and doctor at another root (a dev root) must not move it.
    // No check for global `bash.patterns`: the project `.omp/config.yml`
    // carries the coordinator profile's rules, and `files` reports it out of date.
    let omp_profiles = crate::omp::profile_agent_dirs(env);
    let profile_name = |name: &str| if name.is_empty() { "default".to_string() } else { name.to_string() };
    for (name, dir) in &omp_profiles {
        use crate::setup::ExtensionState;
        let file = crate::setup::omp_extension_path(dir);
        let rendered = crate::setup::render_omp_extension(&crate::paths::binary().unwrap_or_default(), root);
        let journaled = journal.get(&*file.to_string_lossy()).is_some_and(|o| o.kind == "omp-extension");
        let label = &format!("omp extension {}", profile_name(name));
        let version = crate::setup::OMP_EXTENSION_VERSION;
        let move_it = "run `configure --clients omp` from this root to move it here";
        match crate::setup::omp_extension_state(&file, &rendered) {
            Ok(ExtensionState::Current) => check(&mut out, Some(true), label, format!("{} is v{version} and runs this binary", file.display())),
            Ok(state @ (ExtensionState::Missing | ExtensionState::Stale)) => {
                let installed_for = crate::setup::read(&file).ok().flatten().and_then(|t| crate::setup::omp_extension_root(&t));
                let this_root = installed_for.as_deref() == Some(&*root.to_string_lossy());
                if !journaled && state == ExtensionState::Missing {
                    check(&mut out, None, label, format!("{} is missing; `configure --clients omp` installs it", file.display()));
                } else if !journaled {
                    check(&mut out, None, label, format!("{} is left over from an earlier configure; delete it, or {move_it}", file.display()));
                } else if state == ExtensionState::Stale && !this_root {
                    let other = installed_for.unwrap_or_else(|| "an unknown root".into());
                    check(&mut out, None, label, format!("{} is installed for root {other}; {move_it}", file.display()));
                } else if fix {
                    let options = crate::setup::ConfigureOptions { clients: vec![format!("omp:{name}")], claude_home: None, codex_home: None, dry_run: false, hooks: true, sidebar: false, key: None, herdr_config: None, skill: None };
                    let ctx = Ctx { env, root: root.to_path_buf(), config_dir: config_dir.to_path_buf(), runner, detached_ticker: false };
                    match crate::setup::configure(&ctx, &options) {
                        Ok(_) => check(&mut out, Some(true), label, format!("fixed: {} is v{version} and runs this binary", file.display())),
                        Err(error) => check(&mut out, Some(false), label, format!("could not fix: {error:#}")),
                    }
                } else if state == ExtensionState::Missing {
                    check(&mut out, None, label, format!("{} is missing; `doctor --fix` installs it", file.display()));
                } else {
                    check(&mut out, None, label, format!("{} is outdated or runs another binary; `doctor --fix` rewrites it", file.display()));
                }
            }
            Ok(ExtensionState::Foreign) => check(&mut out, None, label, format!("{} is not this plugin's file, so the extension is not installed; move it away and run `configure --clients omp`", file.display())),
            Err(error) => check(&mut out, None, label, format!("{error:#}")),
        }
        // Information only: OMP threads route through mstack when it is enabled for their profile.
        let mstack = match crate::omp::mstack_version(env, name) {
            Some((major, minor, patch)) => format!("{major}.{minor}.{patch} enabled"),
            None if crate::omp::plugins_dir(env, name).join("node_modules").join(crate::omp::MSTACK).join("package.json").is_file() => "disabled".into(),
            None => "not installed".into(),
        };
        check(&mut out, Some(true), &format!("mstack {}", profile_name(name)), mstack);
    }

    // The bundled skill, linked where each installed harness (each OMP
    // profile) looks for skills. `--fix` links it only where the user already
    // ran `configure` (its hooks, extension or link are journaled), so
    // `update` alone brings a newly bundled skill to existing users without a new opt-in.
    if let Some(source) = skill.map(Path::to_path_buf).filter(|s| s.join("SKILL.md").is_file()) {
        // (label, link, what `configure` installed besides the skill and its kind, the `configure` client)
        let mut targets: Vec<(String, std::path::PathBuf, std::path::PathBuf, &str, String)> = ["claude", "codex"]
            .into_iter()
            .filter(|agent| crate::setup::installed(env, agent, None, None))
            .map(|agent| (format!("skill {agent}"), crate::setup::skill_link(env, agent, None), crate::setup::hook_file(env, agent, None, None), "hooks", agent.to_string()))
            .collect();
        targets.extend(omp_profiles.iter().map(|(name, dir)| (format!("skill omp {}", profile_name(name)), crate::setup::omp_skill_link(dir), crate::setup::omp_extension_path(dir), "omp-extension", format!("omp:{name}"))));
        for (label, link, setup_file, setup_kind, client) in targets {
            if client.starts_with("omp:") && crate::setup::omp_sees_shared_skill(env, &source) {
                check(&mut out, Some(true), &label, format!("OMP loads the bundled `{}` skill through ~/.agents/skills", crate::setup::SKILL));
                continue;
            }
            let journaled = journal.contains_key(&*link.to_string_lossy());
            let opted_in = journaled || journal.get(&*setup_file.to_string_lossy()).is_some_and(|o| o.kind == setup_kind);
            let state = crate::setup::skill_state(&link, &source);
            let repairable = matches!(state, crate::setup::SkillState::Missing) || matches!(state, crate::setup::SkillState::Elsewhere(_) if journaled);
            if fix && opted_in && repairable {
                let options = crate::setup::ConfigureOptions { clients: vec![client], claude_home: None, codex_home: None, dry_run: false, hooks: false, sidebar: false, key: None, herdr_config: None, skill: Some(source.clone()) };
                let ctx = Ctx { env, root: root.to_path_buf(), config_dir: config_dir.to_path_buf(), runner, detached_ticker: false };
                match crate::setup::configure(&ctx, &options) {
                    Ok(_) => check(&mut out, Some(true), &label, format!("fixed: {} now links the bundled `{}` skill", link.display(), crate::setup::SKILL)),
                    Err(error) => check(&mut out, Some(false), &label, format!("could not fix: {error:#}")),
                }
                continue;
            }
            let repair = if opted_in { "`doctor --fix`" } else { "`configure`" };
            match state {
                crate::setup::SkillState::Ours => check(&mut out, Some(true), &label, format!("{} links the bundled `{}` skill", link.display(), crate::setup::SKILL)),
                crate::setup::SkillState::Missing => check(&mut out, None, &label, format!("{} is missing; {repair} links the bundled skill", link.display())),
                crate::setup::SkillState::Elsewhere(old) if journaled => check(&mut out, None, &label, format!("{} links {}, another checkout; {repair} relinks it", link.display(), old.display())),
                _ => check(&mut out, None, &label, format!("{} is not this plugin's link, so the bundled skill is not installed; move it away and run {repair}", link.display())),
            }
        }
    }

    // Herdr's config.toml: rows, popup key and a tab-bar entry that runs this binary.
    {
        let file = crate::setup::herdr_config_path(env);
        let binary = crate::paths::binary().unwrap_or_default();
        let expected = crate::setup::tab_command(&binary, root);
        let text = crate::setup::read(&file).ok().flatten().unwrap_or_default();
        let key = file.to_string_lossy().into_owned();
        match journal.get(&key) {
            None => check(&mut out, None, "sidebar", "not configured; `configure` adds the sidebar rows, the popup key and the tab-bar count".into()),
            Some(_) if text.contains(&expected) => check(&mut out, Some(true), "sidebar", format!("{} has the rows, the popup key and the tab-bar entry", file.display())),
            Some(_) if fix => {
                let options = crate::setup::ConfigureOptions { clients: vec![], claude_home: None, codex_home: None, dry_run: false, hooks: true, sidebar: true, key: None, herdr_config: None, skill: crate::setup::skill_source() };
                let ctx = Ctx { env, root: root.to_path_buf(), config_dir: config_dir.to_path_buf(), runner, detached_ticker: false };
                let options = crate::setup::ConfigureOptions { clients: vec!["none".into()], ..options };
                match crate::setup::configure(&ctx, &options) {
                    Ok(_) => check(&mut out, Some(true), "sidebar", format!("fixed: {} now runs this binary in the tab bar", file.display())),
                    Err(error) => check(&mut out, Some(false), "sidebar", format!("could not fix: {error:#}")),
                }
            }
            Some(_) => check(&mut out, None, "sidebar", format!("{}'s tab-bar entry runs another binary or root; `doctor --fix` rewrites it", file.display())),
        }
    }

    // Machines that projects use need an SSH target for report and library copies.
    let mut machines = std::collections::BTreeSet::new();
    for slug in project::list_slugs(root) {
        let Ok(project) = project::Project::load(root, &slug) else {
            continue;
        };
        if let Ok((settings, _)) = project.read_project_md() {
            machines.extend(settings.repos.into_iter().filter_map(|r| r.machine));
        }
        machines.extend(crate::thread::list(&project).into_iter().filter(|t| t.is_remote() && t.status != crate::thread::Status::Resolved).map(|t| t.machine));
    }
    for machine in machines {
        match crate::remote::ssh_target(runner, &bin, config_dir, &machine) {
            Ok(target) => check(&mut out, Some(true), &format!("machine {machine}"), format!("ssh target {target}")),
            Err(error) => check(&mut out, Some(false), &format!("machine {machine}"), format!("{error:#}")),
        }
    }

    (out, healthy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::{FakeRunner, fail, ok};

    fn runner_with_herdr(version: &str) -> FakeRunner {
        let runner = FakeRunner::new();
        runner.on("herdr --version", ok(version));
        runner.on("session list --json", ok(r#"{"sessions":[]}"#));
        runner.on("git --version", ok("git version 2.50.0\n"));
        runner.on("ssh -V", ok(""));
        runner.on("rsync --version", ok("rsync 3\n"));
        runner.on("gh --version", ok("gh version 2\n"));
        runner.on("gh auth status", fail(1, "not logged in"));
        runner
    }

    #[test]
    fn old_herdr_fails_and_names_the_minimum() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.0\n");
        let (text, healthy) = report(
            &env,
            &home.path().join("root"),
            &home.path().join("cfg"),
            &SessionFlags::default(),
            &runner,
            false,
            None,
        );
        assert!(!healthy);
        assert!(text.contains("[FAIL] herdr: 0.9.0"), "{text}");
        assert!(text.contains("0.9.1 or later"));
    }

    #[test]
    fn new_herdr_passes_and_warnings_do_not_fail() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let (text, healthy) = report(
            &env,
            &root,
            &home.path().join("cfg"),
            &SessionFlags::default(),
            &runner,
            false,
            None,
        );
        assert!(healthy, "{text}");
        assert!(text.contains("[warn] gh auth"));
        assert!(text.contains("[warn] root"));
        assert!(text.contains(&format!("root:       {}", root.display())));
        assert!(!root.exists(), "doctor must not create the root");
    }

    #[test]
    fn missing_priming_files_are_reported_and_fixed() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        // A project made before AGENTS.md existed: no priming files, no uploads/.
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        std::fs::remove_dir(project.dir().join("uploads")).unwrap();
        let flags = SessionFlags::default();
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &flags, &runner, false, None);
        assert!(text.contains("[warn] files demo: AGENTS.md is missing; CLAUDE.md is not a link to AGENTS.md; uploads/ is missing; `doctor --fix` repairs this"), "{text}");
        assert!(!project.dir().join("AGENTS.md").exists());

        let (text, _) = report(&env, &root, &home.path().join("cfg"), &flags, &runner, true, None);
        assert!(text.contains("[ok  ] files demo: fixed: AGENTS.md is missing"), "{text}");
        assert!(project.dir().join("AGENTS.md").is_file());
        assert!(project.dir().join("uploads").is_dir());
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &flags, &runner, false, None);
        assert!(text.contains("[ok  ] files demo: AGENTS.md, CLAUDE.md link, .omp/config.yml and uploads/ are in place"), "{text}");
    }

    #[test]
    fn fix_reports_only_what_it_fixed_and_names_files_it_leaves_alone() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let cfg = home.path().join("cfg");
        let flags = SessionFlags::default();
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        let omp = project.dir().join(".omp");
        std::fs::create_dir_all(&omp).unwrap();

        // A file it cannot read is left alone, and `--fix` does not claim it.
        std::fs::write(project.dir().join(project::OMP_CONFIG), b"\xff\xfe").unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, false, None);
        assert!(text.contains("[warn] files demo: .omp/config.yml cannot be read (") && text.contains("); left alone; fix its permissions or remove it"), "{text}");
        assert!(text.contains("[warn] files demo: AGENTS.md is missing; CLAUDE.md is not a link to AGENTS.md; `doctor --fix` repairs this"), "{text}");
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, None);
        assert!(text.contains("[ok  ] files demo: fixed: AGENTS.md is missing; CLAUDE.md is not a link to AGENTS.md\n"), "{text}");
        assert!(text.contains("[warn] files demo: .omp/config.yml cannot be read (") && !text.contains("fixed: .omp"), "{text}");
        assert_eq!(std::fs::read(project.dir().join(project::OMP_CONFIG)).unwrap(), b"\xff\xfe");

        // A missing file it cannot write stays reported, with no `fixed`.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::remove_file(project.dir().join(project::OMP_CONFIG)).unwrap();
            std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o500)).unwrap();
            let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, None);
            std::fs::set_permissions(&omp, std::fs::Permissions::from_mode(0o700)).unwrap();
            assert!(text.contains("[warn] files demo: .omp/config.yml is missing\n") && !text.contains("fixed:"), "{text}");
        }
    }

    #[test]
    fn a_broken_profile_config_is_named_with_its_own_hint_not_the_left_alone_one() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let flags = SessionFlags::default();
        project::create(&root, "demo", "", vec![]).unwrap();
        // Unreadable as text: OMP's own config, which the "cannot be read" bucket would tell the user to remove.
        let config = crate::omp::agent_dir(&env, "").join("config.yml");
        std::fs::create_dir_all(config.parent().unwrap()).unwrap();
        std::fs::write(&config, b"\xff\xfe").unwrap();
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &flags, &runner, true, None);
        let line = text.lines().find(|l| l.starts_with(&format!("[warn] files demo: {}", project::PROFILE_PROBLEM))).unwrap_or_else(|| panic!("{text}"));
        assert!(line.ends_with(&format!("; fix {}, or run `set demo omp_profile <name>` to use another profile", config.display())), "{line}");
        assert!(!text.contains("fix its permissions or remove it"), "{text}");
    }

    #[test]
    fn routines_skipped_for_want_of_a_coordinator_are_named() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        std::fs::write(project.dir().join("routines/standup.md"), "+++\nschedule = \"every 5m\"\n+++\nGo.\n").unwrap();
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
        assert!(!text.contains("routines demo"), "{text}");
        let mut state = crate::steps::State::default();
        state.routines.insert("standup".into(), crate::routine::State { last_run: "2026-09-24T09:00:00Z".into(), no_coordinator: 2, ..Default::default() });
        crate::steps::save_state(&project, &state).unwrap();
        let (text, _) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
        assert!(text.contains("[warn] routines demo: standup: last "), "{text}");
        assert!(text.contains("skipped: no coordinator (2 run(s)); routines run only while a coordinator runs"), "{text}");
    }

    #[test]
    fn fix_links_the_skill_only_for_a_configured_harness_and_never_over_a_foreign_one() {
        let home = tempfile::tempdir().unwrap();
        let claude = home.path().join("claude-config");
        let env = Env::for_test(home.path(), &[("CLAUDE_CONFIG_DIR", claude.to_str().unwrap())]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let cfg = home.path().join("cfg");
        let flags = SessionFlags::default();
        std::fs::create_dir_all(&claude).unwrap();
        let source = home.path().join("plugin/skill/autoproject");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "---\nname: autoproject\n---\n").unwrap();
        let link = claude.join("skills").join(crate::setup::SKILL);

        // Never configured: `--fix` only points to `configure`.
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, Some(&source));
        assert!(text.contains("[warn] skill claude:") && text.contains("`configure` links the bundled skill"), "{text}");
        assert_eq!(crate::setup::skill_state(&link, &source), crate::setup::SkillState::Missing);

        // Configured before the skill shipped: hooks journaled, no link yet.
        let hooks = crate::setup::Owned { before: None, after: "{}".into(), kind: "hooks".into(), command: Some("x".into()) };
        crate::setup::save_journal(&cfg, &[(claude.join("settings.json").to_string_lossy().into_owned(), hooks)].into()).unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, false, Some(&source));
        assert!(text.contains("`doctor --fix` links the bundled skill"), "{text}");
        let (text, healthy) = report(&env, &root, &cfg, &flags, &runner, true, Some(&source));
        assert!(healthy && text.contains("[ok  ] skill claude: fixed:"), "{text}");
        assert_eq!(crate::setup::skill_state(&link, &source), crate::setup::SkillState::Ours);
        assert!(!claude.join("settings.json").exists(), "the hooks were touched");

        // A moved checkout: our journaled link is relinked.
        let moved = home.path().join("moved/skill/autoproject");
        std::fs::create_dir_all(&moved).unwrap();
        std::fs::write(moved.join("SKILL.md"), "x").unwrap();
        report(&env, &root, &cfg, &flags, &runner, true, Some(&moved));
        assert_eq!(crate::setup::skill_state(&link, &moved), crate::setup::SkillState::Ours);

        // A directory of the same name is never touched.
        crate::setup::remove_link(&link).unwrap();
        std::fs::create_dir(&link).unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, Some(&moved));
        assert!(text.contains("is not this plugin's link"), "{text}");
        assert_eq!(crate::setup::skill_state(&link, &moved), crate::setup::SkillState::Foreign);
    }

    #[test]
    fn fix_rewrites_our_outdated_omp_extension_only_when_journaled_for_this_root() {
        let home = tempfile::tempdir().unwrap();
        let agent = home.path().join("omp-agent");
        let neurable = home.path().join(".omp/profiles/neurable/agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::create_dir_all(&neurable).unwrap();
        let env = Env::for_test(home.path(), &[("PI_CODING_AGENT_DIR", agent.to_str().unwrap())]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let cfg = home.path().join("cfg");
        let flags = SessionFlags::default();
        let file = crate::setup::omp_extension_path(&agent);
        let binary = crate::paths::binary().unwrap();
        let rendered = crate::setup::render_omp_extension(&binary, &root);
        let version = crate::setup::OMP_EXTENSION_VERSION;
        let old = rendered.replacen(&format!("OMP_VERSION={version}\n"), "OMP_VERSION=0\n", 1);
        assert_ne!(old, rendered);

        // Never configured: `--fix` does not install it.
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, None);
        assert!(text.contains("[warn] omp extension default:") && text.contains("`configure --clients omp` installs it"), "{text}");
        assert!(!file.exists());

        // Ours, but no longer journaled (`unconfigure` left it): never rewritten.
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, &old).unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, None);
        assert!(text.contains("[warn] omp extension default:") && text.contains("left over"), "{text}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), old);

        // Journaled, an older copy for this root: reported, and `--fix` rewrites it.
        let owned = crate::setup::Owned { before: None, after: "x".into(), kind: "omp-extension".into(), command: None };
        crate::setup::save_journal(&cfg, &[(file.to_string_lossy().into_owned(), owned)].into()).unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, false, None);
        assert!(text.contains("[warn] omp extension default:") && text.contains("outdated"), "{text}");
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, None);
        assert!(text.contains("[ok  ] omp extension default: fixed:"), "{text}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), rendered);
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, false, None);
        assert!(text.contains("[ok  ] omp extension default:"), "{text}");
        // The fix is per profile: one never configured is not touched.
        assert!(text.contains("[warn] omp extension neurable:") && text.contains("`configure --clients omp` installs it"), "{text}");
        assert!(!crate::setup::omp_extension_path(&neurable).exists());

        // Journaled, but serving another root: `--fix` never moves it.
        let other = home.path().join("other-root");
        let theirs = crate::setup::render_omp_extension(&binary, &other);
        std::fs::write(&file, &theirs).unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, None);
        assert!(text.contains(&format!("installed for root {}", other.display())) && text.contains("`configure --clients omp` from this root"), "{text}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), theirs);

        // Journaled and deleted by the user: `--fix` puts it back.
        std::fs::remove_file(&file).unwrap();
        report(&env, &root, &cfg, &flags, &runner, true, None);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), rendered);

        // Someone else's file of that name is never touched.
        std::fs::write(&file, "// mine\n").unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, None);
        assert!(text.contains("is not this plugin's file"), "{text}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "// mine\n");
    }

    #[test]
    fn mstack_is_reported_per_profile_as_information_only() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        std::fs::create_dir_all(home.path().join(".omp/agent")).unwrap();
        std::fs::create_dir_all(home.path().join(".omp/profiles/neurable/agent")).unwrap();
        let plugins = crate::omp::plugins_dir(&env, "neurable");
        let package = plugins.join("node_modules").join(crate::omp::MSTACK);
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("package.json"), r#"{"version":"0.4.0"}"#).unwrap();
        let lock = |enabled: bool| std::fs::write(plugins.join("omp-plugins.lock.json"), format!(r#"{{"plugins":{{"{}":{{"enabled":{enabled}}}}}}}"#, crate::omp::MSTACK)).unwrap();
        let report_text = || report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
        lock(true);
        let (text, healthy) = report_text();
        assert!(healthy, "{text}");
        assert!(text.contains("[ok  ] mstack default: not installed\n") && text.contains("[ok  ] mstack neurable: 0.4.0 enabled\n"), "{text}");
        lock(false);
        let (text, healthy) = report_text();
        assert!(healthy && text.contains("[ok  ] mstack neurable: disabled\n"), "{text}");
    }

    #[test]
    fn the_omp_skill_counts_as_installed_through_agents_skills() {
        let home = tempfile::tempdir().unwrap();
        let agent = home.path().join("omp-agent");
        std::fs::create_dir_all(&agent).unwrap();
        let env = Env::for_test(home.path(), &[("PI_CODING_AGENT_DIR", agent.to_str().unwrap())]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let cfg = home.path().join("cfg");
        let flags = SessionFlags::default();
        let source = home.path().join("plugin/skill/autoproject");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("SKILL.md"), "---\nname: autoproject\n---\n").unwrap();
        let omp_link = crate::setup::omp_skill_link(&agent);
        // Opted in: the extension is journaled.
        let extension = crate::setup::Owned { before: None, after: "x".into(), kind: "omp-extension".into(), command: None };
        crate::setup::save_journal(&cfg, &[(crate::setup::omp_extension_path(&agent).to_string_lossy().into_owned(), extension)].into()).unwrap();

        std::fs::create_dir_all(home.path().join(".agents/skills")).unwrap();
        crate::setup::link_dir(&source, &home.path().join(".agents/skills").join(crate::setup::SKILL)).unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, Some(&source));
        assert!(text.contains("[ok  ] skill omp default: OMP loads the bundled"), "{text}");
        assert_eq!(crate::setup::skill_state(&omp_link, &source), crate::setup::SkillState::Missing);

        // Without the shared link, `--fix` links it into OMP's own skills dir.
        crate::setup::remove_link(&home.path().join(".agents/skills").join(crate::setup::SKILL)).unwrap();
        let (text, _) = report(&env, &root, &cfg, &flags, &runner, true, Some(&source));
        assert!(text.contains("[ok  ] skill omp default: fixed:"), "{text}");
        assert_eq!(crate::setup::skill_state(&omp_link, &source), crate::setup::SkillState::Ours);
    }

    #[test]
    fn colliding_agent_names_fail_the_report() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[]);
        let runner = runner_with_herdr("herdr 0.9.1\n");
        let root = home.path().join("root");
        let long = "x".repeat(30);
        for suffix in ["a", "b"] {
            let project = project::create(&root, &format!("{long}-{suffix}"), "", vec![]).unwrap();
            project::write_priming(&project, &crate::coordinator::current_prefix(&root).unwrap(), &env).unwrap();
        }
        let (text, healthy) = report(&env, &root, &home.path().join("cfg"), &SessionFlags::default(), &runner, false, None);
        assert!(!healthy);
        assert!(text.contains("[FAIL] names"), "{text}");
    }
}
