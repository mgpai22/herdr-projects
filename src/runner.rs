//! Every external command (herdr, git, gh, ssh, scp, rsync, sh) goes through `Runner`.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

#[derive(Debug, Clone, PartialEq)]
pub struct Cmd {
    pub program: String,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub env_remove: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub stdin: Option<String>,
    pub timeout: Duration,
    /// Spawn in its own process group and kill the whole group on timeout.
    pub own_group: bool,
}

impl Cmd {
    pub fn new(program: impl Into<String>, timeout: Duration) -> Self {
        Cmd {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            env_remove: Vec::new(),
            cwd: None,
            stdin: None,
            timeout,
            own_group: false,
        }
    }

    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    pub fn env_remove(mut self, key: impl Into<String>) -> Self {
        self.env_remove.push(key.into());
        self
    }

    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn stdin(mut self, text: impl Into<String>) -> Self {
        self.stdin = Some(text.into());
        self
    }

    pub fn own_group(mut self) -> Self {
        self.own_group = true;
        self
    }

    /// The command as one line; the scripted fake matches on it.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn display(&self) -> String {
        let mut line = self.program.clone();
        for arg in &self.args {
            line.push(' ');
            line.push_str(arg);
        }
        line
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Output {
    /// `None` when the process was killed (timeout or signal).
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

impl Output {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    /// stderr when it has text, else stdout, trimmed; for error messages.
    pub fn error_text(&self) -> String {
        if self.timed_out {
            return "timed out".to_string();
        }
        let text = if self.stderr.trim().is_empty() {
            self.stdout.trim()
        } else {
            self.stderr.trim()
        };
        text.to_string()
    }
}

pub trait Runner {
    /// `Err` means the command could not be spawned at all (for example the
    /// program is missing). A non-zero exit or a timeout is an `Ok(Output)`.
    fn run(&self, cmd: &Cmd) -> Result<Output>;

    /// One JSON line to a herdr socket, one line back. The single exception to
    /// "talk to herdr through its CLI" (client decision during the build):
    /// herdr 0.9.1 has no CLI command for `agent.view.set` / `agent.view.clear`.
    fn socket_request(&self, socket: &Path, line: &str, timeout: Duration) -> Result<String>;

    /// Runs `cmd` on this process's terminal (an agent `open` starts in its own
    /// pane) and waits for it; `cmd.timeout` and `cmd.stdin` are ignored.
    /// While it runs, `poll` is called about twice a second until it returns
    /// true. Returns the exit code, `None` when a signal ended it.
    fn run_foreground(&self, cmd: &Cmd, poll: &mut dyn FnMut() -> bool) -> Result<Option<i32>>;
}

pub struct RealRunner;

const POLL: Duration = Duration::from_millis(20);

impl Runner for RealRunner {
    fn run(&self, cmd: &Cmd) -> Result<Output> {
        let mut command = Command::new(&cmd.program);
        command.args(&cmd.args);
        for key in &cmd.env_remove {
            command.env_remove(key);
        }
        for (key, value) in &cmd.env {
            command.env(key, value);
        }
        if let Some(cwd) = &cmd.cwd {
            command.current_dir(cwd);
        }
        command
            .stdin(if cmd.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        if cmd.own_group {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        // No console window for a child of the windowless ticker; its own group
        // when asked, so the tree is killed as one.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW | if cmd.own_group { CREATE_NEW_PROCESS_GROUP } else { 0 });
        }

        let mut child = command
            .spawn()
            .with_context(|| format!("could not run `{}`", cmd.program))?;
        #[cfg(windows)]
        let job = if cmd.own_group { Job::assign(&child) } else { None };

        // Readers and the writer run on their own threads so a full pipe in
        // either direction cannot deadlock against the deadline loop below.
        let stdin_thread = child.stdin.take().zip(cmd.stdin.clone()).map(|(mut pipe, text)| {
            std::thread::spawn(move || {
                let _ = pipe.write_all(text.as_bytes());
            })
        });
        let stdout_thread = child.stdout.take().map(read_all);
        let stderr_thread = child.stderr.take().map(read_all);

        let deadline = Instant::now() + cmd.timeout;
        let mut timed_out = false;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break Some(status);
            }
            if Instant::now() >= deadline {
                timed_out = true;
                #[cfg(windows)]
                if let Some(job) = &job {
                    job.terminate();
                }
                kill(&mut child, cmd.own_group);
                break child.wait().ok();
            }
            std::thread::sleep(POLL);
        };

        if let Some(thread) = stdin_thread {
            let _ = thread.join();
        }
        let stdout = stdout_thread.map(join_text).unwrap_or_default();
        let stderr = stderr_thread.map(join_text).unwrap_or_default();

        Ok(Output {
            code: if timed_out {
                None
            } else {
                status.and_then(|s| s.code())
            },
            stdout,
            stderr,
            timed_out,
        })
    }

    fn socket_request(&self, socket: &Path, line: &str, timeout: Duration) -> Result<String> {
        socket_round_trip(socket, line, timeout)
    }

    fn run_foreground(&self, cmd: &Cmd, poll: &mut dyn FnMut() -> bool) -> Result<Option<i32>> {
        let mut command = Command::new(&cmd.program);
        command.args(&cmd.args);
        for key in &cmd.env_remove {
            command.env_remove(key);
        }
        for (key, value) in &cmd.env {
            command.env(key, value);
        }
        if let Some(cwd) = &cmd.cwd {
            // This process leads the pane's foreground group, and Herdr reports
            // the leader's directory as the pane's `foreground_cwd`: it moves too.
            std::env::set_current_dir(cwd).with_context(|| format!("could not enter {}", cwd.display()))?;
            command.current_dir(cwd);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("could not run `{}`", cmd.program))?;
        // Ctrl-C and Ctrl-\ reach the whole foreground group: they are the
        // agent's to handle, and this process must outlive it so the shell
        // does not take the terminal back from a running agent.
        let _ignored = IgnoreInterrupts::new();
        let mut polling = true;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if polling {
                polling = !poll();
            }
            std::thread::sleep(Duration::from_millis(500));
        };
        Ok(status.code())
    }
}

/// The POSIX shell that routines and generated scripts run under: `sh`, and on
/// Windows Git Bash (`bash.exe` on `PATH`, else Git's default install folder).
/// `System32\bash.exe` is WSL's launcher, a different machine: never used.
pub fn posix_shell() -> String {
    #[cfg(windows)]
    {
        let system = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into()).to_ascii_lowercase();
        let on_path = std::env::var_os("PATH").and_then(|path| {
            std::env::split_paths(&path)
                .filter(|dir| {
                    let dir = dir.to_string_lossy().to_ascii_lowercase();
                    !dir.starts_with(&system) && !dir.contains("windowsapps")
                })
                .map(|dir| dir.join("bash.exe"))
                .find(|bash| bash.is_file())
        });
        let program_files = std::env::var_os("ProgramFiles").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(r"C:\Program Files"));
        on_path.unwrap_or_else(|| program_files.join(r"Git\bin\bash.exe")).to_string_lossy().into_owned()
    }
    #[cfg(not(windows))]
    "sh".to_string()
}

/// Starts `command` so it outlives this process and its terminal. Unix: a new
/// session. Windows: no console, its own process group, and out of herdr's
/// pane job when the job allows breaking away.
pub fn spawn_detached(command: &mut Command) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe extern "C" {
            fn setsid() -> i32;
        }
        // SAFETY: setsid is async-signal-safe and touches no memory.
        unsafe {
            command.pre_exec(|| {
                setsid();
                Ok(())
            });
        }
        command.spawn().map(drop)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetStdHandle(which: u32) -> isize;
            fn SetHandleInformation(handle: isize, mask: u32, flags: u32) -> i32;
        }
        const DETACHED_PROCESS: u32 = 0x0000_0008;
        const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
        const HANDLE_FLAG_INHERIT: u32 = 1;
        // Windows hands a child every inheritable handle, and this process's
        // stdio (herdr's pipes for a plugin command) is: the detached child
        // would hold them open and the caller would wait for its exit. Later
        // `Stdio::inherit` children still get them (std duplicates the handle).
        for which in [-10i32, -11, -12] {
            // SAFETY: plain handle-flag calls on this process's std handles.
            unsafe { SetHandleInformation(GetStdHandle(which as u32), HANDLE_FLAG_INHERIT, 0) };
        }
        let flags = DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP;
        // A job without JOB_OBJECT_LIMIT_BREAKAWAY_OK refuses with access denied.
        match command.creation_flags(flags | CREATE_BREAKAWAY_FROM_JOB).spawn() {
            Ok(_) => Ok(()),
            Err(e) if e.raw_os_error() == Some(5) => command.creation_flags(flags).spawn().map(drop),
            Err(e) => Err(e),
        }
    }
}

#[cfg(windows)]
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[cfg(unix)]
unsafe extern "C" {
    fn signal(signum: i32, handler: usize) -> usize;
}

#[cfg(unix)]
const SIGINT: i32 = 2;
#[cfg(unix)]
const SIGQUIT: i32 = 3;
#[cfg(unix)]
const SIG_IGN: usize = 1;

/// SIGINT and SIGQUIT ignored in this process (set after the child's exec, so
/// the child keeps the default), restored on drop.
#[cfg(unix)]
struct IgnoreInterrupts(usize, usize);

#[cfg(unix)]
impl IgnoreInterrupts {
    fn new() -> Self {
        // SAFETY: plain signal(2) calls with the ignore disposition.
        unsafe { IgnoreInterrupts(signal(SIGINT, SIG_IGN), signal(SIGQUIT, SIG_IGN)) }
    }
}

#[cfg(unix)]
impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        // SAFETY: restores the dispositions `new` returned.
        unsafe {
            signal(SIGINT, self.0);
            signal(SIGQUIT, self.1);
        }
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(handler: Option<unsafe extern "system" fn(u32) -> i32>, add: i32) -> i32;
}

/// Ctrl-C and Ctrl-Break swallowed by this process while registered; the
/// handler is not inherited, so the already-started child keeps the default.
#[cfg(windows)]
struct IgnoreInterrupts;

#[cfg(windows)]
unsafe extern "system" fn swallow_interrupt(event: u32) -> i32 {
    // CTRL_C_EVENT = 0, CTRL_BREAK_EVENT = 1: handled (ignored). Close, logoff
    // and shutdown fall through to the default handler.
    (event <= 1) as i32
}

#[cfg(windows)]
impl IgnoreInterrupts {
    fn new() -> Self {
        // SAFETY: registers a handler that touches no state.
        unsafe { SetConsoleCtrlHandler(Some(swallow_interrupt), 1) };
        IgnoreInterrupts
    }
}

#[cfg(windows)]
impl Drop for IgnoreInterrupts {
    fn drop(&mut self) {
        // SAFETY: removes the handler `new` registered.
        unsafe { SetConsoleCtrlHandler(Some(swallow_interrupt), 0) };
    }
}

#[cfg(unix)]
fn socket_round_trip(socket: &Path, line: &str, timeout: Duration) -> Result<String> {
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket)
        .with_context(|| format!("could not connect to {}", socket.display()))?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply)?;
    Ok(reply)
}

/// Herdr on Windows serves the named pipe `\\.\pipe\<socket path>` (the path
/// itself is a marker file). A pipe opened as a file has no timeouts, so the
/// exchange runs on a thread that is abandoned at the deadline.
#[cfg(windows)]
fn socket_round_trip(socket: &Path, line: &str, timeout: Duration) -> Result<String> {
    use std::io::{BufRead, BufReader};
    const ERROR_PIPE_BUSY: i32 = 231;
    let pipe = format!(r"\\.\pipe\{}", socket.display());
    let deadline = Instant::now() + timeout;
    let mut stream = loop {
        match std::fs::OpenOptions::new().read(true).write(true).open(&pipe) {
            Ok(stream) => break stream,
            // Every pipe instance is taken; the server makes a new one shortly.
            Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => std::thread::sleep(POLL),
            Err(e) => return Err(e).with_context(|| format!("could not connect to {pipe}")),
        }
    };
    let line = format!("{line}\n");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let exchange = (|| -> std::io::Result<String> {
            stream.write_all(line.as_bytes())?;
            let mut reply = String::new();
            BufReader::new(stream).read_line(&mut reply)?;
            Ok(reply)
        })();
        let _ = sender.send(exchange);
    });
    match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(reply) => Ok(reply?),
        Err(_) => anyhow::bail!("no reply from {pipe} within {timeout:?}"),
    }
}

fn read_all<R: Read + Send + 'static>(mut pipe: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = pipe.read_to_end(&mut buf);
        buf
    })
}

fn join_text(thread: std::thread::JoinHandle<Vec<u8>>) -> String {
    String::from_utf8_lossy(&thread.join().unwrap_or_default()).into_owned()
}

fn kill(child: &mut std::process::Child, own_group: bool) {
    #[cfg(unix)]
    if own_group {
        // The child is its group's leader, so its pid is the pgid. Grandchildren
        // hold the pipes open; killing only the child would leave readers hanging.
        let _ = Command::new("/bin/kill")
            .args(["-TERM", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        std::thread::sleep(Duration::from_millis(200));
        let _ = Command::new("/bin/kill")
            .args(["-KILL", "--", &format!("-{}", child.id())])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    // Windows: the caller already terminated the child's job, if it has one.
    #[cfg(windows)]
    let _ = own_group;
    let _ = child.kill();
}

/// A Windows job holding an `own_group` child and everything it starts.
/// `taskkill /T` is not enough: it walks parent process ids, and a program a
/// Git Bash script `exec`s is not a child of that shell in Windows' eyes, so
/// it would keep the pipes open. Every descendant inherits the job.
#[cfg(windows)]
struct Job(isize);

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn CreateJobObjectW(attributes: *const std::ffi::c_void, name: *const u16) -> isize;
    fn AssignProcessToJobObject(job: isize, process: isize) -> i32;
    fn TerminateJobObject(job: isize, exit_code: u32) -> i32;
    fn CloseHandle(handle: isize) -> i32;
}

#[cfg(windows)]
impl Job {
    /// `None` when Windows refuses; the child alone is then killed on timeout.
    // ponytail: assigned right after spawn, so a grandchild started in the first
    // instant escapes; CREATE_SUSPENDED needs the thread handle std hides.
    fn assign(child: &std::process::Child) -> Option<Job> {
        use std::os::windows::io::AsRawHandle;
        // SAFETY: plain handle calls; the job handle is owned by the returned value.
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job == 0 {
                return None;
            }
            let job = Job(job);
            (AssignProcessToJobObject(job.0, child.as_raw_handle() as isize) != 0).then_some(job)
        }
    }

    fn terminate(&self) {
        // SAFETY: a job handle this value owns.
        unsafe { TerminateJobObject(self.0, 1) };
    }
}

#[cfg(windows)]
impl Drop for Job {
    fn drop(&mut self) {
        // SAFETY: closes the handle `assign` created; without kill-on-close the
        // processes live on, as a finished group's leftovers do on Unix.
        unsafe { CloseHandle(self.0) };
    }
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::RefCell;

    type Matcher = Box<dyn Fn(&Cmd) -> bool>;

    /// A scripted runner: the first rule whose matcher accepts the command
    /// answers it. Every command is recorded, matched or not.
    #[derive(Default)]
    pub struct FakeRunner {
        rules: RefCell<Vec<(Matcher, Box<dyn Fn(&Cmd) -> Result<Output>>)>>,
        pub calls: RefCell<Vec<Cmd>>,
        /// (socket, request line) of every socket request.
        pub socket_requests: RefCell<Vec<(PathBuf, String)>>,
    }

    impl FakeRunner {
        pub fn new() -> Self {
            Self::default()
        }

        /// Answer commands whose display line contains `needle`.
        pub fn on(&self, needle: &str, output: Output) -> &Self {
            let needle = needle.to_string();
            self.rules.borrow_mut().push((
                Box::new(move |cmd| cmd.display().contains(&needle)),
                Box::new(move |_| Ok(output.clone())),
            ));
            self
        }

        pub fn on_fn(
            &self,
            matcher: impl Fn(&Cmd) -> bool + 'static,
            answer: impl Fn(&Cmd) -> Result<Output> + 'static,
        ) -> &Self {
            self.rules
                .borrow_mut()
                .push((Box::new(matcher), Box::new(answer)));
            self
        }

        pub fn count(&self, needle: &str) -> usize {
            self.calls
                .borrow()
                .iter()
                .filter(|cmd| cmd.display().contains(needle))
                .count()
        }
    }

    /// How a `posix_shell() -c` command starts, as the fake sees it.
    pub fn sh_c() -> String {
        format!("{} -c", posix_shell())
    }

    pub fn ok(stdout: &str) -> Output {
        Output {
            code: Some(0),
            stdout: stdout.to_string(),
            ..Output::default()
        }
    }

    pub fn fail(code: i32, stderr: &str) -> Output {
        Output {
            code: Some(code),
            stderr: stderr.to_string(),
            ..Output::default()
        }
    }

    pub fn timeout() -> Output {
        Output {
            timed_out: true,
            ..Output::default()
        }
    }

    impl Runner for FakeRunner {
        fn run(&self, cmd: &Cmd) -> Result<Output> {
            self.calls.borrow_mut().push(cmd.clone());
            for (matcher, answer) in self.rules.borrow().iter() {
                if matcher(cmd) {
                    return answer(cmd);
                }
            }
            anyhow::bail!("FakeRunner: no rule for `{}`", cmd.display())
        }

        fn socket_request(&self, socket: &Path, line: &str, _timeout: Duration) -> Result<String> {
            self.socket_requests.borrow_mut().push((socket.to_path_buf(), line.to_string()));
            Ok(r#"{"id":"hp","result":{"type":"agent_view","active":true}}"#.to_string())
        }

        /// The first matching rule answers (it may change what later calls
        /// see, as a starting agent does), then `poll` runs once and the
        /// command exits with the rule's code.
        fn run_foreground(&self, cmd: &Cmd, poll: &mut dyn FnMut() -> bool) -> Result<Option<i32>> {
            self.calls.borrow_mut().push(cmd.clone());
            let out = {
                let rules = self.rules.borrow();
                let Some((_, answer)) = rules.iter().find(|(matcher, _)| matcher(cmd)) else {
                    anyhow::bail!("FakeRunner: no rule for `{}`", cmd.display())
                };
                answer(cmd)?
            };
            poll();
            Ok(out.code)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sh(script: &str, timeout: Duration) -> Cmd {
        Cmd::new(posix_shell(), timeout).args(["-c", script])
    }

    #[test]
    fn captures_output_and_exit_code() {
        let out = RealRunner
            .run(&sh("echo hi; echo err >&2; exit 3", Duration::from_secs(5)))
            .unwrap();
        assert_eq!(out.code, Some(3));
        assert_eq!(out.stdout, "hi\n");
        assert_eq!(out.stderr, "err\n");
        assert!(!out.success());
    }

    #[test]
    fn passes_stdin() {
        let out = RealRunner
            .run(&sh("cat", Duration::from_secs(5)).stdin("hello"))
            .unwrap();
        assert_eq!(out.stdout, "hello");
    }

    #[test]
    fn missing_program_is_an_error() {
        assert!(
            RealRunner
                .run(&Cmd::new("hp-no-such-program", Duration::from_secs(1)))
                .is_err()
        );
    }

    #[test]
    fn times_out_a_chatty_child() {
        // `yes` fills the pipe far past its buffer; the reader threads keep it
        // drained so the deadline still fires.
        let start = Instant::now();
        // Git Bash keeps a stub process over an exec'd program on Windows, so
        // there the tree is killed as a group.
        #[cfg(unix)]
        let cmd = Cmd::new("yes", Duration::from_millis(300));
        #[cfg(windows)]
        let cmd = sh("yes", Duration::from_millis(300)).own_group();
        let out = RealRunner.run(&cmd).unwrap();
        assert!(out.timed_out);
        assert!(!out.success());
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(out.stdout.len() > 65_536);
    }

    #[test]
    fn group_kill_reaches_grandchildren() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("survived");
        let script = format!("(sleep 2; touch '{}') & wait", crate::paths::shell_path(&marker));
        let start = Instant::now();
        let out = RealRunner
            .run(&sh(&script, Duration::from_millis(300)).own_group())
            .unwrap();
        assert!(out.timed_out);
        assert!(start.elapsed() < Duration::from_secs(2));
        std::thread::sleep(Duration::from_millis(2300));
        assert!(!marker.exists(), "grandchild outlived the group kill");
    }

    /// `ticker start` and the popup's delayed focus return at once to a caller
    /// reading their output (herdr runs plugin commands so): the detached child
    /// must not keep the caller's pipes open for its whole life.
    #[cfg(windows)]
    #[test]
    fn a_detached_child_does_not_hold_the_callers_pipes() {
        if std::env::var_os("HP_DETACH_HELPER").is_some() {
            let mut sleeper = Command::new(posix_shell());
            sleeper.args(["-c", "sleep 6"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            spawn_detached(&mut sleeper).unwrap();
            return;
        }
        let start = Instant::now();
        let out = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "runner::tests::a_detached_child_does_not_hold_the_callers_pipes", "--test-threads", "1"])
            .env("HP_DETACH_HELPER", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stdout));
        assert!(String::from_utf8_lossy(&out.stdout).contains("1 passed"), "{}", String::from_utf8_lossy(&out.stdout));
        assert!(start.elapsed() < Duration::from_secs(4), "the caller waited {:?} for the detached child", start.elapsed());
    }

    /// A request line reaches herdr's named pipe for the socket path and the
    /// reply line comes back; a pipe nobody serves is an error, not a hang.
    #[cfg(windows)]
    #[test]
    fn socket_request_uses_the_named_pipe_of_the_socket_path() {
        use std::io::{BufRead, BufReader};
        use std::os::windows::ffi::OsStrExt;
        use std::os::windows::io::FromRawHandle;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn CreateNamedPipeW(name: *const u16, open: u32, mode: u32, max: u32, out: u32, inp: u32, timeout: u32, security: *const std::ffi::c_void) -> isize;
            fn ConnectNamedPipe(pipe: isize, overlapped: *mut std::ffi::c_void) -> i32;
        }
        let socket = std::env::temp_dir().join(format!("hp-pipe-test-{}.sock", std::process::id()));
        let name: Vec<u16> = std::ffi::OsStr::new(&format!(r"\\.\pipe\{}", socket.display())).encode_wide().chain([0]).collect();
        // PIPE_ACCESS_DUPLEX, byte mode, one instance.
        let handle = unsafe { CreateNamedPipeW(name.as_ptr(), 3, 0, 1, 4096, 4096, 0, std::ptr::null()) };
        assert!(handle != -1, "{}", std::io::Error::last_os_error());
        let server = std::thread::spawn(move || {
            unsafe { ConnectNamedPipe(handle, std::ptr::null_mut()) };
            let mut pipe = unsafe { std::fs::File::from_raw_handle(handle as _) };
            let mut request = String::new();
            BufReader::new(&pipe).read_line(&mut request).unwrap();
            pipe.write_all(format!("{{\"echo\":{}}}\n", request.trim()).as_bytes()).unwrap();
            request
        });
        let reply = RealRunner.socket_request(&socket, r#"{"id":"hp"}"#, Duration::from_secs(5)).unwrap();
        assert_eq!(reply, "{\"echo\":{\"id\":\"hp\"}}\n");
        assert_eq!(server.join().unwrap(), "{\"id\":\"hp\"}\n");
        assert!(RealRunner.socket_request(&socket.with_extension("none"), "{}", Duration::from_secs(1)).is_err());
    }
}
