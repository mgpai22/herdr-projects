// HERDR_PROJECTS_OMP_VERSION=1
// Installed by `herdr-projects configure`; `unconfigure` removes it, `doctor --fix` rewrites it.
// Connects the pane's root OMP session to herdr-projects: instructions and
// reminders (`hook`), progress from the todo list (`report`), and prompts
// queued for this pane (`channel pull/ack`), so a brief never types into the editor.
// @ts-nocheck

import { spawn } from "node:child_process";

const BINARY = "__HP_BINARY__";
const ROOT = "__HP_ROOT__";

const WAITING = "Waiting for you";
const TICK_MS = 2000;
const REPORT_MS = 2000;
const POST_TOOL_MS = 20_000;
const RUN_TIMEOUT_MS = 5000;

// OMP marks every shell it spawns with OMPCODE=1: a nested `omp` started from a
// session's shell is not the pane's agent and must stay silent.
const ENABLED = process.env.HERDR_ENV === "1" && !!process.env.HERDR_SOCKET_PATH && !!process.env.HERDR_PANE_ID && process.env.OMPCODE !== "1";

// One binary call at a time: reports must reach the record in order.
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
  const out = await run(["hook", "--agent", "omp"], JSON.stringify({ hook_event_name: name, ...extra }));
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

  // Only the pane's root session (the one with a UI) talks to herdr-projects.
  function root(ctx): boolean {
    if (ctx?.hasUI !== true) return false;
    latestCtx = ctx;
    if (!timer && typeof ctx.setInterval === "function") timer = ctx.setInterval(() => tick(), TICK_MS);
    return true;
  }

  function startSession(ctx) {
    progress = undefined;
    blocked = 0;
    finished = false;
    reportedKey = undefined;
    reminder = undefined;
    const phases = latestPhases(ctx);
    polledKey = phases ? JSON.stringify(phases) : undefined;
    startText = hook("SessionStart");
  }

  function setPhases(phases: unknown[]) {
    progress = progressOf(phases);
    finished = false;
    flush();
  }

  function poll() {
    const phases = latestPhases(latestCtx);
    const key = phases ? JSON.stringify(phases) : undefined;
    if (key === polledKey) return;
    polledKey = key;
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
      let items;
      try {
        items = JSON.parse(await run(["channel", "pull", "--agent", "omp"]));
      } catch {
        return;
      }
      if (!Array.isArray(items)) return;
      const delivered: string[] = [];
      for (const item of items) {
        if (typeof item?.id !== "string" || typeof item.text !== "string") continue;
        try {
          if (delivered.length === 0 && latestCtx?.isIdle?.() === true) pi.sendUserMessage(item.text);
          else pi.sendUserMessage(item.text, { deliverAs: "followUp" });
        } catch {
          break;
        }
        delivered.push(item.id);
      }
      if (delivered.length > 0) await run(["channel", "ack", ...delivered]);
    } finally {
      ticking = false;
    }
  }

  function block(delta: number) {
    blocked = Math.max(0, blocked + delta);
    flush();
  }

  pi.on("session_start", (_event, ctx) => {
    if (root(ctx)) startSession(ctx);
  });

  pi.on("session_switch", (_event, ctx) => {
    if (root(ctx)) startSession(ctx);
  });

  pi.on("before_agent_start", async (_event, ctx) => {
    if (!root(ctx)) return undefined;
    const start = startText ? await startText : "";
    startText = undefined;
    const prompt = await hook("UserPromptSubmit");
    const content = [start, prompt].filter(Boolean).join("\n\n");
    if (!content) return undefined;
    return { message: { customType: "herdr-projects", content, display: false } };
  });

  pi.on("context", (event, ctx) => {
    if (!reminder || !root(ctx)) return undefined;
    const text = reminder;
    reminder = undefined;
    const messages = Array.isArray(event?.messages) ? event.messages : [];
    return { messages: [...messages, { role: "user", content: [{ type: "text", text }], synthetic: true, timestamp: Date.now() }] };
  });

  pi.on("tool_result", (event, ctx) => {
    if (!root(ctx)) return undefined;
    const details = event?.details;
    if (event?.toolName === "todo" && !event.isError && details?.op !== "view" && Array.isArray(details?.phases)) setPhases(details.phases);
    const now = Date.now();
    if (now - postToolAt < POST_TOOL_MS) return undefined;
    postToolAt = now;
    void hook("PostToolUse", { tool_input: { command: toolSummary(event) } }).then((text) => {
      if (text) reminder = text;
    });
    return undefined;
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
