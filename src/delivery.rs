//! Messages for an agent pane (briefs, nudges, follow-ups). When the OMP
//! extension is live in the pane, the message is queued as a file it pulls
//! and hands to the agent without touching the input box; otherwise it is
//! typed through `herdr agent prompt`. The binary owns every channel file:
//! `<root>/.channel/<pane>-<socket hash>/<id>.json`, one message per file,
//! ids sorting by creation.

use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::herdr::{Herdr, HerdrError};
use crate::paths::Ctx;
use crate::progress;

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
    /// Unix milliseconds.
    pub created_at: i64,
}

fn dir(root: &Path) -> PathBuf {
    root.join(".channel")
}

fn pane_dir(root: &Path, socket: &str, pane_id: &str) -> PathBuf {
    dir(root).join(progress::record_stem(socket, pane_id))
}

/// Ids are `<13-digit unix ms>-<8 hex>`; nothing else names a file, so an id
/// from the command line can never leave the pane's directory.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c) || c == '-')
}

/// Whether a message to this pane is queued for the extension. A remote
/// pane's extension runs against the remote machine's root, so it never sees
/// this root's files.
pub fn routed(root: &Path, socket: &str, pane: &str, remote: bool) -> bool {
    !remote && progress::channel_fresh(root, socket, pane, progress::now())
}

pub fn send(root: &Path, herdr: &Herdr, socket: &str, pane: &str, remote: bool, kind: &str, text: &str) -> Result<Sent, HerdrError> {
    if !routed(root, socket, pane, remote) {
        return herdr.agent_prompt(pane, text).map(|()| Sent::Keystroke);
    }
    enqueue(root, socket, pane, kind, text, jiff::Timestamp::now().as_millisecond())
        .map(|_| Sent::Queued)
        .map_err(|e| HerdrError { code: "failed".into(), message: format!("could not queue the message for the OMP extension: {e:#}") })
}

fn enqueue(root: &Path, socket: &str, pane: &str, kind: &str, text: &str, now_ms: i64) -> Result<Item> {
    // The process id keeps two processes apart within one millisecond, the
    // counter two messages of one process, in order.
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let suffix = ((std::process::id() & 0xffff) << 16) | (COUNTER.fetch_add(1, Ordering::Relaxed) & 0xffff);
    let item = Item { id: format!("{now_ms:013}-{suffix:08x}"), kind: kind.into(), text: text.into(), socket: socket.into(), pane_id: pane.into(), created_at: now_ms };
    // The text is typed into an agent: only this user may read or add items.
    let top = dir(root);
    std::fs::DirBuilder::new().recursive(true).mode(0o700).create(pane_dir(root, socket, pane))?;
    std::fs::set_permissions(&top, std::fs::Permissions::from_mode(0o700))?;
    crate::project::write_json(&pane_dir(root, socket, pane).join(format!("{}.json", item.id)), &item)?;
    Ok(item)
}

/// The pane's queued items, oldest first. An item whose stored socket and
/// pane differ from its directory's is never handed out.
pub fn pending(root: &Path, socket: &str, pane_id: &str) -> Vec<Item> {
    let mut items = items_in(&pane_dir(root, socket, pane_id));
    items.retain(|i| i.socket == socket && i.pane_id == pane_id);
    items
}

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
    let _ = std::fs::remove_file(pane_dir(root, &item.socket, &item.pane_id).join(format!("{}.json", item.id)));
}

/// `channel pull --agent omp`, run by the extension every two seconds in its
/// pane: records the heartbeat and prints the pane's items as a JSON array.
pub fn pull(ctx: &Ctx) -> Result<()> {
    let Some(pane) = progress::current(ctx.env, ctx.runner) else {
        println!("[]");
        return Ok(());
    };
    progress::touch_channel(&ctx.root, &pane.socket, &pane.pane_id, &pane.terminal_id, &pane.agent, progress::now())?;
    let items: Vec<serde_json::Value> = pending(&ctx.root, &pane.socket, &pane.pane_id).into_iter().map(|i| serde_json::json!({ "id": i.id, "kind": i.kind, "text": i.text })).collect();
    println!("{}", serde_json::to_string(&items)?);
    Ok(())
}

/// `channel ack <id>...`: the extension handed these items to the agent. An
/// id that is already gone (the fallback typed it) is not an error.
pub fn ack(ctx: &Ctx, ids: &[String]) -> Result<()> {
    if let Some(bad) = ids.iter().find(|id| !valid_id(id)) {
        bail!("`{bad}` is not a channel item id");
    }
    let Some(pane) = progress::current(ctx.env, ctx.runner) else {
        return Ok(());
    };
    for id in ids {
        let _ = std::fs::remove_file(pane_dir(&ctx.root, &pane.socket, &pane.pane_id).join(format!("{id}.json")));
    }
    Ok(())
}

/// Items of this socket that sat for a minute while their pane's extension
/// stopped pulling are typed instead, oldest first, once each. A refused
/// prompt keeps the item (and those behind it) for the next tick; a pane that
/// is gone drops them, so a later pane reusing the id never gets them.
pub fn fallback(root: &Path, herdr: &Herdr, socket: &str, now_ms: i64) {
    let Ok(entries) = std::fs::read_dir(dir(root)) else {
        return;
    };
    for entry in entries.flatten() {
        for item in items_in(&entry.path()) {
            if item.socket != socket || pane_dir(root, &item.socket, &item.pane_id) != entry.path() {
                continue;
            }
            if now_ms - item.created_at < FALLBACK_MS || progress::channel_fresh(root, socket, &item.pane_id, now_ms / 1000) {
                break;
            }
            match herdr.agent_prompt(&item.pane_id, &item.text) {
                Ok(()) => remove(root, &item),
                Err(error) if error.code == "pane_not_found" => remove(root, &item),
                Err(_) => break,
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

        assert_eq!(pending(root, SOCKET, "w1:p1"), [first, second]);
        let mode = std::fs::metadata(dir(root)).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
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

    fn pane_ctx<'a>(env: &'a Env, runner: &'a FakeRunner, root: &Path) -> Ctx<'a> {
        runner.on("pane current", ok(r#"{"result":{"pane":{"pane_id":"w1:p1","terminal_id":"term","agent":"omp"}}}"#));
        Ctx { env, root: root.to_path_buf(), config_dir: root.join("cfg"), runner, detached_ticker: false }
    }

    #[test]
    fn ack_removes_the_callers_items_and_refuses_ids_that_could_leave_its_directory() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("root");
        let env = Env::for_test(home.path(), &[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w1:p1"), ("HERDR_SOCKET_PATH", SOCKET)]);
        let runner = FakeRunner::new();
        let ctx = pane_ctx(&env, &runner, &root);
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
    fn fallback_types_an_item_once_after_a_minute_without_a_heartbeat() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let runner = FakeRunner::new();
        runner.on("agent prompt", ok(r#"{"result":{}}"#));
        let herdr = Herdr::new("herdr", SOCKET, &runner);
        let created = 1_000_000_000_000;
        enqueue(root, SOCKET, "w1:p1", "brief", "hello", created).unwrap();
        enqueue(root, "/tmp/b.sock", "w1:p1", "brief", "other session", created).unwrap();

        // Too young.
        fallback(root, &herdr, SOCKET, created + FALLBACK_MS - 1);
        assert_eq!(runner.count("agent prompt"), 0);
        // Old enough, but the extension is still pulling.
        let now_ms = created + FALLBACK_MS;
        progress::touch_channel(root, SOCKET, "w1:p1", "term", "omp", now_ms / 1000 - 5).unwrap();
        fallback(root, &herdr, SOCKET, now_ms);
        assert_eq!(runner.count("agent prompt"), 0);
        // The heartbeat stopped: typed once, then gone.
        let later = now_ms + 20_000;
        fallback(root, &herdr, SOCKET, later);
        fallback(root, &herdr, SOCKET, later);
        assert_eq!(runner.count("agent prompt"), 1);
        let calls = runner.calls.borrow();
        assert_eq!(calls[0].args.last().unwrap(), "hello");
        drop(calls);
        assert!(pending(root, SOCKET, "w1:p1").is_empty());
        // Another session's item is that session's business.
        assert_eq!(pending(root, "/tmp/b.sock", "w1:p1").len(), 1);
    }

    #[test]
    fn a_refused_fallback_keeps_the_item_and_a_gone_pane_drops_it() {
        let home = tempfile::tempdir().unwrap();
        let root = home.path();
        let created = 1_000_000_000_000;
        let now_ms = created + FALLBACK_MS;
        enqueue(root, SOCKET, "w1:p1", "brief", "first", created).unwrap();
        enqueue(root, SOCKET, "w1:p1", "follow-up", "second", created + 1).unwrap();

        let refusing = FakeRunner::new();
        refusing.on("agent prompt", fail(1, r#"{"error":{"code":"agent_blocked","message":"blocked"}}"#));
        fallback(root, &Herdr::new("herdr", SOCKET, &refusing), SOCKET, now_ms + 1);
        // The second item waits behind the first.
        assert_eq!(refusing.count("agent prompt"), 1);
        assert_eq!(pending(root, SOCKET, "w1:p1").len(), 2);

        let gone = FakeRunner::new();
        gone.on("agent prompt", fail(1, r#"{"error":{"code":"pane_not_found","message":"no pane"}}"#));
        fallback(root, &Herdr::new("herdr", SOCKET, &gone), SOCKET, now_ms + 1);
        assert!(pending(root, SOCKET, "w1:p1").is_empty());
    }
}
