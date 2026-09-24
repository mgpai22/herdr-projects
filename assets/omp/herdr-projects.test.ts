// @ts-nocheck
import { afterAll, afterEach, beforeEach, expect, setSystemTime, test } from "bun:test";
import { chmodSync, existsSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

const dir = mkdtempSync(join(tmpdir(), "hp-omp-ext-"));
const binary = join(dir, "herdr-projects");
const log = join(dir, "calls.log");
// Logs "argv<TAB>stdin" per call and answers from canned files: hook-<Event>.json
// for `hook`, pull.json (consumed once, else pull-default.json) for `channel pull`,
// which sleeps 1 s while pull-slow exists.
writeFileSync(
  binary,
  `#!/bin/sh
dir=$(dirname "$0")
input=$(cat)
printf '%s\\t%s\\n' "$*" "$input" >> "$dir/calls.log"
case "$*" in
  *"hook --agent omp"*) f="$dir/hook-$(printf '%s' "$input" | sed -n 's/.*"hook_event_name":"\\([A-Za-z]*\\)".*/\\1/p').json" ;;
  *"channel pull"*) [ -f "$dir/pull-slow" ] && sleep 1; f="$dir/pull.json"; [ -f "$f" ] || f="$dir/pull-default.json" ;;
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
const todoEntry = (phases) => ({ type: "message", message: { role: "toolResult", toolName: "todo", isError: false, details: { op: "done", phases } } });
const pull = (items, claimed = true, file = "pull.json") => writeFileSync(join(dir, file), JSON.stringify({ claimed, items }));
const pulls = () => calls().filter(([args]) => args === "--root /r channel pull --agent omp").length;
const acks = () => calls().map(([args]) => args).filter((args) => args.startsWith("--root /r channel ack"));

// Hooks run outside the extension's call queue in a real child process and expose
// no promise, so a test can only poll for their effect (bounded, 3 s).
async function until(condition: () => unknown, what: string) {
  for (let i = 0; i < 300; i += 1) {
    const value = condition();
    if (value) return value;
    await Bun.sleep(10);
  }
  throw new Error(`timed out waiting for ${what}`);
}

let now = 1_000_000;
function advance(ms) {
  now += ms;
  setSystemTime(new Date(now));
}

beforeEach(() => {
  rmSync(log, { force: true });
  for (const name of ["hook-SessionStart.json", "hook-UserPromptSubmit.json", "hook-PostToolUse.json", "pull.json", "pull-default.json", "pull-slow"]) rmSync(join(dir, name), { force: true });
  // A claimed pane pulls every tick, so awaiting a tick settles earlier reports.
  pull([], true, "pull-default.json");
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
  ctx.entries = [todoEntry(todo("completed", "pending"))];
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

test("the poll seeing a todo result already applied does not undo a Done held by the throttle", async () => {
  const { ctx, emit, tick } = await load();
  emit("session_start");
  const phases = todo("completed", "completed");
  emit("tool_result", todoResult(phases));
  ctx.entries.push(todoEntry(phases));
  await tick();
  advance(500);
  emit("agent_end", { messages: [] });
  advance(2000);
  await tick();
  advance(3000);
  await tick();
  expect(reports()).toEqual(["--root /r report --percent=99 --activity=Working", "--root /r report --percent=100 --activity=Done"]);
});

test("channel items: an idle session gets only the first item as a prompt; the rest follow on the next tick", async () => {
  const { ctx, sent, emit, tick } = await load();
  emit("session_start");
  pull([{ id: "1-a", kind: "brief", text: "first" }, { id: "2-b", kind: "nudge", text: "second" }]);
  await tick();
  expect(sent).toEqual([["first"]]);
  expect(acks()).toEqual(["--root /r channel ack 1-a"]);
  ctx.idle = false;
  pull([{ id: "2-b", kind: "nudge", text: "second" }]);
  await tick();
  expect(sent).toEqual([["first"], ["second", { deliverAs: "followUp" }]]);
  expect(acks()).toEqual(["--root /r channel ack 1-a", "--root /r channel ack 2-b"]);
});

test("channel items: an item whose ack failed is acked again, never delivered twice", async () => {
  const { ctx, sent, emit, tick } = await load();
  emit("session_start");
  ctx.idle = false;
  // The fake ack removes nothing, like a failed one: the next pull returns the item again.
  pull([{ id: "1-a", kind: "brief", text: "only" }], true, "pull-default.json");
  await tick();
  await tick();
  expect(sent).toEqual([["only", { deliverAs: "followUp" }]]);
  expect(acks()).toEqual(["--root /r channel ack 1-a", "--root /r channel ack 1-a"]);
  // Once a pull no longer returns it, the id is forgotten.
  pull([]);
  await tick();
  await tick();
  expect(sent).toEqual([["only", { deliverAs: "followUp" }], ["only", { deliverAs: "followUp" }]]);
});

test("a pane no project claims pulls at most every 30 s; a claimed one every tick", async () => {
  const { emit, tick } = await load();
  emit("session_start");
  pull([], false, "pull-default.json");
  await tick();
  advance(2000);
  await tick();
  expect(pulls()).toBe(1);
  advance(28_000);
  await tick();
  expect(pulls()).toBe(2);
  pull([], true, "pull-default.json");
  advance(2000);
  await tick();
  expect(pulls()).toBe(2);
  advance(28_000);
  await tick();
  advance(2000);
  await tick();
  advance(2000);
  await tick();
  expect(pulls()).toBe(5);
});

test("channel items: a failed pull keeps claimed and delivered, so an item whose ack failed is sent once", async () => {
  const { ctx, sent, emit, tick } = await load();
  emit("session_start");
  ctx.idle = false;
  // The fake ack removes nothing, like a failed one; an empty answer is a failed pull.
  pull([{ id: "1-a", kind: "brief", text: "only" }]);
  await tick();
  writeFileSync(join(dir, "pull.json"), "");
  await tick();
  pull([{ id: "1-a", kind: "brief", text: "only" }]);
  advance(2000);
  await tick();
  expect(sent).toEqual([["only", { deliverAs: "followUp" }]]);
  expect(pulls()).toBe(3);
  expect(acks()).toEqual(["--root /r channel ack 1-a", "--root /r channel ack 1-a"]);
});

test("a failed first pull retries on the next tick instead of waiting 30 s", async () => {
  const { emit, tick } = await load();
  emit("session_start");
  writeFileSync(join(dir, "pull.json"), "");
  await tick();
  advance(2000);
  await tick();
  expect(pulls()).toBe(2);
});

test("channel items: a busy session queues every item as a follow-up; nothing pulled means no ack", async () => {
  const { ctx, sent, emit, tick } = await load();
  emit("session_start");
  ctx.idle = false;
  pull([{ id: "1-a", kind: "brief", text: "only" }]);
  await tick();
  pull([]);
  await tick();
  expect(sent).toEqual([["only", { deliverAs: "followUp" }]]);
  expect(pulls()).toBe(2);
  expect(acks()).toEqual(["--root /r channel ack 1-a"]);
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

test("a PostToolUse reminder is appended once to the next tool result, and the hook runs at most every 20 s", async () => {
  writeFileSync(join(dir, "hook-PostToolUse.json"), hookOutput("PostToolUse", "REMINDER"));
  const { emit, tick } = await load();
  emit("session_start");
  const out = [{ type: "text", text: "out" }];
  expect(emit("tool_result", { toolName: "bash", isError: false, input: { command: "ls -la" }, content: out })).toBeUndefined();
  // Results inside the 20 s window start no hook and carry nothing until the first hook answers.
  const result = await until(() => emit("tool_result", { toolName: "bash", isError: false, input: { command: "pwd" }, content: out }), "the reminder");
  expect(result).toEqual({ content: [...out, { type: "text", text: "REMINDER" }] });
  expect(emit("tool_result", { toolName: "bash", isError: false, input: { command: "pwd" }, content: out })).toBeUndefined();

  const postToolUse = () => calls().filter(([, stdin]) => stdin?.includes("PostToolUse")).map(([, stdin]) => stdin);
  expect(postToolUse()).toEqual(['{"hook_event_name":"PostToolUse","tool_input":{"command":"bash ls -la"}}']);
  advance(20_000);
  emit("tool_result", { toolName: "bash", isError: false, input: { command: "pwd" }, content: [] });
  await until(() => postToolUse().length >= 2, "the second PostToolUse hook");
  expect(postToolUse()).toHaveLength(2);
});

test("a PostToolUse reminder skips error results and waits for the next successful one", async () => {
  writeFileSync(join(dir, "hook-PostToolUse.json"), hookOutput("PostToolUse", "REMINDER"));
  const { emit } = await load();
  emit("session_start");
  const out = [{ type: "text", text: "out" }];
  emit("tool_result", { toolName: "bash", isError: false, input: { command: "ls" }, content: out });
  // Both in one synchronous step: the reminder cannot arrive between them.
  const result = await until(() => {
    expect(emit("tool_result", { toolName: "bash", isError: true, input: {}, content: out })).toBeUndefined();
    return emit("tool_result", { toolName: "bash", isError: false, input: {}, content: out });
  }, "the reminder");
  expect(result).toEqual({ content: [...out, { type: "text", text: "REMINDER" }] });
});

test("a pending PostToolUse reminder is dropped when the next prompt starts", async () => {
  writeFileSync(join(dir, "hook-PostToolUse.json"), hookOutput("PostToolUse", "REMINDER"));
  const { emit } = await load();
  emit("session_start");
  emit("tool_result", { toolName: "bash", isError: false, input: { command: "ls" }, content: [] });
  await until(() => calls().some(([, stdin]) => stdin?.includes("PostToolUse")), "the PostToolUse hook");
  // A real delay: a pending reminder is invisible until a successful result
  // takes it, so no signal says the hook child's answer has been stored.
  await Bun.sleep(300);
  await emit("before_agent_start", { prompt: "next" });
  expect(emit("tool_result", { toolName: "bash", isError: false, input: {}, content: [] })).toBeUndefined();
});

test("a SessionStart text is kept when a session switch happens while a prompt awaits the old one", async () => {
  writeFileSync(join(dir, "hook-SessionStart.json"), hookOutput("SessionStart", "INSTRUCTIONS"));
  const { emit } = await load();
  emit("session_start");
  const first = emit("before_agent_start", { prompt: "hi" });
  emit("session_switch", { reason: "new" });
  await first;
  expect(await emit("before_agent_start", { prompt: "again" })).toEqual({ message: { customType: "herdr-projects", content: "INSTRUCTIONS", display: false } });
});

test("a session switch resets the record only after reports the old session queued", async () => {
  const { emit, tick } = await load();
  emit("session_start");
  writeFileSync(join(dir, "pull-slow"), "");
  const ticked = tick();
  // Queued behind the slow pull, like a Done flushed by the agent_end of the run /new aborts.
  emit("tool_result", todoResult(todo("completed", "in_progress")));
  emit("session_switch", { reason: "new" });
  await ticked;
  const order = () => calls().map(([args, stdin]) => (args.startsWith("--root /r report") ? "report" : stdin?.includes("SessionStart") ? "start" : "")).filter(Boolean);
  await until(() => order().length >= 3, "the second SessionStart");
  expect(order()).toEqual(["start", "report", "start"]);
});

test("a reload during a run skips SessionStart; compaction sends the instructions again with the next prompt", async () => {
  writeFileSync(join(dir, "hook-SessionStart.json"), hookOutput("SessionStart", "INSTRUCTIONS"));
  const { ctx, emit } = await load();
  ctx.idle = false;
  emit("session_start");
  expect(await emit("before_agent_start", { prompt: "hi" })).toBeUndefined();
  emit("session_compact", { compactionEntry: {}, fromExtension: false });
  expect(await emit("before_agent_start", { prompt: "again" })).toEqual({ message: { customType: "herdr-projects", content: "INSTRUCTIONS", display: false } });
  expect(calls().filter(([, stdin]) => stdin?.includes("SessionStart")).map(([, stdin]) => stdin)).toEqual(['{"hook_event_name":"SessionStart","source":"compact"}']);
});

test("a prompt's hooks do not wait behind a slow pull", async () => {
  writeFileSync(join(dir, "hook-UserPromptSubmit.json"), hookOutput("UserPromptSubmit", "PROMPT"));
  const { emit, tick } = await load();
  emit("session_start");
  writeFileSync(join(dir, "pull-slow"), "");
  let pulled = false;
  const ticked = tick().then(() => (pulled = true));
  expect(await emit("before_agent_start", { prompt: "hi" })).toEqual({ message: { customType: "herdr-projects", content: "PROMPT", display: false } });
  expect(pulled).toBe(false);
  await ticked;
});

test("compaction resets the record, so the current todo state is reported again", async () => {
  const { emit, tick } = await load();
  emit("session_start");
  emit("tool_result", todoResult(todo("completed", "in_progress")));
  await tick();
  advance(3000);
  emit("session_compact", { compactionEntry: {}, fromExtension: false });
  await emit("before_agent_start", { prompt: "go on" });
  await tick();
  expect(reports()).toEqual(["--root /r report --percent=50 --activity=task 1", "--root /r report --percent=50 --activity=task 1"]);
});
