// @ts-nocheck
import { afterAll, afterEach, beforeEach, expect, setSystemTime, test } from "bun:test";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const dir = mkdtempSync(join(tmpdir(), "hp-omp-ext-"));
const binary = join(dir, "herdr-projects");
const log = join(dir, "calls.log");
// Logs "argv<TAB>stdin" per call and answers from canned files: hook-<Event>.json
// for `hook`, pull.json (consumed once) for `channel pull`.
writeFileSync(
  binary,
  `#!/bin/sh
dir=$(dirname "$0")
input=$(cat)
printf '%s\\t%s\\n' "$*" "$input" >> "$dir/calls.log"
case "$*" in
  *"hook --agent omp"*) f="$dir/hook-$(printf '%s' "$input" | sed -n 's/.*"hook_event_name":"\\([A-Za-z]*\\)".*/\\1/p').json" ;;
  *"channel pull"*) f="$dir/pull.json" ;;
  *) f="" ;;
esac
if [ -n "$f" ] && [ -f "$f" ]; then cat "$f"; case "$f" in */pull.json) rm -f "$f" ;; esac; fi
`,
);
chmodSync(binary, 0o755);

const asset = readFileSync(join(import.meta.dir, "herdr-projects.ts"), "utf8");
let renders = 0;

async function load(env = { HERDR_ENV: "1", HERDR_SOCKET_PATH: "/tmp/h.sock", HERDR_PANE_ID: "w1:p1" }) {
  for (const key of ["HERDR_ENV", "HERDR_SOCKET_PATH", "HERDR_PANE_ID", "OMPCODE"]) delete process.env[key];
  Object.assign(process.env, env);
  const file = join(dir, `ext-${renders++}.ts`);
  writeFileSync(file, asset.replace('"__HP_BINARY__"', JSON.stringify(binary)).replace('"__HP_ROOT__"', JSON.stringify("/r")));
  // The module is rendered at runtime, like `configure` does, so it cannot be a static import.
  const factory = (await import(file)).default;
  const handlers = new Map();
  const sent = [];
  const pi = { on: (name, fn) => handlers.set(name, fn), sendUserMessage: (text, options) => sent.push(options ? [text, options] : [text]) };
  factory(pi);
  const ctx = {
    hasUI: true,
    idle: true,
    entries: [],
    ticks: [],
    isIdle: () => ctx.idle,
    sessionManager: { getBranch: () => ctx.entries },
    setInterval: (fn) => ctx.ticks.push(fn),
  };
  const emit = (name, event = {}, c = ctx) => handlers.get(name)?.({ type: name, ...event }, c);
  // A tick queues `channel pull` behind every earlier call, so awaiting it settles them.
  const tick = () => ctx.ticks[0]();
  return { handlers, sent, ctx, emit, tick };
}

function calls(): string[][] {
  if (!existsSync(log)) return [];
  return readFileSync(log, "utf8").trimEnd().split("\n").map((line) => line.split("\t"));
}

const reports = () => calls().map(([args]) => args).filter((args) => args.startsWith("--root /r report"));

const todo = (...statuses) => [{ name: "Build", tasks: statuses.map((status, i) => ({ content: `task ${i}`, status })) }];
const todoResult = (phases) => ({ toolName: "todo", isError: false, input: {}, details: { op: "done", phases } });

let now = 1_000_000;
function advance(ms) {
  now += ms;
  setSystemTime(new Date(now));
}

beforeEach(() => {
  rmSync(log, { force: true });
  for (const name of ["hook-SessionStart.json", "hook-UserPromptSubmit.json", "hook-PostToolUse.json", "pull.json"]) rmSync(join(dir, name), { force: true });
  advance(60_000);
});
afterEach(() => setSystemTime());
afterAll(() => rmSync(dir, { recursive: true, force: true }));

test("todo results report percent and activity, change-only and at most every 2 s", async () => {
  const { emit, tick } = await load();
  emit("session_start");
  emit("tool_result", todoResult(todo("completed", "in_progress", "pending", "pending")));
  await tick();
  expect(reports()).toEqual(["--root /r report --percent=25 --activity=task 1"]);

  // Same state again: nothing new.
  emit("tool_result", todoResult(todo("completed", "in_progress", "pending", "pending")));
  await tick();
  expect(reports()).toHaveLength(1);

  // A change inside the 2 s window is held, then sent by a later tick.
  advance(500);
  emit("tool_result", todoResult(todo("completed", "completed", "in_progress", "abandoned")));
  await tick();
  expect(reports()).toHaveLength(1);
  advance(2000);
  await tick();
  expect(reports()[1]).toBe("--root /r report --percent=67 --activity=task 2");
});

test("todo phases persisted by the eval bridge are picked up by the poll; the list at session start is a baseline", async () => {
  const { ctx, emit, tick } = await load();
  ctx.entries = [{ type: "message", message: { role: "toolResult", toolName: "todo", isError: false, details: { op: "done", phases: todo("completed", "pending") } } }];
  emit("session_start");
  await tick();
  expect(reports()).toEqual([]);

  ctx.entries.push({ type: "custom", customType: "user_todo_edit", data: { phases: todo("completed", "completed", "in_progress") } });
  await tick();
  expect(reports()).toEqual(["--root /r report --percent=67 --activity=task 2"]);
});

test("ask and approval prompts report Waiting for you, then restore the todo state", async () => {
  const { emit, tick } = await load();
  emit("session_start");
  emit("tool_result", todoResult(todo("completed", "in_progress")));
  advance(3000);
  emit("tool_execution_start", { toolName: "ask" });
  await tick();
  advance(3000);
  emit("tool_execution_end", { toolName: "ask" });
  await tick();
  advance(3000);
  emit("tool_approval_requested", { toolName: "bash" });
  await tick();
  advance(3000);
  emit("tool_approval_resolved", { toolName: "bash" });
  await tick();
  expect(reports()).toEqual([
    "--root /r report --percent=50 --activity=task 1",
    "--root /r report --percent=50 --activity=Waiting for you",
    "--root /r report --percent=50 --activity=task 1",
    "--root /r report --percent=50 --activity=Waiting for you",
    "--root /r report --percent=50 --activity=task 1",
  ]);
});

test("no todo list means no automatic reports, even for ask", async () => {
  const { emit, tick } = await load();
  emit("session_start");
  emit("tool_execution_start", { toolName: "ask" });
  emit("agent_end", { messages: [] });
  await tick();
  expect(reports()).toEqual([]);
});

test("a finished list reports 100 only when the agent ends without continuing", async () => {
  const { emit, tick } = await load();
  emit("session_start");
  emit("tool_result", todoResult(todo("completed", "abandoned", "completed")));
  await tick();
  expect(reports()).toEqual(["--root /r report --percent=99 --activity=Working"]);

  advance(3000);
  emit("agent_end", { messages: [], willContinue: true });
  await tick();
  expect(reports()).toHaveLength(1);

  emit("agent_end", { messages: [] });
  await tick();
  expect(reports()[1]).toBe("--root /r report --percent=100 --activity=Done");
});

test("channel items: an idle session gets the first as a prompt, the rest as follow-ups, then all are acked", async () => {
  const { sent, emit, tick } = await load();
  emit("session_start");
  writeFileSync(join(dir, "pull.json"), JSON.stringify([{ id: "1-a", kind: "brief", text: "first" }, { id: "2-b", kind: "nudge", text: "second" }]));
  await tick();
  expect(sent).toEqual([["first"], ["second", { deliverAs: "followUp" }]]);
  expect(calls().map(([args]) => args)).toContain("--root /r channel ack 1-a 2-b");
});

test("channel items: a busy session queues every item as a follow-up; nothing pulled means no ack", async () => {
  const { ctx, sent, emit, tick } = await load();
  emit("session_start");
  ctx.idle = false;
  writeFileSync(join(dir, "pull.json"), JSON.stringify([{ id: "1-a", kind: "brief", text: "only" }]));
  await tick();
  await tick();
  expect(sent).toEqual([["only", { deliverAs: "followUp" }]]);
  const args = calls().map(([a]) => a);
  expect(args.filter((a) => a === "--root /r channel pull --agent omp")).toHaveLength(2);
  expect(args.filter((a) => a.startsWith("--root /r channel ack"))).toEqual(["--root /r channel ack 1-a"]);
});

test("disabled outside a Herdr pane and in a nested omp", async () => {
  expect((await load({})).handlers.size).toBe(0);
  expect((await load({ HERDR_ENV: "1", HERDR_SOCKET_PATH: "/tmp/h.sock", HERDR_PANE_ID: "w1:p1", OMPCODE: "1" })).handlers.size).toBe(0);
});

test("sessions without a UI (subagents) stay silent", async () => {
  const { ctx, emit } = await load();
  ctx.hasUI = false;
  emit("session_start");
  expect(await emit("before_agent_start", { prompt: "hi" })).toBeUndefined();
  emit("tool_result", todoResult(todo("in_progress")));
  emit("tool_execution_start", { toolName: "ask" });
  expect(ctx.ticks).toHaveLength(0);
  expect(calls()).toEqual([]);
});

const hookOutput = (event, text) => JSON.stringify({ hookSpecificOutput: { hookEventName: event, additionalContext: text } });

test("SessionStart instructions arrive with the first prompt, UserPromptSubmit text with every prompt", async () => {
  writeFileSync(join(dir, "hook-SessionStart.json"), hookOutput("SessionStart", "INSTRUCTIONS"));
  writeFileSync(join(dir, "hook-UserPromptSubmit.json"), hookOutput("UserPromptSubmit", "PROMPT"));
  const { emit } = await load();
  emit("session_start");
  expect(await emit("before_agent_start", { prompt: "hi" })).toEqual({ message: { customType: "herdr-projects", content: "INSTRUCTIONS\n\nPROMPT", display: false } });
  expect(await emit("before_agent_start", { prompt: "again" })).toEqual({ message: { customType: "herdr-projects", content: "PROMPT", display: false } });
  expect(calls().filter(([args]) => args === "--root /r hook --agent omp").map(([, stdin]) => stdin)).toEqual([
    '{"hook_event_name":"SessionStart"}',
    '{"hook_event_name":"UserPromptSubmit"}',
    '{"hook_event_name":"UserPromptSubmit"}',
  ]);
});

test("a PostToolUse reminder is appended once to the next context, and the hook runs at most every 20 s", async () => {
  writeFileSync(join(dir, "hook-PostToolUse.json"), hookOutput("PostToolUse", "REMINDER"));
  const { emit, tick } = await load();
  emit("session_start");
  emit("tool_result", { toolName: "bash", isError: false, input: { command: "ls -la" }, content: [] });
  emit("tool_result", { toolName: "bash", isError: false, input: { command: "pwd" }, content: [] });
  await tick();
  const history = [{ role: "user", content: "hi", timestamp: 1 }];
  const result = await emit("context", { messages: history });
  expect(result.messages.slice(0, 1)).toEqual(history);
  expect(result.messages[1]).toMatchObject({ role: "user", content: [{ type: "text", text: "REMINDER" }] });
  expect(await emit("context", { messages: history })).toBeUndefined();

  const postToolUse = () => calls().filter(([, stdin]) => stdin?.includes("PostToolUse")).map(([, stdin]) => stdin);
  expect(postToolUse()).toEqual(['{"hook_event_name":"PostToolUse","tool_input":{"command":"bash ls -la"}}']);
  advance(20_000);
  emit("tool_result", { toolName: "bash", isError: false, input: { command: "pwd" }, content: [] });
  await tick();
  expect(postToolUse()).toHaveLength(2);
});
