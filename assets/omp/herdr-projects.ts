// HERDR_PROJECTS_OMP_VERSION=1
// Installed by `herdr-projects configure`; `unconfigure` removes it, `doctor --fix` rewrites it.
// Connects the pane's root OMP session to herdr-projects: instructions and
// reminders (`hook`), progress from the todo list (`report`), and prompts
// queued for this pane (`channel pull/ack`), so a brief never types into the editor.
// An item is acked once OMP has it: a queued follow-up the user restores to the
// editor with Esc (or drops with /new) is theirs to keep or discard.
// @ts-nocheck

import { spawn } from "node:child_process";

const BINARY = "__HP_BINARY__";
const ROOT = "__HP_ROOT__";

const WAITING = "Waiting for you";
const TICK_MS = 2000;
const REPORT_MS = 2000;
const POST_TOOL_MS = 20_000;
// A pane no project claims pulls rarely: nothing is routed to it until it is claimed.
const UNCLAIMED_PULL_MS = 30_000;
const RUN_TIMEOUT_MS = 5000;

// OMP marks every shell it spawns with OMPCODE=1: a nested `omp` started from a
// session's shell is not the pane's agent and must stay silent.
const ENABLED = process.env.HERDR_ENV === "1" && !!process.env.HERDR_SOCKET_PATH && !!process.env.HERDR_PANE_ID && process.env.OMPCODE !== "1";

// One binary call at a time: reports must reach the record in order. Hooks
// bypass it so a prompt never waits behind a slow pull.
let queue: Promise<unknown> = Promise.resolve();

function run(args: string[], input = ""): Promise<string> {
  const next = queue.then(() => spawnOnce(args, input));
  queue = next;
  return next;
}

function spawnOnce(args: string[], input: string): Promise<string> {
  const { promise, resolve } = Promise.withResolvers<string>();
  let out = "";
  let child;
  try {
    child = spawn(BINARY, ["--root", ROOT, ...args], { stdio: ["pipe", "pipe", "ignore"], timeout: RUN_TIMEOUT_MS });
  } catch {
    resolve("");
    return promise;
  }
  child.on("error", () => resolve(""));
  child.on("close", (code) => resolve(code === 0 ? out : ""));
  child.stdout.setEncoding("utf8");
  child.stdout.on("data", (chunk) => (out += chunk));
  child.stdin.on("error", () => {});
  child.stdin.end(input);
  return promise;
}

async function hook(name: string, extra: Record<string, unknown> = {}): Promise<string> {
  const out = await spawnOnce(["hook", "--agent", "omp"], JSON.stringify({ hook_event_name: name, ...extra }));
  try {
    const text = JSON.parse(out)?.hookSpecificOutput?.additionalContext;
    return typeof text === "string" ? text : "";
  } catch {
    return "";
  }
}

// Mirrors OMP's getLatestTodoPhasesFromEntries (tools/todo.ts): the newest todo
// tool result, or the `user_todo_edit` entry the eval bridge persists for `tool.todo`.
function entryPhases(entry): unknown[] | undefined {
  if (entry?.type === "custom" && entry.customType === "user_todo_edit") {
    return Array.isArray(entry.data?.phases) ? entry.data.phases : undefined;
  }
  const message = entry?.type === "message" ? entry.message : undefined;
  if (message?.role !== "toolResult" || message.toolName !== "todo" || message.isError || message.details?.op === "view") return undefined;
  return Array.isArray(message.details?.phases) ? message.details.phases : undefined;
}

function latestPhases(ctx): unknown[] | undefined {
  let entries;
  try {
    entries = ctx?.sessionManager?.getBranch?.() ?? ctx?.sessionManager?.getEntries?.();
  } catch {
    return undefined;
  }
  if (!Array.isArray(entries)) return undefined;
  for (let i = entries.length - 1; i >= 0; i -= 1) {
    const phases = entryPhases(entries[i]);
    if (phases) return phases;
  }
  return undefined;
}

type Progress = { percent: number | undefined; activity: string; complete: boolean };

// Undefined for an empty list: nothing to derive progress from.
function progressOf(phases: unknown[]): Progress | undefined {
  const tasks = phases.flatMap((phase) => (Array.isArray(phase?.tasks) ? phase.tasks : []));
  if (tasks.length === 0) return undefined;
  const counted = tasks.filter((task) => task?.status !== "abandoned");
  const done = counted.filter((task) => task?.status === "completed").length;
  const complete = counted.length > 0 && done === counted.length;
  // 100 means Done; the list alone never says the turn has finished, so it caps at 99.
  const percent = counted.length === 0 ? undefined : Math.min(99, Math.round((100 * done) / counted.length));
  const current = tasks.find((task) => task?.status === "in_progress") ?? tasks.find((task) => task?.status === "pending");
  const activity = typeof current?.content === "string" && current.content.trim() ? current.content : "Working";
  return { percent, activity, complete };
}

function toolSummary(event): string {
  const input = event?.input ?? {};
  let detail;
  try {
    detail = typeof input.command === "string" ? input.command : JSON.stringify(input);
  } catch {
    detail = "";
  }
  return `${event?.toolName ?? "tool"} ${detail}`.slice(0, 500);
}

export default function (pi) {
  if (!ENABLED) return;

  let latestCtx;
  let timer;
  let ticking = false;
  let startText: Promise<string> | undefined;
  let reminder: string | undefined;
  let postToolAt = 0;
  // Todo state this process has seen change; phases present at session start
  // are a baseline, so a resumed or finished list never reports by itself.
  let progress: Progress | undefined;
  let polledKey: string | undefined;
  let blocked = 0;
  let finished = false;
  let reportedKey: string | undefined;
  let reportedAt = 0;
  let claimed = false;
  let pulledAt: number | undefined;
  // Delivered but maybe not acked (the ack can fail): never delivered twice.
  const delivered = new Set<string>();

  // Only the pane's root session (the one with a UI) talks to herdr-projects.
  function root(ctx): boolean {
    if (ctx?.hasUI !== true) return false;
    latestCtx = ctx;
    if (!timer && typeof ctx.setInterval === "function") timer = ctx.setInterval(() => tick(), TICK_MS);
    return true;
  }

  function startSession(ctx, instructions = true) {
    progress = undefined;
    blocked = 0;
    finished = false;
    reportedKey = undefined;
    reminder = undefined;
    const phases = latestPhases(ctx);
    polledKey = phases ? JSON.stringify(phases) : undefined;
    startText = instructions ? hook("SessionStart") : undefined;
  }

  // Deduped by content: the poll later sees the same list the tool_result
  // already applied, and must not undo a Done reported in between.
  function setPhases(phases: unknown[]) {
    const key = JSON.stringify(phases);
    if (key === polledKey) return;
    polledKey = key;
    progress = progressOf(phases);
    finished = false;
    flush();
  }

  function poll() {
    const phases = latestPhases(latestCtx);
    if (phases) setPhases(phases);
  }

  function desired(): { percent: number | undefined; activity: string } | undefined {
    if (!progress) return undefined;
    if (blocked > 0) return { percent: progress.percent, activity: WAITING };
    if (finished) return { percent: 100, activity: "Done" };
    return progress;
  }

  // Change-only, at most one report per REPORT_MS; the tick sends what was held back.
  function flush() {
    const next = desired();
    if (!next) return;
    const key = `${next.percent ?? "?"}\0${next.activity}`;
    if (key === reportedKey || Date.now() - reportedAt < REPORT_MS) return;
    reportedKey = key;
    reportedAt = Date.now();
    const percent = next.percent === undefined ? "--unknown" : `--percent=${next.percent}`;
    void run(["report", percent, `--activity=${next.activity}`]);
  }

  async function tick() {
    if (ticking) return;
    ticking = true;
    try {
      poll();
      flush();
      const now = Date.now();
      if (!claimed && pulledAt !== undefined && now - pulledAt < UNCLAIMED_PULL_MS) return;
      pulledAt = now;
      let pulled;
      try {
        pulled = JSON.parse(await run(["channel", "pull", "--agent", "omp"]));
      } catch {
        return;
      }
      if (typeof pulled?.claimed !== "boolean" || !Array.isArray(pulled.items)) return;
      claimed = pulled.claimed;
      const items = pulled.items.filter((item) => typeof item?.id === "string" && typeof item.text === "string");
      const ids = new Set(items.map((item) => item.id));
      for (const id of delivered) if (!ids.has(id)) delivered.delete(id);
      const ack: string[] = [];
      for (const item of items) {
        if (delivered.has(item.id)) {
          ack.push(item.id);
          continue;
        }
        // An idle session starts a turn with the first item and nothing else:
        // a follow-up sent in the same tick would start first (OMP's prompt path
        // awaits longer than its follow-up path). The rest wait for the next tick.
        const idle = latestCtx?.isIdle?.() === true;
        try {
          if (idle) pi.sendUserMessage(item.text);
          else pi.sendUserMessage(item.text, { deliverAs: "followUp" });
        } catch {
          break;
        }
        delivered.add(item.id);
        ack.push(item.id);
        if (idle) break;
      }
      if (ack.length > 0) await run(["channel", "ack", ...ack]);
    } finally {
      ticking = false;
    }
  }

  function block(delta: number) {
    blocked = Math.max(0, blocked + delta);
    flush();
  }

  // A reload mid-run re-inits the extension: SessionStart would reset the
  // record and repeat the instructions of a running session.
  pi.on("session_start", (_event, ctx) => {
    if (root(ctx)) startSession(ctx, ctx.isIdle?.() !== false);
  });

  pi.on("session_switch", (_event, ctx) => {
    if (root(ctx)) startSession(ctx);
  });

  pi.on("before_agent_start", async (_event, ctx) => {
    if (!root(ctx)) return undefined;
    // Captured before awaiting: a session switch meanwhile sets the new session's text.
    const pending = startText;
    startText = undefined;
    const start = pending ? await pending : "";
    const prompt = await hook("UserPromptSubmit");
    const content = [start, prompt].filter(Boolean).join("\n\n");
    if (!content) return undefined;
    return { message: { customType: "herdr-projects", content, display: false } };
  });

  // Compaction drops the early instructions: send them again with the next prompt.
  // The hook resets the record, so the current state must be reported again.
  pi.on("session_compact", (_event, ctx) => {
    if (!root(ctx)) return;
    startText = hook("SessionStart", { source: "compact" });
    void startText.then(() => {
      reportedKey = undefined;
      flush();
    });
  });

  pi.on("tool_result", (event, ctx) => {
    if (!root(ctx)) return undefined;
    const details = event?.details;
    if (event?.toolName === "todo" && !event.isError && details?.op !== "view" && Array.isArray(details?.phases)) setPhases(details.phases);
    // A reminder from an earlier PostToolUse rides on this result, so it
    // reaches the main turn without a user message or a per-request hook.
    const text = reminder;
    reminder = undefined;
    const now = Date.now();
    if (now - postToolAt >= POST_TOOL_MS) {
      postToolAt = now;
      void hook("PostToolUse", { tool_input: { command: toolSummary(event) } }).then((next) => {
        if (next) reminder = next;
      });
    }
    if (!text) return undefined;
    const content = Array.isArray(event?.content) ? event.content : [];
    return { content: [...content, { type: "text", text }] };
  });

  pi.on("tool_execution_start", (event, ctx) => {
    if (event?.toolName === "ask" && root(ctx)) block(1);
  });

  pi.on("tool_execution_end", (event, ctx) => {
    if (event?.toolName === "ask" && root(ctx)) block(-1);
  });

  pi.on("tool_approval_requested", (_event, ctx) => {
    if (root(ctx)) block(1);
  });

  pi.on("tool_approval_resolved", (_event, ctx) => {
    if (root(ctx)) block(-1);
  });

  pi.on("agent_start", (_event, ctx) => {
    if (root(ctx)) finished = false;
  });

  pi.on("agent_end", (event, ctx) => {
    if (!root(ctx) || event?.willContinue === true || !progress?.complete) return;
    finished = true;
    flush();
  });

  pi.on("session_shutdown", () => {
    // OMP clears ctx timers on shutdown.
    timer = undefined;
  });
}
