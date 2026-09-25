//! Messages for an agent pane (briefs, nudges, follow-ups). When the OMP
//! extension is live in the pane, the message is queued as a file it pulls
//! and hands to the agent without touching the input box; otherwise it is
//! typed through `herdr agent prompt`. The binary owns every channel file:
//! `<root>/.channel/<pane>-<socket hash>/<id>.json`, one message per file,
//! ids sorting by creation. An item being typed is renamed to
//! `.<id>.typing` first, out of `pull`'s sight.

#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::herdr::{Agent, Herdr, HerdrError, Pane};
use crate::paths::Ctx;
use crate::project::{self, Project};
use crate::{progress, thread};

/// A queued message the extension has not taken for this long, while its
/// heartbeat has stopped, is typed instead.
pub const FALLBACK_MS: i64 = 60_000;

/// How a message went out. Both count as delivered for the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sent {
    Keystroke,
    Queued,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub text: String,
    pub socket: String,
    pub pane_id: String,
    /// The pane's terminal when the item was queued, "" when unknown: a later
    /// terminal reusing the pane id never gets it typed.
    #[serde(default)]
    pub terminal_id: String,
    /// Unix milliseconds.
    pub created_at: i64,
}

fn dir(root: &Path) -> PathBuf {
    root.join(".channel")
}

fn pane_dir(root: &Path, socket: &str, pane_id: &str) -> PathBuf {
    dir(root).join(progress::record_stem(socket, pane_id))
}

fn item_path(root: &Path, item: &Item) -> PathBuf {
    pane_dir(root, &item.socket, &item.pane_id).join(format!("{}.json", item.id))
}

/// Ids are `<13-digit unix ms>-<8 hex>`; nothing else names a file, so an id
/// from the command line can never leave the pane's directory.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c) || c == '-')
}

/// Whether a message to this pane is queued for the extension. A remote
/// pane's extension runs against the remote machine's root, so it never sees
/// this root's files. Callers decide once and pass the answer to `send`, so a
/// blocked agent admitted because it is routed is never typed at.
pub fn routed(root: &Path, socket: &str, pane: &str, remote: bool) -> bool {
    !remote && progress::channel_fresh(root, socket, pane, progress::now())
}

pub fn send(root: &Path, herdr: &Herdr, socket: &str, pane: &str, routed: bool, kind: &str, text: &str) -> Result<Sent, HerdrError> {
    if routed {
        return queue(root, socket, pane, kind, text);
    }
    // Older queued items go first, so a keystroke never overtakes them. A
    // forwarded call reaches another server's pane that only shares the id.
    if !herdr.remote() {
        let record = progress::load(root, socket, pane);
        for item in pending(root, socket, pane) {
            if record.as_ref().is_some_and(|r| (!r.agent.is_empty() && r.agent != "omp") || differs(&item.terminal_id, &r.terminal_id)) {
                remove(root, &item);
                continue;
            }
            match type_item(root, herdr, &item, progress::now()) {
                Typed::Done => {}
                // The extension is back: the new text waits behind the rest.
                Typed::Fresh => return queue(root, socket, pane, kind, text),
                Typed::Refused(error) => return Err(error),
            }
        }
    }
    herdr.agent_prompt(pane, text).map(|()| Sent::Keystroke)
}

fn queue(root: &Path, socket: &str, pane: &str, kind: &str, text: &str) -> Result<Sent, HerdrError> {
    enqueue(root, socket, pane, kind, text, jiff::Timestamp::now().as_millisecond())
        .map(|_| Sent::Queued)
        .map_err(|e| HerdrError { code: "failed".into(), message: format!("could not queue the message for the OMP extension: {e:#}") })
}

fn enqueue(root: &Path, socket: &str, pane: &str, kind: &str, text: &str, now_ms: i64) -> Result<Item> {
    // The process id keeps two processes apart within one millisecond, the
    // counter two messages of one process, in order.
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let suffix = ((std::process::id() & 0xffff) << 16) | (COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff);
    let terminal_id = progress::load(root, socket, pane).map(|r| r.terminal_id).unwrap_or_default();
    let item = Item { id: format!("{now_ms:013}-{suffix:08x}"), kind: kind.into(), text: text.into(), socket: socket.into(), pane_id: pane.into(), terminal_id, created_at: now_ms };
    // The text is typed into an agent: only this user may read or add items.
    // On Windows the folder inherits the user profile's ACL instead.
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    builder.mode(0o700);
    builder.create(pane_dir(root, socket, pane))?;
    #[cfg(unix)]
    std::fs::set_permissions(dir(root), std::fs::Permissions::from_mode(0o700))?;
    crate::project::write_json(&item_path(root, &item), &item)?;
    Ok(item)
}

/// The pane's queued items, oldest first. An item whose stored socket and
/// pane differ from its directory's is never handed out.
pub fn pending(root: &Path, socket: &str, pane_id: &str) -> Vec<Item> {
    let mut items = items_in(&pane_dir(root, socket, pane_id));
    items.retain(|i| i.socket == socket && i.pane_id == pane_id);
    items
}

/// `<id>.json` files only: `valid_id` has no dot, so claimed `.<id>.typing`
/// items and `write_json`'s dot-named temp files are skipped.
fn items_in(dir: &Path) -> Vec<Item> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut items: Vec<Item> = entries
        .flatten()
        .filter(|e| e.file_name().to_str().and_then(|n| n.strip_suffix(".json")).is_some_and(valid_id))
        .filter_map(|e| crate::project::read_json::<Item>(&e.path()))
        .filter(|i| valid_id(&i.id))
        .collect();
    items.sort_by(|a, b| a.id.cmp(&b.id));
    items
}

fn remove(root: &Path, item: &Item) {
    let _ = std::fs::remove_file(item_path(root, item));
}

/// Both terminal ids known and different: the pane id was reused.
fn differs(queued: &str, live: &str) -> bool {
    !queued.is_empty() && !live.is_empty() && queued != live
}

enum Typed {
    /// Typed, or gone for good (acked meanwhile, or no agent to take it).
    Done,
    /// The extension pulls again; the item is back in the queue.
    Fresh,
    /// herdr refused; the item is back in the queue.
    Refused(HerdrError),
}

/// Types one queued item unless the extension takes it. The item is renamed
/// out of `pull`'s sight before the heartbeat is checked again: `pull`
/// touches before it lists, so a pull that saw the file reads as fresh here.
fn type_item(root: &Path, herdr: &Herdr, item: &Item, now: i64) -> Typed {
    let file = item_path(root, item);
    let claimed = claimed_path(root, item);
    // The claim's age is what `fallback` reads to put back one whose typist
    // died; stamped first, so the claim is never born looking old.
    let _ = std::fs::File::options().write(true).open(&file).and_then(|f| f.set_modified(std::time::SystemTime::now()));
    if std::fs::rename(&file, &claimed).is_err() {
        return Typed::Done;
    }
    if progress::channel_fresh(root, &item.socket, &item.pane_id, now) {
        let _ = std::fs::rename(&claimed, &file);
        return Typed::Fresh;
    }
    match herdr.agent_prompt(&item.pane_id, &item.text) {
        Ok(()) => {}
        // herdr answers `agent_not_found` for a pane that is gone or runs no agent.
        Err(error) if matches!(error.code.as_str(), "agent_not_found" | "pane_not_found") => {}
        Err(error) => {
            let _ = std::fs::rename(&claimed, &file);
            return Typed::Refused(error);
        }
    }
    let _ = std::fs::remove_file(&claimed);
    Typed::Done
}

fn claimed_path(root: &Path, item: &Item) -> PathBuf {
    pane_dir(root, &item.socket, &item.pane_id).join(format!(".{}.typing", item.id))
}

/// Claims older than `FALLBACK_MS` whose typist was killed before it typed
/// or put them back: back in the queue, or nothing would ever deliver them.
fn release_stale_claims(dir: &Path, now_ms: i64) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let Some(id) = entry.file_name().to_str().and_then(|n| n.strip_prefix('.')?.strip_suffix(".typing")).filter(|id| valid_id(id)).map(str::to_string) else {
            continue;
        };
        let claimed_ms = entry.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_millis() as i64);
        if now_ms - claimed_ms >= FALLBACK_MS {
            let _ = std::fs::rename(entry.path(), dir.join(format!("{id}.json")));
        }
    }
}

/// Whether herdr-projects sends anything to this pane: an open local thread
/// of a project runs in it, or it works in a project folder (a coordinator)
/// or below one. Only such panes get a heartbeat, so the user's other OMP
/// panes never become routed. File reads only: this runs every two seconds.
fn claimed(root: &Path, pane: &progress::Current) -> bool {
    let here = std::env::current_dir().ok();
    let mut cwds: Vec<&Path> = [&pane.foreground_cwd, &pane.cwd].into_iter().filter(|c| !c.is_empty()).map(Path::new).collect();
    if cwds.is_empty() {
        // `channel pull` inherits the OMP process's directory.
        cwds.extend(here.as_deref());
    }
    project::list_slugs(root).iter().filter_map(|slug| Project::load(root, slug).ok()).any(|p| {
        let folder = p.canonical_dir();
        cwds.iter().any(|c| c.starts_with(&folder))
            || (p.coordinator().is_some_and(|c| c.socket == pane.socket) && thread::list(&p).iter().any(|t| t.status != thread::Status::Resolved && !t.is_remote() && t.pane_id == pane.pane_id))
    })
}

/// Whether the calling pane is claimed, and its queued items. Only a claimed
/// pane gets a heartbeat, so only it is handed items: an unclaimed pull
/// would race the fallback, which sees no heartbeat. Items queued for an
/// earlier terminal with this pane id are dropped. A pane herdr cannot
/// resolve fails, so the extension keeps what it delivered and its cadence.
fn pulled(ctx: &Ctx) -> Result<(bool, Vec<Item>)> {
    let Some(pane) = progress::current(ctx.env, ctx.runner) else {
        if ctx.env.var("HERDR_ENV") == Some("1") && ctx.env.var("HERDR_PANE_ID").is_some() {
            bail!("herdr did not resolve this pane; try again");
        }
        return Ok((false, Vec::new()));
    };
    if !claimed(&ctx.root, &pane) {
        return Ok((false, Vec::new()));
    }
    progress::touch_channel(&ctx.root, &pane.socket, &pane.pane_id, &pane.terminal_id, &pane.agent, progress::now())?;
    let mut items = pending(&ctx.root, &pane.socket, &pane.pane_id);
    items.retain(|item| {
        let stale = differs(&item.terminal_id, &pane.terminal_id);
        if stale {
            remove(&ctx.root, item);
        }
        !stale
    });
    Ok((true, items))
}

/// `channel pull --agent omp`, run by the extension in its pane: records the
/// heartbeat and prints `{"claimed": bool, "items": [{id, kind, text}]}`.
pub fn pull(ctx: &Ctx) -> Result<()> {
    let (claimed, items) = pulled(ctx)?;
    let items: Vec<serde_json::Value> = items.into_iter().map(|i| serde_json::json!({ "id": i.id, "kind": i.kind, "text": i.text })).collect();
    println!("{}", serde_json::json!({ "claimed": claimed, "items": items }));
    Ok(())
}

/// `channel ack <id>...`: the extension handed these items to the agent. An
/// id that is already gone (the fallback typed it) is not an error; a pane
/// herdr cannot resolve is, so the extension retries instead of pulling the
/// same items again.
pub fn ack(ctx: &Ctx, ids: &[String]) -> Result<()> {
    if let Some(bad) = ids.iter().find(|id| !valid_id(id)) {
        bail!("`{bad}` is not a channel item id");
    }
    let Some(pane) = progress::current(ctx.env, ctx.runner) else {
        if ctx.env.var("HERDR_ENV") == Some("1") && ctx.env.var("HERDR_PANE_ID").is_some() {
            bail!("herdr did not resolve this pane; the items stay queued");
        }
        return Ok(());
    };
    let dir = pane_dir(&ctx.root, &pane.socket, &pane.pane_id);
    for id in ids {
        let _ = std::fs::remove_file(dir.join(format!("{id}.json")));
        // Claimed by a typist that will find the channel fresh and put it back.
        let _ = std::fs::remove_file(dir.join(format!(".{id}.typing")));
    }
    Ok(())
}

/// Items of this socket that sat for a minute while their pane's extension
/// stopped pulling are typed instead, oldest first, once each. A pane that is
/// gone loses its queue, and an item is dropped rather than typed into
/// another harness or a later terminal with the same pane id. A refused
/// prompt keeps the item (and those behind it) for the next tick.
pub fn fallback(root: &Path, herdr: &Herdr, socket: &str, panes: &[Pane], agents: &[Agent], now_ms: i64) {
    let Ok(entries) = std::fs::read_dir(dir(root)) else {
        return;
    };
    // Every stem of this socket ends in its hash, which is the stem of an empty pane id.
    let suffix = progress::record_stem(socket, "");
    for entry in entries.flatten() {
        let Some(stem) = entry.file_name().to_str().map(str::to_string) else {
            continue;
        };
        if !stem.ends_with(&suffix) {
            continue;
        }
        let Some(pane) = panes.iter().find(|p| progress::record_stem(socket, &p.pane_id) == stem) else {
            let _ = std::fs::remove_dir_all(entry.path());
            continue;
        };
        let agent = agents.iter().find(|a| a.pane_id == pane.pane_id);
        release_stale_claims(&entry.path(), now_ms);
        for item in pending(root, socket, &pane.pane_id) {
            if agent.is_none_or(|a| a.agent != "omp") || differs(&item.terminal_id, &pane.terminal_id) {
                remove(root, &item);
                continue;
            }
            if now_ms - item.created_at < FALLBACK_MS || progress::channel_fresh(root, socket, &pane.pane_id, now_ms / 1000) {
                break;
            }
            match type_item(root, herdr, &item, now_ms / 1000) {
                Typed::Done => {}
                Typed::Fresh | Typed::Refused(_) => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::Env;
    use crate::runner::fake::{FakeRunner, fail, ok};

    const SOCKET: &str = "/tmp/a.sock";

    #[test]
    fn items_come_out_oldest_first_and_only_for_their_own_pane() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let second = enqueue(root, SOCKET, "w1:p1", "follow-up", "second", 2_000_000_000_000).unwrap();
        let first = enqueue(root, SOCKET, "w1:p1", "brief", "first", 1_000_000_000_000).unwrap();
        enqueue(root, SOCKET, "w1:p2", "brief", "other pane", 1_000_000_000_000).unwrap();
        enqueue(root, "/tmp/b.sock", "w1:p1", "brief", "other session", 1_000_000_000_000).unwrap();
        // A file planted in the pane's directory for another pane is ignored.
        let planted = Item { id: "1500000000000-00000001".into(), socket: SOCKET.into(), pane_id: "w9:p9".into(), ..first.clone() };
        crate::project::write_json(&pane_dir(root, SOCKET, "w1:p1").join(format!("{}.json", planted.id)), &planted).unwrap();
        // An item being typed is out of sight.
        let typing = enqueue(root, SOCKET, "w1:p1", "brief", "typing", 1_200_000_000_000).unwrap();
        std::fs::rename(item_path(root, &typing), pane_dir(root, SOCKET, "w1:p1").join(format!(".{}.typing", typing.id))).unwrap();

        assert_eq!(pending(root, SOCKET, "w1:p1"), [first, second]);
        #[cfg(unix)]
        {
            let mode = std::fs::metadata(dir(root)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o700);
        }
    }

    #[test]
    fn two_messages_in_one_millisecond_keep_their_order() {
        let home = tempfile::tempdir().unwrap();
        let a = enqueue(home.path(), SOCKET, "w1:p1", "brief", "a", 1_000_000_000_000).unwrap();
        let b = enqueue(home.path(), SOCKET, "w1:p1", "brief", "b", 1_000_000_000_000).unwrap();
        let texts: Vec<String> = pending(home.path(), SOCKET, "w1:p1").into_iter().map(|i| i.text).collect();
        assert_eq!(texts, ["a", "b"]);
        assert!(a.id < b.id);
    }

    #[test]
    fn an_item_remembers_the_terminal_it_was_queued_for() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        assert_eq!(enqueue(root, SOCKET, "w1:p1", "brief", "a", 1).unwrap().terminal_id, "");
        progress::touch_channel(root, SOCKET, "w1:p1", "term_7", "omp", 1).unwrap();
        assert_eq!(enqueue(root, SOCKET, "w1:p1", "brief", "b", 2).unwrap().terminal_id, "term_7");
    }

    fn in_pane(home: &Path) -> Env {
        Env::for_test(home, &[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w1:p1"), ("HERDR_SOCKET_PATH", SOCKET)])
    }

    fn pane_ctx<'a>(env: &'a Env, runner: &'a FakeRunner, root: &Path, cwd: &Path) -> Ctx<'a> {
        let reply = serde_json::json!({"result": {"pane": {"pane_id": "w1:p1", "terminal_id": "term", "agent": "omp", "cwd": cwd}}});
        runner.on("pane current", ok(&reply.to_string()));
        Ctx { env, root: root.to_path_buf(), config_dir: root.join("cfg"), runner, detached_ticker: false }
    }

    #[test]
    fn ack_removes_the_callers_items_and_refuses_ids_that_could_leave_its_directory() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let env = in_pane(home.path());
        let runner = FakeRunner::new();
        let ctx = pane_ctx(&env, &runner, &root, home.path());
        let mine = enqueue(&root, SOCKET, "w1:p1", "brief", "mine", 1_000_000_000_000).unwrap();
        let theirs = enqueue(&root, SOCKET, "w1:p2", "brief", "theirs", 1_000_000_000_000).unwrap();
        // A file one level up that a traversal would reach.
        let outside = dir(&root).join(format!("{}.json", theirs.id));
        std::fs::write(&outside, "{}").unwrap();

        assert!(ack(&ctx, &[format!("../{}", theirs.id)]).is_err());
        assert!(ack(&ctx, &["..".into()]).is_err());
        assert!(outside.exists());
        // Another pane's id is not in the caller's directory: nothing happens.
        ack(&ctx, &[theirs.id.clone(), "1234-deadbeef".into()]).unwrap();
        assert_eq!(pending(&root, SOCKET, "w1:p2").len(), 1);
        ack(&ctx, std::slice::from_ref(&mine.id)).unwrap();
        assert!(pending(&root, SOCKET, "w1:p1").is_empty());
    }

    #[test]
    fn ack_takes_an_item_a_typist_claimed_so_it_never_comes_back() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let env = in_pane(home.path());
        let runner = FakeRunner::new();
        let ctx = pane_ctx(&env, &runner, &root, home.path());
        let item = enqueue(&root, SOCKET, "w1:p1", "brief", "b", 1).unwrap();
        let claimed = pane_dir(&root, SOCKET, "w1:p1").join(format!(".{}.typing", item.id));
        std::fs::rename(item_path(&root, &item), &claimed).unwrap();
        ack(&ctx, std::slice::from_ref(&item.id)).unwrap();
        // The typist found the channel fresh and tries to put it back.
        assert!(std::fs::rename(&claimed, item_path(&root, &item)).is_err());
        assert!(pending(&root, SOCKET, "w1:p1").is_empty());
    }

    #[test]
    fn ack_fails_in_a_pane_herdr_cannot_resolve_and_is_a_no_op_outside_herdr() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let item = enqueue(&root, SOCKET, "w1:p1", "brief", "b", 1).unwrap();
        let runner = FakeRunner::new();
        runner.on("pane current", fail(1, r#"{"error":{"code":"timeout","message":"slow"}}"#));
        let env = in_pane(home.path());
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: root.join("cfg"), runner: &runner, detached_ticker: false };
        assert!(ack(&ctx, std::slice::from_ref(&item.id)).is_err());
        assert_eq!(pending(&root, SOCKET, "w1:p1").len(), 1);

        // Not a Herdr pane, even with a stray pane id.
        let outside = Env::for_test(home.path(), &[("HERDR_PANE_ID", "w1:p1")]);
        let ctx = Ctx { env: &outside, root: root.clone(), config_dir: root.join("cfg"), runner: &runner, detached_ticker: false };
        ack(&ctx, std::slice::from_ref(&item.id)).unwrap();
    }

    #[test]
    fn a_pull_fails_in_a_pane_herdr_cannot_resolve_so_the_extension_keeps_its_state() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        enqueue(&root, SOCKET, "w1:p1", "brief", "b", 1).unwrap();
        let runner = FakeRunner::new();
        runner.on("pane current", fail(1, r#"{"error":{"code":"unreachable","message":"refused"}}"#));
        let env = in_pane(home.path());
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: root.join("cfg"), runner: &runner, detached_ticker: false };
        assert!(pulled(&ctx).is_err());

        let outside = Env::for_test(home.path(), &[("HERDR_PANE_ID", "w1:p1")]);
        let ctx = Ctx { env: &outside, root: root.clone(), config_dir: root.join("cfg"), runner: &runner, detached_ticker: false };
        assert_eq!(pulled(&ctx).unwrap(), (false, Vec::new()));
    }

    #[test]
    fn a_pull_drops_items_queued_for_an_earlier_terminal_with_the_pane_id() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        progress::touch_channel(&root, SOCKET, "w1:p1", "term_old", "omp", 1).unwrap();
        let old = enqueue(&root, SOCKET, "w1:p1", "brief", "old", 1).unwrap();
        let unknown = enqueue(&root, SOCKET, "w1:p1", "brief", "unknown", 2).unwrap();
        std::fs::write(item_path(&root, &unknown), serde_json::to_string(&Item { terminal_id: String::new(), ..unknown.clone() }).unwrap()).unwrap();
        let env = in_pane(home.path());
        // `pane_ctx` reports terminal `term`.
        let (claimed, items) = pulled(&pane_ctx(&env, &FakeRunner::new(), &root, &project.canonical_dir())).unwrap();
        assert!(claimed);
        assert_eq!(items.iter().map(|i| i.text.as_str()).collect::<Vec<_>>(), ["unknown"]);
        assert!(!item_path(&root, &old).exists());
    }

    #[test]
    fn only_panes_herdr_projects_sends_to_are_claimed_and_get_a_heartbeat() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        std::fs::create_dir_all(&root).unwrap();
        let project = project::create(&root, "demo", "", vec![]).unwrap();
        let env = in_pane(home.path());
        let elsewhere = tempfile::tempdir().unwrap();
        let fresh = || progress::channel_fresh(&root, SOCKET, "w1:p1", progress::now());

        // Some other OMP session: no heartbeat, never routed, and handed
        // nothing, since the fallback sees no heartbeat and types it too.
        let queued = enqueue(&root, SOCKET, "w1:p1", "brief", "for a claimed pane", 1).unwrap();
        let runner = FakeRunner::new();
        assert_eq!(pulled(&pane_ctx(&env, &runner, &root, elsewhere.path())).unwrap(), (false, Vec::new()));
        assert!(progress::load(&root, SOCKET, "w1:p1").is_none());
        remove(&root, &queued);

        // A thread's pane in this session; a resolved one or another session's does not count.
        project.update_coordinator(|c| c.socket = SOCKET.into()).unwrap();
        let t = thread::allocate(&project, |t| t.pane_id = "w1:p1".into()).unwrap();
        assert!(pulled(&pane_ctx(&env, &FakeRunner::new(), &root, elsewhere.path())).unwrap().0);
        assert!(fresh());
        progress::remove(&root, SOCKET, "w1:p1");
        thread::update(&project, &t.id, |t| t.status = thread::Status::Resolved).unwrap();
        assert!(!pulled(&pane_ctx(&env, &FakeRunner::new(), &root, elsewhere.path())).unwrap().0);
        thread::update(&project, &t.id, |t| t.status = thread::Status::Open).unwrap();
        project.update_coordinator(|c| c.socket = "/tmp/b.sock".into()).unwrap();
        assert!(!pulled(&pane_ctx(&env, &FakeRunner::new(), &root, elsewhere.path())).unwrap().0);

        // A coordinator: works in the project folder, or below it.
        let below = project.canonical_dir().join("notes");
        std::fs::create_dir_all(&below).unwrap();
        for cwd in [project.canonical_dir(), below] {
            progress::remove(&root, SOCKET, "w1:p1");
            let item = enqueue(&root, SOCKET, "w1:p1", "coordinator", "hi", 1).unwrap();
            let (claimed, items) = pulled(&pane_ctx(&env, &FakeRunner::new(), &root, &cwd)).unwrap();
            assert!(claimed, "{}", cwd.display());
            assert_eq!(items, std::slice::from_ref(&item));
            assert!(fresh());
            remove(&root, &item);
        }
    }

    fn herdr(runner: &FakeRunner) -> Herdr<'_> {
        Herdr::new("herdr", SOCKET, runner)
    }

    fn omp_pane(pane_id: &str, terminal_id: &str) -> (Pane, Agent) {
        (Pane { pane_id: pane_id.into(), terminal_id: terminal_id.into(), ..Pane::default() }, Agent { pane_id: pane_id.into(), agent: "omp".into(), terminal_id: terminal_id.into(), ..Agent::default() })
    }

    fn prompts(runner: &FakeRunner) -> Vec<String> {
        runner.calls.borrow().iter().filter(|c| c.display().contains("agent prompt")).map(|c| c.args.last().unwrap().clone()).collect()
    }

    #[test]
    fn a_keystroke_types_the_panes_older_items_first() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        enqueue(root, SOCKET, "w1:p1", "brief", "brief", 1).unwrap();
        enqueue(root, SOCKET, "w1:p1", "nudge", "nudge", 2).unwrap();
        let runner = FakeRunner::new();
        runner.on("agent prompt", ok(r#"{"result":{}}"#));
        assert_eq!(send(root, &herdr(&runner), SOCKET, "w1:p1", false, "follow-up", "follow-up").unwrap(), Sent::Keystroke);
        assert_eq!(prompts(&runner), ["brief", "nudge", "follow-up"]);
        assert!(pending(root, SOCKET, "w1:p1").is_empty());
        assert_eq!(std::fs::read_dir(pane_dir(root, SOCKET, "w1:p1")).unwrap().count(), 0, "no claimed file left");
    }

    #[test]
    fn a_keystroke_refused_for_an_older_item_keeps_both() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        enqueue(root, SOCKET, "w1:p1", "brief", "brief", 1).unwrap();
        let runner = FakeRunner::new();
        runner.on("agent prompt", fail(1, r#"{"error":{"code":"agent_blocked","message":"blocked"}}"#));
        assert_eq!(send(root, &herdr(&runner), SOCKET, "w1:p1", false, "follow-up", "later").unwrap_err().code, "agent_blocked");
        assert_eq!(prompts(&runner), ["brief"]);
        assert_eq!(pending(root, SOCKET, "w1:p1").len(), 1);
    }

    #[test]
    fn a_keystroke_queues_behind_older_items_when_the_extension_came_back() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        enqueue(root, SOCKET, "w1:p1", "brief", "brief", 1).unwrap();
        // The caller found the channel stale; a pull came in since.
        progress::touch_channel(root, SOCKET, "w1:p1", "", "omp", progress::now()).unwrap();
        let runner = FakeRunner::new();
        assert_eq!(send(root, &herdr(&runner), SOCKET, "w1:p1", false, "follow-up", "later").unwrap(), Sent::Queued);
        assert!(prompts(&runner).is_empty());
        let texts: Vec<String> = pending(root, SOCKET, "w1:p1").into_iter().map(|i| i.text).collect();
        assert_eq!(texts, ["brief", "later"]);
    }

    #[test]
    fn a_routed_send_queues_and_a_remote_one_never_types_local_items() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let runner = FakeRunner::new();
        runner.on("agent prompt", ok(r#"{"result":{}}"#));
        assert_eq!(send(root, &herdr(&runner), SOCKET, "w1:p1", true, "brief", "local").unwrap(), Sent::Queued);
        assert_eq!(send(root, &herdr(&runner).on_machine("box"), SOCKET, "w1:p1", false, "brief", "remote").unwrap(), Sent::Keystroke);
        assert_eq!(prompts(&runner), ["remote"]);
        assert_eq!(pending(root, SOCKET, "w1:p1").len(), 1);
    }

    #[test]
    fn a_keystroke_drops_older_items_of_another_harness_or_terminal() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        progress::touch_channel(root, SOCKET, "w1:p1", "term_1", "omp", 1).unwrap();
        enqueue(root, SOCKET, "w1:p1", "brief", "old terminal", 1).unwrap();
        progress::save(root, &progress::Record { socket: SOCKET.into(), pane_id: "w1:p1".into(), terminal_id: "term_2".into(), agent: "omp".into(), ..Default::default() }).unwrap();
        let runner = FakeRunner::new();
        runner.on("agent prompt", ok(r#"{"result":{}}"#));
        send(root, &herdr(&runner), SOCKET, "w1:p1", false, "follow-up", "new").unwrap();
        assert_eq!(prompts(&runner), ["new"]);
        assert!(pending(root, SOCKET, "w1:p1").is_empty());
    }

    #[test]
    fn fallback_types_an_item_once_after_a_minute_without_a_heartbeat() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let runner = FakeRunner::new();
        runner.on("agent prompt", ok(r#"{"result":{}}"#));
        let (pane, agent) = omp_pane("w1:p1", "term");
        let (panes, agents) = ([pane], [agent]);
        let created = 1_000_000_000_000;
        progress::touch_channel(root, SOCKET, "w1:p1", "term", "omp", 0).unwrap();
        enqueue(root, SOCKET, "w1:p1", "brief", "hello", created).unwrap();
        enqueue(root, "/tmp/b.sock", "w1:p1", "brief", "other session", created).unwrap();

        // Too young.
        fallback(root, &herdr(&runner), SOCKET, &panes, &agents, created + FALLBACK_MS - 1);
        assert_eq!(runner.count("agent prompt"), 0);
        // Old enough, but the extension is still pulling.
        let now_ms = created + FALLBACK_MS;
        progress::touch_channel(root, SOCKET, "w1:p1", "term", "omp", now_ms / 1000 - 5).unwrap();
        fallback(root, &herdr(&runner), SOCKET, &panes, &agents, now_ms);
        assert_eq!(runner.count("agent prompt"), 0);
        // The heartbeat stopped: typed once, then gone.
        let later = now_ms + 20_000;
        fallback(root, &herdr(&runner), SOCKET, &panes, &agents, later);
        fallback(root, &herdr(&runner), SOCKET, &panes, &agents, later);
        assert_eq!(prompts(&runner), ["hello"]);
        assert!(pending(root, SOCKET, "w1:p1").is_empty());
        // Another session's item is that session's business.
        assert_eq!(pending(root, "/tmp/b.sock", "w1:p1").len(), 1);
    }

    #[test]
    fn a_refused_fallback_keeps_the_item_and_a_missing_agent_drops_it() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let created = 1_000_000_000_000;
        let now_ms = created + FALLBACK_MS + 1;
        let (pane, agent) = omp_pane("w1:p1", "");
        let (panes, agents) = ([pane], [agent]);
        enqueue(root, SOCKET, "w1:p1", "brief", "first", created).unwrap();
        enqueue(root, SOCKET, "w1:p1", "follow-up", "second", created + 1).unwrap();

        let refusing = FakeRunner::new();
        refusing.on("agent prompt", fail(1, r#"{"error":{"code":"agent_blocked","message":"blocked"}}"#));
        fallback(root, &herdr(&refusing), SOCKET, &panes, &agents, now_ms);
        // The second item waits behind the first.
        assert_eq!(refusing.count("agent prompt"), 1);
        assert_eq!(pending(root, SOCKET, "w1:p1").len(), 2);

        // What herdr answers when the agent exited between the lists and the prompt.
        let gone = FakeRunner::new();
        gone.on("agent prompt", fail(1, r#"{"error":{"code":"agent_not_found","message":"no agent"}}"#));
        fallback(root, &herdr(&gone), SOCKET, &panes, &agents, now_ms);
        assert!(pending(root, SOCKET, "w1:p1").is_empty());
    }

    #[test]
    fn fallback_never_types_into_another_harness_a_later_terminal_or_a_closed_pane() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let created = 1_000_000_000_000;
        let now_ms = created + FALLBACK_MS + 1;
        for pane in ["w1:p1", "w1:p2", "w1:p3", "w1:p4"] {
            progress::touch_channel(root, SOCKET, pane, "term_old", "omp", 0).unwrap();
            enqueue(root, SOCKET, pane, "brief", pane, created).unwrap();
        }
        enqueue(root, "/tmp/b.sock", "w1:p4", "brief", "other session", created).unwrap();
        let (p1, _) = omp_pane("w1:p1", "term_old");
        let (p2, mut claude) = omp_pane("w1:p2", "term_old");
        claude.agent = "claude".into();
        let (p3, reused) = omp_pane("w1:p3", "term_new");
        // w1:p1 runs no agent any more; w1:p4 is closed.
        let runner = FakeRunner::new();
        runner.on("agent prompt", ok(r#"{"result":{}}"#));
        fallback(root, &herdr(&runner), SOCKET, &[p1, p2, p3], &[claude, reused], now_ms);

        assert!(prompts(&runner).is_empty());
        for pane in ["w1:p1", "w1:p2", "w1:p3"] {
            assert!(pending(root, SOCKET, pane).is_empty(), "{pane}");
        }
        assert!(!pane_dir(root, SOCKET, "w1:p4").exists());
        assert_eq!(pending(root, "/tmp/b.sock", "w1:p4").len(), 1);
    }

    #[test]
    fn fallback_puts_back_a_claim_whose_typist_died() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let created = 1_000_000_000_000;
        let now_ms = created + FALLBACK_MS + 1;
        let at = |ms: i64| std::time::UNIX_EPOCH + std::time::Duration::from_millis(ms as u64);
        let claim = |item: &Item, ms: i64| {
            std::fs::rename(item_path(root, item), claimed_path(root, item)).unwrap();
            std::fs::File::options().write(true).open(claimed_path(root, item)).unwrap().set_modified(at(ms)).unwrap();
        };
        let dead = enqueue(root, SOCKET, "w1:p1", "brief", "dead", created).unwrap();
        claim(&dead, created);
        // Claimed a moment ago: its typist is still at work.
        let busy = enqueue(root, SOCKET, "w1:p1", "brief", "busy", created + 1).unwrap();
        claim(&busy, now_ms - 1);
        let (pane, agent) = omp_pane("w1:p1", "");
        let runner = FakeRunner::new();
        runner.on("agent prompt", ok(r#"{"result":{}}"#));
        fallback(root, &herdr(&runner), SOCKET, &[pane], &[agent], now_ms);
        assert_eq!(prompts(&runner), ["dead"]);
        assert!(claimed_path(root, &busy).exists());
    }
}
