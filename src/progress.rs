//! Built-in progress self-reporting: the agent runs `report` in its own pane,
//! the harness hooks (`hook`) inject the instructions and a reminder, one JSON
//! record per pane under `<root>/.progress/` feeds the ticker. The binding is
//! the pane id Herdr hands every pane shell; there is nothing to mint and no
//! daemon. Outside a Herdr pane both commands do nothing.

use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::herdr::{CALL_TIMEOUT, Herdr};
use crate::paths::{Ctx, Env};
use crate::runner::Runner;

pub const ACTIVITY_COLUMNS: usize = 40;
pub const ACTIVITY_TTL_MS: u64 = 300_000;
/// Reminders go out at most about once a minute.
pub const REMIND_SECS: i64 = 60;
pub const WAITING: &str = "Waiting for you";
pub const DONE: &str = "Done";
/// The OMP extension pulls its channel every two seconds; five missed pulls
/// and queued prompts go back to keystrokes.
pub const CHANNEL_FRESH_SECS: i64 = 10;
pub const CHANNEL_OMP: &str = "omp-ext";

/// One pane's self-report.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Record {
    pub socket: String,
    pub pane_id: String,
    /// Pane ids restart from `w1` after a server restart; the terminal id tells
    /// a stale record from a live pane that reused the id.
    pub terminal_id: String,
    pub agent: String,
    pub activity: String,
    pub percent: Option<u8>,
    /// Unix seconds of the last `report`; 0 when none since the session started.
    pub reported_at: i64,
    /// Unix seconds of the last reminder the hook injected.
    pub reminded_at: i64,
    pub session_started_at: i64,
    /// `omp-ext` while the OMP extension pulls this pane's prompt channel.
    pub channel: String,
    /// Unix seconds of the last `channel pull`; 0 when never.
    pub channel_seen: i64,
}

impl Record {
    pub fn waiting(&self) -> bool {
        self.reported_at > 0 && self.activity == WAITING
    }

    pub fn done(&self) -> bool {
        self.reported_at > 0 && self.percent == Some(100)
    }

    pub fn reported(&self) -> bool {
        self.reported_at > 0
    }
}

pub fn now() -> i64 {
    jiff::Timestamp::now().as_second()
}

pub fn dir(root: &Path) -> PathBuf {
    root.join(".progress")
}

/// `<pane id>-<short hash of the socket path>`: pane ids repeat across sessions.
pub fn record_stem(socket: &str, pane_id: &str) -> String {
    let hash = &crate::thread::sha256_hex(socket.as_bytes())[..8];
    format!("{}-{hash}", pane_id.replace(':', "_"))
}

pub fn path(root: &Path, socket: &str, pane_id: &str) -> PathBuf {
    dir(root).join(format!("{}.json", record_stem(socket, pane_id)))
}

pub fn load(root: &Path, socket: &str, pane_id: &str) -> Option<Record> {
    crate::project::read_json(&path(root, socket, pane_id))
}

pub fn save(root: &Path, record: &Record) -> Result<()> {
    std::fs::create_dir_all(dir(root))?;
    crate::project::write_json(&path(root, &record.socket, &record.pane_id), record)
}

/// Load-modify-save under one lock for the whole directory: `channel pull`
/// rewrites the record every two seconds and must not drop a concurrent
/// `report` or hook write. Saves only when the record changed.
fn update(root: &Path, socket: &str, pane_id: &str, change: impl FnOnce(&mut Record)) -> Result<()> {
    std::fs::create_dir_all(dir(root))?;
    let lock = std::fs::File::options().create(true).truncate(false).write(true).open(dir(root).join(".lock"))?;
    lock.lock()?;
    let mut record = load(root, socket, pane_id).unwrap_or_default();
    let before = record.clone();
    change(&mut record);
    if record != before {
        save(root, &record)?;
    }
    Ok(())
}

/// The heartbeat of `channel pull`: marks the pane's channel live without
/// touching the report, so heartbeats never age or refresh progress. The
/// live terminal id always wins, so items queued now are stamped with it;
/// a report of an earlier terminal with this pane id is dropped with it.
pub fn touch_channel(root: &Path, socket: &str, pane_id: &str, terminal_id: &str, agent: &str, now: i64) -> Result<()> {
    update(root, socket, pane_id, |record| {
        if !terminal_id.is_empty() && record.terminal_id != terminal_id {
            if !record.terminal_id.is_empty() {
                *record = Record::default();
            }
            record.terminal_id = terminal_id.to_string();
        }
        record.socket = socket.to_string();
        record.pane_id = pane_id.to_string();
        if record.agent.is_empty() {
            record.agent = agent.to_string();
        }
        record.channel = CHANNEL_OMP.to_string();
        record.channel_seen = now;
    })
}

/// Whether the OMP extension pulled this pane's channel within the last
/// `CHANNEL_FRESH_SECS`, so a queued prompt will be picked up.
pub fn channel_fresh(root: &Path, socket: &str, pane_id: &str, now: i64) -> bool {
    load(root, socket, pane_id).is_some_and(|r| r.channel == CHANNEL_OMP && now - r.channel_seen <= CHANNEL_FRESH_SECS)
}

pub fn remove(root: &Path, socket: &str, pane_id: &str) {
    let _ = std::fs::remove_file(path(root, socket, pane_id));
}

/// Every record under the root, for the ticker's cleanup.
pub fn all(root: &Path) -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir(root)) else {
        return Vec::new();
    };
    entries.flatten().filter_map(|e| crate::project::read_json::<Record>(&e.path())).collect()
}

/// At most 40 columns, no control or bidi characters, trimmed. 100% is "Done".
pub fn clean(input: &str, columns: usize) -> String {
    let mut width = 0;
    input
        .chars()
        .filter(|c| !c.is_control() && !matches!(*c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'))
        .take(120)
        .take_while(|c| {
            // East Asian wide characters take two columns; everything else one.
            width += if ('\u{1100}'..='\u{115f}').contains(c) || ('\u{2e80}'..='\u{a4cf}').contains(c) || ('\u{ac00}'..='\u{d7a3}').contains(c) || ('\u{f900}'..='\u{faff}').contains(c) || ('\u{fe30}'..='\u{fe4f}').contains(c) || ('\u{ff00}'..='\u{ff60}').contains(c) || ('\u{ffe0}'..='\u{ffe6}').contains(c) || ('\u{1f300}'..='\u{1f64f}').contains(c) || ('\u{1f900}'..='\u{1f9ff}').contains(c) || ('\u{20000}'..='\u{3fffd}').contains(c) { 2 } else { 1 };
            width <= columns
        })
        .collect::<String>()
        .trim()
        .to_string()
}

/// The calling pane, when this process runs inside a Herdr pane: the id
/// Herdr shows for it (a moved pane keeps its launch-time `HERDR_PANE_ID`),
/// its terminal id, the agent Herdr detects and its working directories
/// (empty when herdr does not report them). `None` outside Herdr.
pub struct Current {
    pub socket: String,
    pub pane_id: String,
    pub terminal_id: String,
    pub agent: String,
    pub cwd: String,
    pub foreground_cwd: String,
}

pub fn current(env: &Env, runner: &dyn Runner) -> Option<Current> {
    current_within(env, runner, CALL_TIMEOUT)
}

/// The hook uses a short timeout: a slow server must not stall every tool call.
pub fn current_within(env: &Env, runner: &dyn Runner, timeout: std::time::Duration) -> Option<Current> {
    if env.var("HERDR_ENV") != Some("1") {
        return None;
    }
    env.var("HERDR_PANE_ID")?;
    let socket = env.var("HERDR_SOCKET_PATH")?.to_string();
    let herdr = Herdr::new(env.herdr_bin(), &socket, runner);
    let result = herdr.call(&["pane", "current", "--current"], timeout).ok()?;
    let pane = &result["pane"];
    let pane_id = pane["pane_id"].as_str()?.to_string();
    Some(Current {
        socket,
        pane_id,
        terminal_id: pane["terminal_id"].as_str().unwrap_or("").to_string(),
        agent: pane["agent"].as_str().unwrap_or("").to_string(),
        cwd: pane["cwd"].as_str().unwrap_or("").to_string(),
        foreground_cwd: pane["foreground_cwd"].as_str().unwrap_or("").to_string(),
    })
}

/// `report --percent N|--unknown --activity "..."`, run by the agent in its
/// pane. Writes the record and one `hp_activity` token with a TTL, so silence
/// clears the activity after five minutes with no daemon.
pub fn report(ctx: &Ctx, percent: Option<u8>, activity: &str) -> Result<()> {
    let Some(pane) = current(ctx.env, ctx.runner) else {
        println!("not inside a Herdr pane; nothing reported");
        return Ok(());
    };
    if percent.is_some_and(|p| p > 100) {
        bail!("--percent must be 0 to 100");
    }
    let activity = if percent == Some(100) { DONE.to_string() } else { clean(activity, ACTIVITY_COLUMNS) };
    if activity.is_empty() {
        bail!("--activity is empty");
    }
    update(&ctx.root, &pane.socket, &pane.pane_id, |record| {
        record.socket = pane.socket.clone();
        record.pane_id = pane.pane_id.clone();
        record.terminal_id = pane.terminal_id.clone();
        record.agent = pane.agent.clone();
        record.activity = activity.clone();
        record.percent = percent;
        record.reported_at = now();
    })?;
    let herdr = Herdr::new(ctx.env.herdr_bin(), &pane.socket, ctx.runner);
    let token = format!("hp_activity={activity}");
    let ttl = ACTIVITY_TTL_MS.to_string();
    if let Err(error) = herdr.call(&["pane", "report-metadata", &pane.pane_id, "--source", crate::herdr::SOURCE, "--token", &token, "--ttl-ms", &ttl], CALL_TIMEOUT) {
        println!("recorded; the sidebar token was not set ({error})");
    } else {
        println!("recorded: {}{activity}", percent.map(|p| format!("{p}% · ")).unwrap_or_default());
    }
    Ok(())
}

/// `progress [--pane ID]`: the record for the calling pane, or the given one.
pub fn print(ctx: &Ctx, pane: Option<&str>) -> Result<()> {
    let (socket, pane_id) = match pane {
        Some(id) => (ctx.env.var("HERDR_SOCKET_PATH").unwrap_or("").to_string(), id.to_string()),
        None => match current(ctx.env, ctx.runner) {
            Some(c) => (c.socket, c.pane_id),
            None => bail!("not inside a Herdr pane; pass --pane ID"),
        },
    };
    match load(&ctx.root, &socket, &pane_id) {
        Some(record) => println!("{}", serde_json::to_string_pretty(&record)?),
        None => println!("null"),
    }
    Ok(())
}

// ---------------------------------------------------------------- hooks

/// The instructions the SessionStart hook injects, with the exact command.
pub fn instructions(prefix: &str, pane_id: &str) -> String {
    format!(
        "# Progress (herdr-projects)\n\n\
         Report the progress of the user's whole current task through `{prefix} report`. This is your estimate, not a timer or a count of tools. Reporting failures must never stop the actual work: give one short diagnostic and continue, without retries.\n\n\
         Report a rough percentage in five-point increments and a two-to-four-word activity, such as `Reading code`, `Testing changes` or `Waiting for you`:\n\n\
         `{prefix} report --percent 25 --activity 'Reading code'`\n\
         `{prefix} report --unknown --activity 'Assessing task'`\n\n\
         Report at the start of new work, after meaningful milestones, when the activity changes, at blockers, and before every substantive reply. Report `--activity 'Waiting for you'` whenever you stop to ask the user something. During active work aim for one report per minute at a natural tool boundary; never invent progress to satisfy a reminder. Revise the estimate downward when you discover more work; use `--unknown` while the scope is unclear.\n\n\
         Use `--percent 100` only when the entire requested outcome and its checks are finished; it displays `Done`. If more work is requested afterwards, report a fresh, lower percentage.\n\n\
         Only the top-level agent in this pane ({pane_id}) reports; subagents and helpers do not. Await each report command; do not run it in the background."
    )
}

pub fn reminder(prefix: &str) -> String {
    format!("Progress check-in is due if this is a natural boundary: `{prefix} report --percent N --activity '...'`. Reassess the current task; do not invent progress. Report `Waiting for you` before a question to the user.")
}

/// Events the reporter reacts to: the top-level agent's own SessionStart,
/// UserPromptSubmit and PostToolUse, never a subagent's, and never the
/// PostToolUse of the `report` call itself.
pub fn eligible(event: &serde_json::Value) -> bool {
    if ["agent_id", "subagent_id", "agent_transcript_path"].iter().any(|key| event.get(key).is_some_and(|v| !v.is_null())) {
        return false;
    }
    if event["transcript_path"].as_str().is_some_and(|p| p.contains("/subagents/")) {
        return false;
    }
    let name = event["hook_event_name"].as_str().unwrap_or("");
    if !matches!(name, "SessionStart" | "PostToolUse" | "UserPromptSubmit") {
        return false;
    }
    if name == "PostToolUse" && event["tool_input"].to_string().contains("herdr-projects") && event["tool_input"].to_string().contains(" report ") {
        return false;
    }
    true
}

/// What the hook answers for one event: the text to inject, and whether the
/// record changed. Pure, so it is testable without a pane.
pub fn respond(record: &mut Record, kind: &str, prefix: &str, now: i64) -> Option<String> {
    match kind {
        "SessionStart" => {
            // A new session in this pane: the old report no longer describes it.
            // An OMP session keeps the channel: it belongs to the extension, not
            // the session. Any other harness starting in the pane ends it, or
            // its prompts would wait for an extension that is not there.
            let old = std::mem::take(record);
            let (channel, channel_seen) = if old.agent == "omp" { (old.channel, old.channel_seen) } else { Default::default() };
            *record = Record { socket: old.socket, pane_id: old.pane_id, terminal_id: old.terminal_id, agent: old.agent, channel, channel_seen, session_started_at: now, reminded_at: now, ..Record::default() };
            Some(instructions(prefix, &record.pane_id))
        }
        "UserPromptSubmit" => {
            record.reminded_at = now;
            // The user answered: an old "Waiting for you" no longer holds.
            if record.activity == WAITING {
                record.activity.clear();
                record.reported_at = 0;
            }
            Some(format!("Before task tools or a blocking question, check whether this request starts new work; if so report a fresh estimate. Then report before waiting for the user, for example `{prefix} report --unknown --activity 'Waiting for you'`."))
        }
        "PostToolUse" => {
            let done = record.percent == Some(100);
            let due = now - record.reminded_at >= REMIND_SECS && !done && (record.reported_at == 0 || now - record.reported_at >= REMIND_SECS);
            if due {
                record.reminded_at = now;
                Some(reminder(prefix))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// `hook --agent claude|codex|omp`, the entry point the harness hooks (and the
/// OMP extension) run. Silent (exit 0, no output) outside a Herdr pane, so the
/// same hooks may sit in the user's settings for every session on the machine.
pub fn hook(ctx: &Ctx, agent: &str) -> Result<()> {
    if ctx.env.var("HERDR_ENV") != Some("1") || ctx.env.var("HERDR_PANE_ID").is_none() {
        return Ok(());
    }
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().take(1_048_576).read_to_string(&mut input)?;
    let Ok(event) = serde_json::from_str::<serde_json::Value>(&input) else {
        return Ok(());
    };
    if !eligible(&event) {
        return Ok(());
    }
    let kind = event["hook_event_name"].as_str().unwrap_or("").to_string();
    // Codex runs hooks for nested threads too; only the pane's own thread reports.
    if agent == "codex"
        && let Some(native) = event["session_id"].as_str()
        && ctx.env.var("CODEX_THREAD_ID").is_some_and(|id| id != native)
    {
        return Ok(());
    }
    // At SessionStart Herdr may not have detected the agent yet: wait briefly.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let pane = loop {
        match current_within(ctx.env, ctx.runner, std::time::Duration::from_secs(2)) {
            Some(p) if kind != "SessionStart" || !p.agent.is_empty() => break p,
            Some(p) if std::time::Instant::now() >= deadline => break p,
            None if kind != "SessionStart" || std::time::Instant::now() >= deadline => return Ok(()),
            _ => std::thread::sleep(std::time::Duration::from_millis(100)),
        }
    };
    if !pane.agent.is_empty() && pane.agent != agent {
        return Ok(()); // another harness's hook fired in a pane that is not its own
    }
    let prefix = crate::coordinator::current_prefix(&ctx.root)?;
    let mut text = None;
    update(&ctx.root, &pane.socket, &pane.pane_id, |record| {
        record.socket = pane.socket.clone();
        record.pane_id = pane.pane_id.clone();
        record.terminal_id = pane.terminal_id.clone();
        record.agent = if pane.agent.is_empty() { agent.to_string() } else { pane.agent.clone() };
        text = respond(record, &kind, &prefix, now());
    })?;
    if let Some(text) = text {
        println!("{}", serde_json::json!({"hookSpecificOutput": {"hookEventName": kind, "additionalContext": text}}));
    }
    Ok(())
}

/// The record's contribution to a thread's live state, or nothing when the
/// record is missing or describes an earlier pane with the same id.
pub fn self_report(root: &Path, socket: &str, pane_id: &str, terminal_id: &str) -> Option<Record> {
    let record = load(root, socket, pane_id)?;
    if !terminal_id.is_empty() && !record.terminal_id.is_empty() && record.terminal_id != terminal_id {
        return None;
    }
    record.reported().then_some(record)
}

/// Drops records whose pane is no longer listed in the session they belong to.
/// Only records of `socket` are judged: other sessions' records are theirs.
pub fn prune(root: &Path, socket: &str, live_pane_ids: &[String]) {
    for record in all(root) {
        if record.socket == socket && !live_pane_ids.contains(&record.pane_id) {
            remove(root, &record.socket, &record.pane_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runner::fake::{FakeRunner, ok};
    use serde_json::json;

    #[test]
    fn activity_is_cleaned_and_capped_at_forty_columns() {
        assert_eq!(clean("  Reading code\n", 40), "Reading code");
        assert_eq!(clean(&"x".repeat(60), 40).len(), 40);
        assert_eq!(clean("a\u{202e}b\u{7}c", 40), "abc");
        assert_eq!(clean("日本語テキスト", 6), "日本語");
    }

    #[test]
    fn helpers_and_the_reporters_own_call_do_not_trigger() {
        assert!(!eligible(&json!({"hook_event_name":"PostToolUse","agent_id":"child"})));
        assert!(!eligible(&json!({"hook_event_name":"PostToolUse","transcript_path":"/x/subagents/y.jsonl"})));
        assert!(!eligible(&json!({"hook_event_name":"PostToolUse","tool_input":{"command":"/p/herdr-projects --root /r report --percent 5 --activity x"}})));
        assert!(eligible(&json!({"hook_event_name":"PostToolUse","tool_input":{"command":"/p/herdr-projects --root /r context demo"}})));
        assert!(eligible(&json!({"hook_event_name":"SessionStart","source":"compact"})));
        assert!(!eligible(&json!({"hook_event_name":"Stop"})));
    }

    #[test]
    fn the_omp_extensions_hook_payloads_are_eligible() {
        assert!(eligible(&json!({"hook_event_name":"SessionStart"})));
        assert!(eligible(&json!({"hook_event_name":"UserPromptSubmit"})));
        assert!(eligible(&json!({"hook_event_name":"PostToolUse","tool_input":{"command":"bash cargo test -p demo"}})));
        assert!(!eligible(&json!({"hook_event_name":"PostToolUse","tool_input":{"command":"bash /p/herdr-projects --root /r report --percent 40 --activity x"}})));
    }

    #[test]
    fn session_start_injects_instructions_and_resets_the_record() {
        let mut record = Record { pane_id: "w1:p1".into(), agent: "omp".into(), activity: "Old".into(), percent: Some(50), reported_at: 5, channel: CHANNEL_OMP.into(), channel_seen: 990, ..Record::default() };
        let text = respond(&mut record, "SessionStart", "/p/hp --root /r", 1000).unwrap();
        assert!(text.contains("/p/hp --root /r report --percent 25"));
        assert!(text.contains("(w1:p1)"));
        assert_eq!(record.activity, "");
        assert_eq!(record.percent, None);
        assert_eq!(record.reported_at, 0);
        assert_eq!(record.session_started_at, 1000);
        assert_eq!((record.channel.as_str(), record.channel_seen), (CHANNEL_OMP, 990), "a new OMP session keeps the extension's channel");

        // Another harness started in the pane the extension pulled from: its
        // prompts must not wait for an extension that is gone.
        let mut claude = Record { pane_id: "w1:p1".into(), agent: "claude".into(), channel: CHANNEL_OMP.into(), channel_seen: 990, ..Record::default() };
        respond(&mut claude, "SessionStart", "hp", 1000);
        assert_eq!((claude.channel.as_str(), claude.channel_seen), ("", 0));
    }

    #[test]
    fn reminders_are_throttled_to_once_a_minute_and_stop_at_done() {
        let mut record = Record { reminded_at: 1000, ..Record::default() };
        assert!(respond(&mut record, "PostToolUse", "hp", 1030).is_none());
        assert!(respond(&mut record, "PostToolUse", "hp", 1061).is_some());
        assert_eq!(record.reminded_at, 1061);
        // A fresh report also quiets the reminder for a minute.
        record.reported_at = 1100;
        assert!(respond(&mut record, "PostToolUse", "hp", 1130).is_none());
        assert!(respond(&mut record, "PostToolUse", "hp", 1200).is_some());
        record.percent = Some(100);
        assert!(respond(&mut record, "PostToolUse", "hp", 9000).is_none());
        assert!(respond(&mut record, "UserPromptSubmit", "hp", 9001).unwrap().contains("Waiting for you"));
        let mut asked = Record { activity: WAITING.into(), percent: Some(40), reported_at: 5, ..Record::default() };
        respond(&mut asked, "UserPromptSubmit", "hp", 10);
        assert!(!asked.waiting(), "an answer clears the old question");
        assert!(respond(&mut record, "Stop", "hp", 9002).is_none());
    }

    #[test]
    fn records_round_trip_per_session_and_stale_terminals_are_ignored() {
        let root = tempfile::tempdir().unwrap();
        let record = Record { socket: "/a.sock".into(), pane_id: "w1:p1".into(), terminal_id: "term_1".into(), activity: WAITING.into(), reported_at: 7, ..Record::default() };
        save(root.path(), &record).unwrap();
        let other = Record { socket: "/b.sock".into(), pane_id: "w1:p1".into(), terminal_id: "term_9".into(), activity: "Testing".into(), reported_at: 8, ..Record::default() };
        save(root.path(), &other).unwrap();
        assert_eq!(load(root.path(), "/a.sock", "w1:p1").unwrap(), record);
        assert!(self_report(root.path(), "/a.sock", "w1:p1", "term_1").unwrap().waiting());
        assert!(self_report(root.path(), "/a.sock", "w1:p1", "term_2").is_none());
        assert!(self_report(root.path(), "/a.sock", "w1:p1", "").is_some());
        prune(root.path(), "/a.sock", &["w1:p2".into()]);
        assert!(load(root.path(), "/a.sock", "w1:p1").is_none());
        assert!(load(root.path(), "/b.sock", "w1:p1").is_some());
    }

    #[test]
    fn records_written_before_channels_existed_still_load() {
        let old = r#"{"socket":"/a.sock","pane_id":"w1:p1","terminal_id":"t1","agent":"claude","activity":"Testing","percent":40,"reported_at":7,"reminded_at":6,"session_started_at":5}"#;
        let record: Record = serde_json::from_str(old).unwrap();
        assert_eq!((record.activity.as_str(), record.percent, record.reported_at), ("Testing", Some(40), 7));
        assert_eq!((record.channel.as_str(), record.channel_seen), ("", 0));
        let saved = serde_json::to_value(Record { channel: CHANNEL_OMP.into(), channel_seen: 9, ..record }).unwrap();
        assert_eq!((saved["channel"].as_str(), saved["channel_seen"].as_i64()), (Some(CHANNEL_OMP), Some(9)));
    }

    #[test]
    fn heartbeats_mark_the_channel_and_never_touch_the_report() {
        let root = tempfile::tempdir().unwrap();
        let report = Record { socket: "/a.sock".into(), pane_id: "w1:p1".into(), terminal_id: "t1".into(), agent: "omp".into(), activity: "Testing".into(), percent: Some(40), reported_at: 900, reminded_at: 950, session_started_at: 800, ..Record::default() };
        save(root.path(), &report).unwrap();
        touch_channel(root.path(), "/a.sock", "w1:p1", "t1", "other", 1000).unwrap();
        let touched = load(root.path(), "/a.sock", "w1:p1").unwrap();
        assert_eq!(touched, Record { channel: CHANNEL_OMP.into(), channel_seen: 1000, ..report.clone() }, "only the channel fields change");
        // A pull that does not know its terminal keeps the stored one.
        touch_channel(root.path(), "/a.sock", "w1:p1", "", "other", 1001).unwrap();
        assert_eq!(load(root.path(), "/a.sock", "w1:p1").unwrap().terminal_id, "t1");

        // A new terminal with this pane id (herdr restarted): its id wins, so
        // items queued now are its own, and the old terminal's report goes.
        touch_channel(root.path(), "/a.sock", "w1:p1", "t2", "omp", 1002).unwrap();
        let restarted = load(root.path(), "/a.sock", "w1:p1").unwrap();
        assert_eq!((restarted.terminal_id.as_str(), restarted.agent.as_str(), restarted.channel_seen), ("t2", "omp", 1002));
        assert!(!restarted.reported());

        // No record yet: the heartbeat creates one bound to the pane, with no report.
        touch_channel(root.path(), "/a.sock", "w1:p2", "t3", "omp", 1000).unwrap();
        let fresh = load(root.path(), "/a.sock", "w1:p2").unwrap();
        assert_eq!((fresh.terminal_id.as_str(), fresh.agent.as_str()), ("t3", "omp"));
        assert!(!fresh.reported());
        assert!(self_report(root.path(), "/a.sock", "w1:p2", "t3").is_none());
    }

    #[test]
    fn a_channel_is_fresh_for_ten_seconds_after_the_last_pull() {
        let root = tempfile::tempdir().unwrap();
        assert!(!channel_fresh(root.path(), "/a.sock", "w1:p1", 1000), "no record");
        save(root.path(), &Record { socket: "/a.sock".into(), pane_id: "w1:p1".into(), reported_at: 999, ..Record::default() }).unwrap();
        assert!(!channel_fresh(root.path(), "/a.sock", "w1:p1", 1000), "reports alone are no channel");
        touch_channel(root.path(), "/a.sock", "w1:p1", "", "omp", 1000).unwrap();
        assert!(channel_fresh(root.path(), "/a.sock", "w1:p1", 1000 + CHANNEL_FRESH_SECS));
        assert!(!channel_fresh(root.path(), "/a.sock", "w1:p1", 1001 + CHANNEL_FRESH_SECS));
        assert!(!channel_fresh(root.path(), "/b.sock", "w1:p1", 1000), "another session's pane");
    }

    #[test]
    fn a_report_keeps_the_channel() {
        let home = tempfile::tempdir().unwrap();
        let env = Env::for_test(home.path(), &[("HERDR_ENV", "1"), ("HERDR_PANE_ID", "w1:p1"), ("HERDR_SOCKET_PATH", "/a.sock")]);
        let runner = FakeRunner::new();
        runner.on("pane current", ok(r#"{"result":{"pane":{"pane_id":"w1:p1","terminal_id":"t1","agent":"omp"}}}"#));
        runner.on("report-metadata", ok(r#"{"result":{}}"#));
        let root = home.path().join("root");
        let ctx = Ctx { env: &env, root: root.clone(), config_dir: home.path().join("cfg"), runner: &runner, detached_ticker: false };
        touch_channel(&root, "/a.sock", "w1:p1", "t1", "omp", 1000).unwrap();
        report(&ctx, Some(30), "Reading code").unwrap();
        let record = load(&root, "/a.sock", "w1:p1").unwrap();
        assert_eq!((record.activity.as_str(), record.percent), ("Reading code", Some(30)));
        assert!(record.reported());
        assert_eq!((record.channel.as_str(), record.channel_seen), (CHANNEL_OMP, 1000));
    }
}
