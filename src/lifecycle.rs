//! pause, resume, archive, unarchive and delete.

use anyhow::{Context, Result, bail};

use crate::paths::Ctx;
use crate::project::{Project, Status};
use crate::thread;
use crate::threads::{self, SessionView};
use crate::coordinator;

/// (what, pane id) of every recorded pane that is alive in the project's session.
fn alive_panes(project: &Project, view: &SessionView) -> Vec<(String, String, String)> {
    let mut alive = Vec::new();
    if let Some(record) = project.coordinator() {
        let mut any = false;
        for agent in view.agents.iter().filter(|a| coordinator::is_coordinator(&record, a)) {
            any = true;
            alive.push(("coordinator".to_string(), agent.pane_id.clone(), agent.agent_status.clone()));
        }
        if !any && view.panes.iter().any(|p| coordinator::pane_matches(&record, p)) {
            alive.push(("coordinator".to_string(), record.pane_id.clone(), String::new()));
        }
    }
    let now = jiff::Timestamp::now();
    for t in thread::list(project) {
        if t.status == thread::Status::Resolved || t.is_remote() {
            continue;
        }
        let live = thread::live_state(&t, &view.agents, &view.panes, now);
        if live.pane_exists {
            alive.push((t.id.clone(), t.pane_id.clone(), live.agent_state.unwrap_or_default()));
        }
    }
    alive
}

/// Closes the open workspaces of local threads, then the project's own.
/// Never `--group`: archive never closes a repository's primary workspace,
/// and Herdr's refusal (`workspace_group_close_required`) is reported.
fn close_workspaces(project: &Project, view: &SessionView) -> Vec<String> {
    let mut notes = Vec::new();
    let mut close = |workspace: &str, what: &str| match view.herdr.call(&["workspace", "close", workspace], crate::herdr::CALL_TIMEOUT) {
        Ok(_) => notes.push(format!("closed {what} (workspace {workspace})")),
        Err(error) if error.code == "workspace_group_close_required" => notes.push(format!("left {what} open: it is a repository's primary workspace")),
        Err(error) => notes.push(format!("could not close {what}: {error}")),
    };
    let mut done = Vec::new();
    for t in thread::list(project).iter().filter(|t| t.status != thread::Status::Resolved && !t.is_remote() && t.kind == thread::Kind::Worktree) {
        let workspace = view.panes.iter().find(|p| !t.worktree_path.is_empty() && std::path::Path::new(&p.cwd).starts_with(&t.worktree_path)).map(|p| p.workspace_id.clone());
        if let Some(workspace) = workspace
            && !done.contains(&workspace)
        {
            close(&workspace, &format!("{}'s workspace", t.id));
            done.push(workspace);
        }
    }
    if let Some(record) = project.coordinator()
        && coordinator::workspace_open(&record, &view.panes)
    {
        crate::sidebar::clear_workspace(&view.herdr, &record.workspace_id);
        close(&record.workspace_id, "the project workspace");
    }
    notes
}

pub fn set_status(ctx: &Ctx, slug: &str, status: Status) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    let current = project.status();
    match (current, status) {
        (Status::Archived, Status::Paused) => bail!("`{slug}` is archived; `unarchive` it first"),
        (Status::Archived, Status::Active) | (_, Status::Archived) | (_, Status::Paused) | (Status::Paused, Status::Active) | (Status::Active, Status::Active) => {}
    }
    project.set_status(status)?;
    println!("`{slug}` is now {status}");

    let view = threads::session_view(ctx, &project);
    match status {
        Status::Paused => {
            println!("The ticker skips it and `thread start` is refused. Running agents are not interrupted.");
            if let Some(view) = &view {
                for (what, pane, _) in alive_panes(&project, view).into_iter().filter(|(_, _, s)| s == "working") {
                    println!("  still working: {what} (pane {pane})");
                }
            }
        }
        Status::Archived => {
            println!("It is hidden from `list` and the popup, the ticker skips it, and `open` is refused until `unarchive`. Its folder and every unresolved thread's worktree stay.");
            if let Some(view) = &view {
                for (_, pane, _) in alive_panes(&project, view) {
                    crate::sidebar::clear_pane(&view.herdr, &pane);
                }
                for note in close_workspaces(&project, view) {
                    println!("  {note}");
                }
            }
        }
        Status::Active if current == Status::Archived => {
            // Unarchive reopens it: the workspace and a coordinator.
            let options = crate::coordinator::OpenOptions { session: crate::paths::SessionFlags::default(), rebind: false, profile: None, new: false, here: false };
            if let Err(error) = crate::coordinator::open(ctx, slug, &options) {
                println!("reopen it with `open {slug}` ({error:#})");
            }
        }
        Status::Active => {}
    }
    Ok(())
}

/// Moves the project folder to `<root>/.trash/<slug>-<timestamp>/`. Touches no
/// worktree, branch or pull request.
pub fn delete(ctx: &Ctx, slug: &str, force: bool) -> Result<()> {
    let project = Project::load(&ctx.root, slug)?;
    if !force {
        if let Some(view) = threads::session_view(ctx, &project) {
            let alive = alive_panes(&project, &view);
            if !alive.is_empty() {
                let list: Vec<String> = alive.iter().map(|(what, pane, _)| format!("{what} (pane {pane})")).collect();
                bail!("`{slug}` still has live panes: {}. Close them, or pass --force.", list.join(", "));
            }
        }
    }
    // Windows cannot move a folder that a running process has as its current
    // directory, and the project's shells and agents do: --force closes them.
    #[cfg(windows)]
    if force && let Some(view) = threads::session_view(ctx, &project) {
        for (what, pane, _) in alive_panes(&project, &view) {
            match view.herdr.pane_close(&pane) {
                Ok(()) => println!("closed {what} (pane {pane})"),
                Err(error) => println!("could not close {what} (pane {pane}): {error}"),
            }
        }
    }
    let threads = thread::list(&project);
    let canonical = project.canonical_dir();

    let trash = ctx.root.join(".trash");
    std::fs::create_dir_all(&trash)?;
    let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string();
    let target = trash.join(format!("{slug}-{stamp}"));
    {
        // Held while the folder moves, so no writer lands in between; writers
        // re-check PROJECT.md after taking the lock and drop their write.
        let _lock = project.lock()?;
        // Windows cannot rename a folder while a file in it is open, the lock
        // file included: it is let go first. A writer that opens a file in
        // between makes the move fail (retry), never loses its write.
        #[cfg(windows)]
        drop(_lock);
        move_to_trash(ctx, &project, &target)?;
    }
    println!("moved `{slug}` to {}", target.display());

    let left: Vec<&thread::Thread> = threads.iter().filter(|t| !t.worktree_path.is_empty() || !t.branch.is_empty()).collect();
    if !left.is_empty() {
        println!("Left alone (remove them yourself if you no longer want them):");
        for t in left {
            let place = if t.machine.is_empty() { String::new() } else { format!(" on {}", t.machine) };
            println!("  {}: worktree {}{place}, branch {} in {}", t.id, if t.worktree_path.is_empty() { "-" } else { &t.worktree_path }, if t.branch.is_empty() { "-" } else { &t.branch }, t.repo);
        }
    }
    println!(
        "The `[safety.\"{}\"]` table and any routine approvals for this path remain in {} and would apply to a new project at the same path.",
        canonical.display(),
        ctx.config_dir.display()
    );
    Ok(())
}

#[cfg(unix)]
fn move_to_trash(_ctx: &Ctx, project: &Project, target: &std::path::Path) -> Result<()> {
    std::fs::rename(project.dir(), target).with_context(|| format!("could not move {} to the trash", project.dir().display()))
}

/// Windows refuses the move while any process has a file or its current
/// directory in the folder (ERROR_SHARING_VIOLATION or ERROR_ACCESS_DENIED):
/// a ticker write or a pane that is closing lets go within moments, so the
/// move is retried for about two seconds.
#[cfg(windows)]
fn move_to_trash(ctx: &Ctx, project: &Project, target: &std::path::Path) -> Result<()> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        match std::fs::rename(project.dir(), target) {
            Ok(()) => return Ok(()),
            Err(e) if matches!(e.raw_os_error(), Some(32 | 5)) && std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(100)),
            Err(e) if matches!(e.raw_os_error(), Some(32 | 5)) => {
                let panes: Vec<String> = threads::session_view(ctx, project).map(|view| alive_panes(project, &view).into_iter().map(|(_, pane, _)| pane).collect()).unwrap_or_default();
                let close = if panes.is_empty() { "close any shell, editor or agent working in it".to_string() } else { format!("close the project's panes (`herdr pane close {}`)", panes.join(" ")) };
                return Err(e).with_context(|| format!("could not move {} to the trash: a process has it open; {close} and retry", project.dir().display()));
            }
            Err(e) => return Err(e).with_context(|| format!("could not move {} to the trash", project.dir().display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenarios::World;

    #[test]
    fn delete_refuses_while_a_pane_is_alive_and_force_moves_the_folder() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        world.thread(&project, world.home.path(), |t| t.branch = "hp/demo/t-0001-x".into());
        *world.panes.borrow_mut() = format!("[{}]", world.coordinator_pane(&project));
        world.runner.on("pane close", crate::runner::fake::ok(r#"{"result":{}}"#));
        let ctx = world.ctx();

        let error = delete(&ctx, "demo", false).unwrap_err().to_string();
        assert!(error.contains("coordinator (pane w1:p1)"), "{error}");
        assert!(project.dir().is_dir());

        delete(&ctx, "demo", true).unwrap();
        assert!(!project.dir().exists());
        let trashed: Vec<_> = std::fs::read_dir(world.root.join(".trash")).unwrap().flatten().collect();
        assert_eq!(trashed.len(), 1);
        assert!(trashed[0].file_name().to_string_lossy().starts_with("demo-"));
        assert!(trashed[0].path().join("PROJECT.md").is_file());
        assert!(trashed[0].path().join("threads/t-0001.toml").is_file());
        // Windows closes the live panes first: a pane's shell keeps the folder in use.
        assert_eq!(world.runner.count("pane close w1:p1"), usize::from(cfg!(windows)));
        // Nothing else but herdr list calls ran: no worktree, branch or PR was touched.
        assert!(world.runner.calls.borrow().iter().all(|c| c.display().contains(" list") || c.display().contains("pane close")));
        // `.trash` is not a project.
        assert!(crate::project::list_slugs(&world.root).is_empty());
    }

    #[test]
    fn delete_without_live_panes_needs_no_force() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        delete(&world.ctx(), "demo", false).unwrap();
        assert!(!project.dir().exists());
    }

    /// A process whose current directory is in the project folder blocks the
    /// move on Windows: one that lets go within the retry window does not
    /// fail the delete; one that stays gets an actionable error.
    #[cfg(windows)]
    #[test]
    fn windows_delete_waits_briefly_for_a_process_in_the_folder_then_explains() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        // One native process (no child of its own) with its directory in the folder.
        let hold = |secs: u32| std::process::Command::new("ping").args(["-n", &(secs + 1).to_string(), "127.0.0.1"]).stdout(std::process::Stdio::null()).current_dir(project.dir()).spawn().unwrap();

        let mut long = hold(8);
        std::thread::sleep(std::time::Duration::from_millis(300));
        let start = std::time::Instant::now();
        let error = format!("{:#}", delete(&world.ctx(), "demo", false).unwrap_err());
        assert!(start.elapsed() >= std::time::Duration::from_secs(2), "no retry: {error}");
        assert!(error.contains("a process has it open") && error.contains("retry"), "{error}");
        assert!(project.dir().join("PROJECT.md").is_file());
        let _ = long.kill();
        let _ = long.wait();

        let mut short = hold(1);
        std::thread::sleep(std::time::Duration::from_millis(300));
        delete(&world.ctx(), "demo", false).unwrap();
        assert!(!project.dir().exists());
        let _ = short.wait();
    }

    #[test]
    fn archive_clears_tokens_and_blocks_pause() {
        let world = World::new();
        let project = world.project("demo", "a.sock");
        *world.panes.borrow_mut() = format!("[{}]", world.coordinator_pane(&project));
        let ctx = world.ctx();
        set_status(&ctx, "demo", Status::Archived).unwrap();
        assert_eq!(project.status(), Status::Archived);
        let calls = world.runner.calls.borrow();
        let clear = calls.iter().find(|c| c.display().contains("--clear-token")).expect("tokens cleared");
        assert!(clear.display().contains("w1:p1"));
        drop(calls);
        assert!(set_status(&ctx, "demo", Status::Paused).is_err());
        set_status(&ctx, "demo", Status::Active).unwrap();
        assert_eq!(project.status(), Status::Active);
    }
}
