# Operations and development

How Herdr Projects works, what it writes where, what its safety settings do and don't stop, and how to run threads on other machines.

## How it works

- **It relies on Herdr and nothing else.** No other plugin is needed or called. Pull requests open in your browser, text files open in a new Herdr tab running `$EDITOR`.
- **A project is a folder.** `~/.herdr-projects/<slug>/` holds `AGENTS.md`, which tells any agent started in that folder that it is the coordinator and which commands to run. `CLAUDE.md` is a link to it. Several coordinators can share the folder.
- **The coordinator is an ordinary agent** following a skill (`herdr-projects skill` prints it). Plugin code does not route messages, plan work or decide anything.
- **The binary does mechanics.** Starting a thread, copying reports, cleaning up after a resolve: each is one deterministic subcommand. It talks to Herdr through Herdr's CLI. The exception is the agent view (`focus`, `unfocus`, the default sort): Herdr 0.9.1 has no CLI for `agent.view.set`, so those send one JSON line to the socket.
- **Agents report their own progress.** `herdr-projects report --percent N --activity "..."`, run by the agent in its pane, writes one small JSON file per pane under `<root>/.progress/` and sets the `hp_activity` sidebar token for five minutes. Hooks in Claude Code and Codex, and an extension in OMP (installed by `configure`), inject the instructions and a reminder. There is no daemon and no database.
- **Files are the record, prompts are nudges.** Threads write a report file, the ticker writes events to an inbox folder, and the coordinator reads state with `context` at the start of every turn. A missed prompt loses nothing.
- **One ticker per projects root** checks every 15 seconds: coordinators (any agent in a project folder, also one started by hand in a project never opened), thread state and groups, sidebar tokens, pending prompts, changed reports, pull requests (every two minutes), routines, auto-resolve, notifications. Remote machines are polled once a minute.
- **Tools are found even under a bare `PATH`.** The binary appends `/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin` and `~/.cargo/bin` to its own `PATH`, so a ticker started by Herdr finds `gh` and `rsync`.
- **Cleanup is part of the flow, never forced.** Resolving a thread removes its worktree (Herdr and git refuse a dirty one, and the plugin never forces) and, once its pull request is merged, its local branch. Reports and library files always stay. Text from reports, pull requests and command output is never placed in a prompt.

## Where things live

```
~/.herdr-projects/<project>/
  PROJECT.md              settings (TOML between +++ lines) and your standing instructions
  AGENTS.md, CLAUDE.md    who is the coordinator, by working directory; written by the binary
  MEMORY.md, memory/      project memory; the coordinator's
  TASKS.md                the task list; the coordinator's
  routines/<name>.md      routines, including pr-followup.md; the coordinator's
  uploads/                files you give the threads
  scratch/                the coordinator's temporary files
  threads/<id>.toml       thread record          threads/<id>.md       home copy of its report
  threads/<id>.task.md    the task and every forwarded prompt (## Follow-ups)
  threads/<id>.next.md    Next lines the coordinator added    threads/<id>/  a tab thread's folder
  inbox/, inbox/done/     events for the coordinator
  library/<id>/           home copy of files a thread produced
  .state/                 status, coordinator record, live coordinators, ticker state, lock
  .omp/config.yml         an OMP coordinator's approval rules; written by the binary (see OMP)
  .mstack/config.yml      turns mstack mode on for an OMP coordinator with mstack 0.4.0+ (see OMP)
~/.herdr-projects/.ticker.lock  .ticker.log  .progress/  .channel/  .trash/
~/.config/herdr-projects/config.toml             yours: root, safety tables, machines
~/.config/herdr-projects/owned.json              what `configure` changed, for `unconfigure`
~/.config/herdr-projects/approved-routines.json  written only by `routine approve`
```

Every thread works from `<its working directory>/.herdr-project/<project>-<id>/`: `brief.md` (written by the binary), `report.md` and `library/` (written by the agent). In a git repository that folder is in `info/exclude`, so nothing in it is committed. Git therefore treats it as clean and removing a worktree deletes it, which is why a resolve keeps the worktree when the final copy home was partial.

`PROJECT.md` settings, all changeable from the popup's settings section, from chat, or with `herdr-projects set <project> <key> <value>`: `name` (the workspace label), `goal`, `repos` (`repos.add PATH[@MACHINE]`, `repos.remove PATH`), `coordinator_agent` and `thread_agent` (the default Herdr agent kinds), `omp_profile` (the OMP profile for OMP coordinators and threads; empty = default, see OMP), `max_parallel_threads` (3), `auto_resolve_days` (7), `nudge` (`true`), `mute` (`false`).

## Commands

| Command | What it does |
| --- | --- |
| `new <name> [--goal] [--repo PATH[@MACHINE]]...` | Create a project folder. |
| `open <project> [--agent KIND] [--agent-arg A]... [--new] [--tab] [--session N \| --socket P] [--rebind]` | A coordinator agent in the project folder; focuses a running one. From a shell pane inside Herdr it runs in that pane and quitting it returns to the shell; `--tab`, the popup, actions and a terminal outside Herdr use a tab of the project's workspace. |
| `context <project> [--peek]` | The digest the coordinator reads every turn. |
| `coordinator prompt <project> --text-file F` | A sentence to the coordinator (the popup's task keys use it). |
| `thread start <project> --title T [--repo PATH] [--kind worktree\|tab\|checkout] [--agent KIND] [--agent-arg A]... [--machine M] [--base REF] --task-file F` | New thread; `-` reads the task from standard input. `--agent-arg` takes only a model flag (see below). |
| `thread prompt`, `thread next [--line N \| --add TEXT]`, `thread stop`, `thread restart [--agent KIND] [--agent-arg A]...` | Steer a thread. Prompts are recorded in its task file. |
| `thread list/show [--json]`, `thread ack`, `thread adopt` | Look at threads. |
| `thread resolve [--keep-worktree] [--discard-uncopied] [--skip-copy] [--reopen]` | Final copy home, then clean up. |
| `sweep <project> [--dry-run] [--yes]` | Remove what nothing uses any more. |
| `set <project> <key> <value>`, `routine list/toggle/approve`, `safety show` | Settings and routines. |
| `pause`, `resume`, `archive`, `unarchive`, `delete [--force]` | Project lifecycle. |
| `popup [project]`, `focus [project]`, `unfocus`, `overview [project]`, `needs-you --line` | Views. |
| `configure [--clients claude,codex,omp] [--key K] [--hooks-only] [--dry-run]`, `unconfigure`, `report`, `progress` | Sidebar, keys, hooks, the OMP extension, the `autoproject` skill, self-reports. |
| `open-file <path>`, `open-url <url>` | Open a text file in a new tab with `$EDITOR`, or a PR in the browser. |
| `ticker start \| run \| stop \| status`, `doctor [--fix]`, `skill` | Housekeeping. |
| `update [--check]` | Update to the newest release: fetch, rebuild, `doctor --fix`, restart the ticker. Refuses on an OMP fork build (see OMP). |

## Groups

Every thread is in one group, shown in the sidebar, the popup and the digest, needs-you first:

1. **Waiting on you** (`needs you`): a failed start, a pane that is gone before a report, a launch stuck on a dialog, the agent blocked on a question or permission for 30 seconds, or the agent's own report `Waiting for you` while it is not working.
2. **Ready for review** (`review`): a report you haven't acknowledged, or a report with an open pull request, while the agent is not working.
3. **Landing**: an open pull request that is approved.
4. **Working**: the agent works, a launch is under way, or the agent reported progress under 100% in the last five minutes.
5. **Idle**, then **Resolved**.

Threads idle for `auto_resolve_days` are resolved (and cleaned) after a final copy home.

## The popup

`prefix+a` (or the **Projects** action) opens it, scoped to the current workspace's project: the coordinator's workspace or a thread's, found by where its panes work. From any section, `P` opens a project picker with All projects first and the current scope highlighted: `↑`/`↓` (or `k`/`j`) move, `↵` switches, `esc` closes (archived projects are skipped; `↵` on a settings project row still jumps there). In the picker `/` filters by name or slug as you type; `esc` clears the filter, then closes. `/` in any section opens the picker straight into the filter. Outside a project it opens on all projects. Every key runs a CLI command; the popup can do nothing the CLI cannot.

| Section | Keys |
| --- | --- |
| threads | `↵` jump to the pane · `1`-`9` send that Next line to the thread · `s` stop (Escape) · `r` restart with a kind picker · `a` ack · `x` resolve · `o` open the PR · `i` detail (report, Next list, files: `↵` opens, `y` copies the path) · `c` start or focus a coordinator of a chosen kind · `S` sweep |
| tasks | `↵` jump to the delegated thread · `d` delegate · `m` done · `D` drop (each sends a sentence to the coordinator, which stays the only writer of TASKS.md) |
| inbox | `↵` detail · `a` done |
| routines | `↵` enable or disable · `i` the prompt |
| settings | `↵` edit · `p` pause or resume · `A` archive · `X` delete (asks first) |
| memory | `↵` read (change memory by asking the coordinator) |

## Safety settings

Set per project in `~/.config/herdr-projects/config.toml`; `safety show <project>` prints the table header to use.

```toml
[safety."/Users/you/.herdr-projects/billing"]
start_threads = "propose"          # or "auto": the coordinator starts threads without asking
coordinator_agent_args = []        # extra arguments for every coordinator's agent CLI
thread_agent_args = []             # extra arguments for every thread's agent CLI
routine_commands = false           # true lets approved routines run shell commands
```

`--agent-arg` on `open`, `thread start` and `thread restart` is for the model only: `--model NAME` or `--model=NAME` for every harness, plus `-m NAME` for Codex. Anything else is refused with the table above, because the coordinator sets `--agent-arg` and must never be able to widen an agent's powers (`--dangerously-skip-permissions`, `--yolo`). Other launch flags go in `thread_agent_args` and `coordinator_agent_args`, which only you set. The ticker checks a thread's stored arguments again at launch: any that are not a model flag are dropped and reported in one inbox item.

## The allow-list for your coordinator

The coordinator runs the binary every turn, so allow-list it in your agent by subcommand, never the bare binary. `context` prints the exact prefix (`Commands: <binary> --root <root>`); the patterns must start with it. For Claude Code, in the project folder's `.claude/settings.local.json`:

```json
{ "permissions": { "allow": [
  "Bash(<binary> --root <root> skill:*)",
  "Bash(<binary> --root <root> context:*)",
  "Bash(<binary> --root <root> report:*)",
  "Bash(<binary> --root <root> inbox done:*)",
  "Bash(<binary> --root <root> list:*)",
  "Bash(<binary> --root <root> routine list:*)",
  "Bash(<binary> --root <root> thread list:*)",
  "Bash(<binary> --root <root> thread show:*)",
  "Bash(<binary> --root <root> thread prompt:*)",
  "Bash(<binary> --root <root> thread next:*)",
  "Bash(<binary> --root <root> thread ack:*)",
  "Bash(<binary> --root <root> thread restart:*)"
] } }
```

- Allow `thread start` only where you've set `start_threads = "auto"`. Left off the list, every thread start meets your agent's own permission prompt.
- Never allow `thread resolve`, `sweep`, `delete`, `archive`, `routine approve`, `configure` or `unconfigure`.
- An OMP coordinator gets these rules from the project's `.omp/config.yml` (see OMP).

## What the safety settings do and don't stop

- **They are soft.** Agents have a shell. The guards are the skill text, your agent's permission prompts, keeping `config.toml` and approvals outside every agent's working directory, and `routine approve` refusing without a terminal and a typed confirmation.
- **A thread can impersonate you.** Any thread agent can prompt the coordinator's pane through Herdr. The skill's rule that a go-ahead must name the threads lowers the risk; it does not remove it.
- **An approved routine command covers the command text only.** `./check.sh` keeps its hash while the script changes.
- **Prompt injection is reduced, not removed.** No GitHub text reaches a prompt from the plugin, but threads read pull request comments themselves with `gh`, and memory is inlined into every later brief.
- **Hooks run in every agent session on the machine.** They exit at once outside a Herdr pane.
- **Cost.** Every thread is a full agent session, and each nudge and each `context` spends coordinator tokens.

## Nudges and notifications

- **Notifications** go out once per event, titled `<Project> · <thread>`: `needs you · ...` with Herdr's request sound; a new report or a merged pull request with the done sound; failed checks, review activity and due routines without sound. `mute = true` silences a project except for errors (a broken routine file, `gh` failing for ten minutes).
- **Nudges** (`nudge = true`, the default for new projects) prompt a coordinator with `[hp ticker] new inbox items, run context` once a set of new inbox items arrives. On Herdr 0.9.1 a prompt merges with text you have half-typed, so the ticker only prompts a coordinator whose state has not changed and been idle for 60 seconds, and picks the one that changed most recently when several qualify. `nudge = false` turns this off; notifications still come.

## Routines

A file `routines/<name>.md` with TOML front matter; the body is the prompt.

- `schedule = "every <N>m|h|d"` or `"daily HH:MM"` (local time): the coordinator gets the body as an inbox item when it is due; while that item is unhandled, later runs add none. With no coordinator running (any agent in the project folder counts), a due run does nothing, runs no command and is not made up later; `routine list`, the popup and `doctor` show it as `skipped: no coordinator`. `routine list` shows each routine's last and next run. An optional `command` runs (`sh -c`, in the project folder, 60 second timeout) only when `routine_commands = true` and you have run `herdr-projects routine approve <project> <name>` in a terminal; its output reaches the coordinator capped at 4,000 characters inside a fence labelled as untrusted.
- `on = "pr"`, optionally `events = ["opened", "checks-failed", "review", "merged"]`: fired by the ticker's pull request poll. The body goes to the thread whose pull request changed, as a prompt, with facts the binary generates (how many checks fail, how many comments, the `gh` commands to read them). It needs no coordinator, only the open thread.
- Every project has `routines/pr-followup.md` (`checks-failed`, `review`): it tells the thread to fix failing checks and address review comments, and ends with an `Authorized:` line that lets the thread push to its own branch and comment on that pull request, and nothing else (mstack in an OMP thread refuses a push without such a line). Turn it off in the popup's routines section; `doctor --fix` puts it back if the file is missing. A copy that is exactly an earlier default, apart from its `enabled` line, is replaced by `doctor --fix`, which keeps your `enabled` value; a copy you edited is left alone, and `doctor` says when it has no `Authorized:` line, unless it is turned off.

## Cleanup

- **Resolve on merge**: the ticker resolves a thread whose pull request merged only when its agent is neither working nor waiting on you, and it has written a report since the merge or 10 minutes have passed. A thread that merged its own pull request can still tag, deploy and write its final report.
- **Resolve** (popup `x`, chat, or `thread resolve`) copies the report and library home, then removes the worktree with `herdr worktree remove --workspace` (which also closes the workspace) or `git worktree remove` and `git worktree prune`, deletes the local branch if the pull request is merged (it asks GitHub once more first, and finds a pull request by the thread's branch when the report has no `PR:` line), and closes a tab thread's tab. Once no open thread uses the repository's primary workspace that Herdr grouped the worktree under, and it has no agent and only idle, unfocused shells, it is closed too, with one inbox item; the ticker retries one it had to keep. An adopted pane is left alone. The inbox item lists what was removed and what was kept, and why.
- **Merged pull requests and auto-resolve** clean up the same way.
- **Sweep** lists and removes worktrees on `hp/<project>/` branches with no open thread, branches of resolved threads whose pull request merged, tabs of resolved threads, working folders of tab threads resolved longer than `auto_resolve_days` ago, empty repository workspaces its threads' worktrees were grouped under, and handled inbox items older than 30 days. `doctor` shows the same list. Sweep covers local repositories.
- **Archive** closes the project's workspace and its threads' workspaces and hides the project; nothing is deleted, and `unarchive` reopens it. Archive never closes a repository's primary workspace.
- **Delete** moves the folder to `.trash/`.

## Threads on other machines

Save the machine with `herdr machine add --label <label> <ssh target>` (both machines need Herdr 0.9.1), then list a repo as `/path/on/machine@<label>` or pass `thread start --machine <label>`. The home machine owns the project; only outbound SSH from home is needed, in batch mode.

- The worktree, the brief and the report live on the remote machine. The home ticker polls it once a minute and copies a changed report with `scp` and the thread's `library/` with `rsync -rt` (symbolic links are never followed; a library over 50 MB is not copied).
- Remote threads get no self-reports: `report` writes on the machine where the agent runs. Their group comes from the agent state Herdr detects and from their pull request.
- A machine that doesn't answer is left alone: no state is read, threads keep their last group, and after ten minutes you get one `outage` inbox item, and one more when it is back.
- Tasks with no repository always run locally, as tabs.

## Laptop-closed operation

Install Herdr and this plugin on an always-on machine, keep the projects root there, open the project there, and attach from your laptop with `herdr --remote <ssh target>` (add `--session <name>` for a named session). The ticker runs on that machine. If Herdr asks whether to restart a remote server "that may not survive SSH connection loss", answering `n` keeps its panes.

## OMP

This fork (`mgpai22/herdr-projects`, branch `omp`, versions `0.2.13-omp.N`) adds support for [OMP](https://github.com/can1357/oh-my-pi) as a coordinator and thread harness.

**Install.** The fork has no release binaries, so build it from source in a linked checkout:

```bash
git clone -b omp https://github.com/mgpai22/herdr-projects.git ~/dev/herdr-projects-omp
herdr plugin link ~/dev/herdr-projects-omp
cd ~/dev/herdr-projects-omp && HERDR_PROJECTS_BUILD=source sh scripts/install.sh
herdr-projects configure --clients omp        # or claude,codex,omp
```

`herdr-projects update` refuses on a fork build. To update, run `git -C ~/dev/herdr-projects-omp pull`, then `HERDR_PROJECTS_BUILD=source sh scripts/install.sh` in the checkout, then `herdr-projects doctor --fix`, `herdr-projects ticker stop` and `herdr-projects ticker start`.

**Windows.** The fork runs natively on Windows 11 with Git for Windows installed. In PowerShell, after `git clone` and `herdr plugin link` as above, build with `powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install.ps1` in the checkout (Herdr runs the same script as the plugin's build step; it always builds `target\release\herdr-projects.exe` with `cargo build --release --locked`). To update, run `git pull`, the same script, then `herdr-projects doctor --fix`, `herdr-projects ticker stop` and `herdr-projects ticker start`. The ticker and other running copies do not block the build: the script renames the running `herdr-projects.exe` to `herdr-projects.exe.<number>.old` first (Windows cannot overwrite a running program but can rename it), and deletes those copies on a later run once nothing runs them. If the build fails, the old binary is put back.

The script refuses to build when `assets\omp` or `skill\` has Windows (CRLF) line endings, because the binary embeds those files byte for byte. A clone made with `core.autocrlf=true` before the fork added `.gitattributes` has them. The script prints the files and the one-time fix: delete them and check them out again with `git checkout -- <files>`, which rewrites them with LF endings and discards local edits to them.

Differences from Linux and macOS:

- Command routines, the coordinator's printed commands and the Claude Code hooks run under Git Bash (`bash.exe` on `PATH`, else `C:\Program Files\Git\bin\bash.exe`; WSL's `System32\bash.exe` is never used). The binary and root appear with `/` separators (`C:/Users/...`), so set OMP's shell to Git Bash for the `.omp/config.yml` rules to match.
- A command routine that times out is killed with every program it started, also one it detached: Git Bash starts programs with the request to leave the routine's Windows job whenever the job allows that, so the job allows no program to leave.
- `configure` installs no Codex hooks: Codex runs hooks through the user's own shell (cmd, PowerShell or Git Bash), and no single hook command works in all of them. Codex still gets the skill link.
- The tab-bar entry runs under `cmd.exe`, so `configure` writes it with double quotes.
- `open` in the current pane finds the agent the way `cmd.exe` does (each `PATH` folder, each `PATHEXT` extension), so npm-installed `claude.cmd` and `codex.cmd` start.
- Links are symbolic links (Developer Mode on). Without the symbolic-link privilege, `CLAUDE.md` is a copy of `AGENTS.md`, refreshed on each write, and a skill link is a directory junction.
- A thread's library is copied in process: Windows has no `du` or `rsync`. Threads on saved machines still need those machines to be POSIX hosts.
- `delete` cannot move a project folder while a process has its current directory or an open file in it, as the project's panes do. It retries for about two seconds, then names the panes to close. `delete --force` closes the project's live panes first.
- The popup's `y` copies with `clip.exe`.
- `HOME` wins when set; otherwise the home folder is `USERPROFILE`.

**What `configure` installs.** `configure` picks OMP by itself when an OMP agent folder exists. It installs into every OMP profile: the default profile's agent folder is `$PI_CODING_AGENT_DIR` when that is set, else `~/.omp/agent`; a named profile's is `~/.omp/profiles/<name>/agent`, one for each such folder. OMP layouts moved to XDG folders are not supported. If you set `PI_CODING_AGENT_DIR`, set the same value in the shell where you run `configure` and in the Herdr server's environment: the Herdr **doctor** and **configure** actions run with the server's environment, and with a different value they look in another folder and install a second copy there. A profile created later gets its copy from the next `configure`.

- `<agent folder>/extensions/herdr-projects.ts` in each profile, with this binary's path and root written into it. `unconfigure` removes it only when it is unchanged. `doctor` reports each profile's copy (`omp extension <profile>`) as ok, outdated, missing or foreign; `doctor --fix` rewrites it only when `configure` installed it for this root; a copy left over from `unconfigure`, or one installed for another root, is reported with the command to run. A file of that name that is not ours is never touched.
- `<agent folder>/skills/autoproject` in each profile, a link to the `autoproject` skill. It is skipped when `~/.agents/skills/autoproject` already links the same skill, because OMP reads that folder too, in every profile.

**Profiles.** A project's `omp_profile` setting (in `PROJECT.md`, `set`, or the popup's settings) picks the OMP profile its OMP coordinator and threads start with; empty, or `default`, is OMP's default profile. `open --profile <name>`, `thread start --profile <name>` and `thread restart --profile <name>` override it for one coordinator or thread, and the thread keeps its profile for later restarts; `--profile ""` goes back to the project's setting. `--profile` with another harness is an error. With a named profile, `open` always starts the coordinator in a new tab, never in the pane it runs from. The profile must exist in Herdr's `[session.omp_launchers]` on the machine that runs the agent, for example:

```toml
[session.omp_launchers]
default = "/home/me/.local/bin/omp"
neurable = "/home/me/.local/bin/omp-neurable"
```

A named profile needs a Herdr build with `herdr agent start --profile` (the OMP profile-recovery build, newer than 0.9.1-custom.afd9e19893db.293f3bc6a84a), and the running Herdr server must be that build too: after you install it, restart or hand off the server, or Herdr refuses with `agent_profile_unsupported`. The default profile works with any Herdr. When Herdr refuses a profile (an unknown or invalid name, a server or CLI without `--profile`, or a pane that came up under another profile), `open` restores the previous coordinator record, so its session can still resume (a first `open` leaves no record), and exits with Herdr's message; a thread fails at once with Herdr's message as its error. A pane that came up under another profile already runs that agent, so the binary closes the pane first. `adopt` records the profile Herdr reports for the adopted pane.

**What the extension does.** It runs in every OMP session and does nothing outside a Herdr pane. In the main session, not in subagents:

- **Hooks.** It runs `herdr-projects hook --agent omp`, the entry point the Claude Code and Codex hooks use, and gives the model the same progress instructions and reminders. The reminder after tool use comes at most once every 20 seconds.
- **Progress from the todo list.** When the agent keeps a todo list, the extension reports the percentage of tasks done (abandoned ones do not count) and the current task as the activity. It reports `Waiting for you` while the agent asks you something or waits for an approval, and `100` with `Done` when every task is done and the agent stops. It reports only changes, at most once every 2 seconds. An agent with no todo list reports by hand as before. Both kinds of report go to the same record, and the newest one wins.
- **Channel delivery.** Every 2 seconds the extension fetches prompts that the binary queued for its pane (`channel pull`), gives them to the agent as user messages (as a follow-up while it works), and confirms them (`channel ack`). The binary queues a brief, a nudge, a `thread prompt` or a `coordinator prompt` under `<root>/.channel/` in place of typing it into the pane when that pane's extension checked in during the last 10 seconds. A queued prompt never merges with text you have half-typed, and it reaches an agent that waits for an approval once you answer. The extension confirms a prompt as soon as it hands it to OMP, so a queued follow-up that you pull back into the editor with Esc is yours to keep or delete: it is not sent again. The ticker types an item that nothing picks up in 60 seconds with `herdr agent prompt`, as before. Threads on other machines always get typed prompts.

**The coordinator's approval rules.** Every project folder has `.omp/config.yml`, written by `new`, `open` and `doctor --fix`. OMP reads it only in a session whose working directory is exactly the project folder, so it applies to the coordinator and not to threads. It holds even when your own OMP config sets `approvalMode: yolo`:

| Command | OMP |
| --- | --- |
| `routine approve`, `configure`, `unconfigure` | refused (`deny`); the coordinator tells you the command to run |
| `thread resolve`, `sweep`, `archive`, `delete` | asks you to confirm in the coordinator's pane (`prompt`) |
| the `eval` and `debug` tools | ask you to confirm each call, because both can start a process that the patterns above do not see |

The first line of the file is `# herdr-projects: managed`, and `doctor --fix` rewrites such a file when it is out of date, for example after the binary or the root moved. To keep your own version, delete that line: the binary then never writes the file again. A file it cannot read is left alone too, and `doctor` names it, also after `--fix`; fix its permissions or remove it.

OMP replaces a list setting as a whole, so in the coordinator's session this file's `bash.patterns` would take the place of the `bash.patterns` in the coordinator profile's own OMP config (`<agent folder>/config.yml`, or `config.yaml` when there is no `config.yml`). The file therefore repeats those rules, so they keep applying. OMP uses the first rule that matches, and the file lists them in this order: the project's `deny` rules, the profile's `deny` rules, the project's confirmations, then the profile's whole list. So `<binary> --root <root> thread resolve ... && gh pr merge 42` is still refused when the profile denies `gh pr merge`; the one cost is that a profile `allow` placed before a broader profile `deny` no longer makes an exception to it. The coordinator profile is the profile the project's recorded coordinator runs under when that coordinator is OMP (`open` and the ticker record it), else the project's `omp_profile`. One folder has one such file, so two OMP coordinators of different profiles (`open --new`) share the rules of the one recorded last. When you change that config, or the coordinator's profile changes, `doctor` shows the file as out of date and `doctor --fix` (or the next `open`) rewrites it. The binary reads that config as OMP does: a duplicated key keeps its last value, and `<<` merge keys are resolved. When the config cannot be read or used, or the profile name is invalid, the file holds only the project's rules and says so in a comment; `doctor` names the problem on its own line and tells you to fix the config at its path, or to use another profile: `open <project> --profile <name>` while an OMP coordinator is recorded (its profile wins over the setting, so `set` would change nothing), else `set <project> omp_profile <name>`.

Each rule starts with the exact `<binary> --root <root>` prefix the coordinator is told to use, for example `<binary> --root <root> configure *`. A slug such as `configure-ci` or a brief that mentions "delete" does not match. OMP also checks each part of a compound command, so `cd x && <binary> --root <root> sweep` still asks. The rules are advice, not a sandbox: another path to the binary, a copied or renamed binary, `sh -c '...'` and similar wrappers get past them. When the binary or root path contains a space, the prefix is quoted and the check of the parts of a compound command does not match it. A subagent of the coordinator cannot answer a confirmation, so `eval`, `debug` and the confirmed commands are refused there.

**Threads.** Start an OMP thread with `--agent omp` and pick its model with `--agent-arg --model=<provider/model>[:<level>]`, for example `--agent-arg --model=anthropic/claude-opus-4-5:high`. Without mstack, the prompt that starts an OMP thread contains the word workflowz, which turns on OMP's workflow notice for that turn, and the brief tells the thread to run work with several independent slices as an `eval` `workpool()`.

**mstack.** The binary looks for the [mstack](https://github.com/mgpai22/mstack) OMP plugin in each profile's plugins folder (`~/.omp/plugins`, or `~/.omp/profiles/<name>/plugins`; OMP installs plugins per profile), and counts it only when it is enabled there. `doctor` shows what it found for each profile. When a local OMP thread's profile has mstack, the prompt that starts it says to route the task with `skill://mstack-mode` (falling back to `skill://mstack-figure-it-out`) in place of the workflowz text; remote threads keep the workflowz text. The coordinator's instructions tell it to plan with mstack and give each thread one worktree or folder to write, and, for every harness, to end every task that may push or open a pull request with an `Authorized:` line naming the branch and the target. mstack never treats its own mode as permission to push. When the coordinator profile (see above) has mstack 0.4.0 or newer, the project folder also gets `.mstack/config.yml` with `mode: true`, because a prompt the binary queues can not run `/mstack on`, and `promotion.directory: scratch/mstack`, so the plans, decisions and ledgers mstack skills write go to the coordinator's `scratch/` and not into `.mstack/`. It follows the same rules as `.omp/config.yml`: first line `# herdr-projects: managed`, delete that line to keep your own version. When the coordinator profile no longer has mstack 0.4.0 or newer, `doctor` reports `.mstack/config.yml is no longer wanted`, and `doctor --fix` (or the next `open` or `set`) deletes a managed copy, because an older mstack refuses the `mode` key; a copy without the first line is left alone.

## Development

```bash
cargo test                       # unit tests and scenarios against a scripted fake runner
scripts/dev-server               # a throwaway `hp-dev` Herdr session with a scratch root
scripts/dev-hp <subcommand>      # the binary against <repo>/.dev-root; pass --session hp-dev to open/doctor
scripts/dev-herdr <args>         # herdr against that session
HERDR_CONFIG_PATH=<copy> ...     # point configure and `herdr config check` at a scratch config
```

Never develop against your default session, `~/.herdr-projects` or your real `config.toml`. [`herdr-notes.md`](herdr-notes.md) records what was verified about Herdr, and [`manual-test.md`](manual-test.md) lists the acceptance checks, including the visual ones only a person can confirm.
