//! End-to-end checks of the built binary with a scrubbed environment.

use std::path::Path;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_herdr-projects");

fn hp(home: &Path, args: &[&str]) -> std::process::Output {
    Command::new(BIN)
        .env_clear()
        .env("HOME", home)
        .args(args)
        .output()
        .unwrap()
}

#[test]
fn context_prints_a_usable_prefix_in_a_scrubbed_environment() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("my root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "Demo"]).status.success());

    let out = hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = String::from_utf8(out.stdout).unwrap();
    let prefix = text.lines().next().unwrap().strip_prefix("Commands: ").unwrap();
    // Fixed shape `<binary> --root <root>`, with the spaced root shell-quoted.
    assert_eq!(prefix, format!("{BIN} --root '{root_arg}'"));

    // The printed prefix works as typed, from a bare shell.
    let listed = Command::new("/bin/sh")
        .env_clear()
        .env("HOME", home.path())
        .args(["-c", &format!("{prefix} list")])
        .output()
        .unwrap();
    assert!(listed.status.success());
    assert_eq!(String::from_utf8_lossy(&listed.stdout), "demo\tactive\tno threads\n");
}

#[test]
fn peek_records_nothing_and_context_records_seen_items() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(hp(home.path(), &["--root", root_arg, "new", "demo"]).status.success());
    let item = "+++\nid = \"20260917T000000Z-routine-r-1\"\nkind = \"routine\"\nsubject = \"r\"\ncreated = \"x\"\nsummary = \"s\"\n+++\n";
    std::fs::write(root.join("demo/inbox/20260917T000000Z-routine-r-1.md"), item).unwrap();
    let seen = root.join("demo/.state/inbox-seen.json");

    assert!(hp(home.path(), &["--root", root_arg, "context", "demo", "--peek"]).status.success());
    assert!(!seen.exists());
    assert!(hp(home.path(), &["--root", root_arg, "context", "demo"]).status.success());
    assert!(std::fs::read_to_string(&seen).unwrap().contains("routine-r-1"));
}

#[test]
fn path_like_names_and_slugs_are_refused() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    assert!(!hp(home.path(), &["--root", root_arg, "new", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "open", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "context", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "thread", "list", "../x"]).status.success());
    assert!(!hp(home.path(), &["--root", root_arg, "delete", "../x", "--force"]).status.success());
    assert!(!root.exists());
    assert!(!home.path().join("x").exists());
}

#[test]
fn ticker_start_without_projects_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    assert!(hp(home.path(), &["ticker", "start"]).status.success());
    assert!(!home.path().join(".herdr-projects").exists());
    assert!(!home.path().join(".config").exists());
}

#[test]
fn configure_and_unconfigure_for_omp_touch_only_our_extension() {
    let home = tempfile::tempdir().unwrap();
    let agent = home.path().join("omp-agent");
    let extensions = agent.join("extensions");
    std::fs::create_dir_all(&extensions).unwrap();
    std::fs::write(extensions.join("other.ts"), "// someone else's\n").unwrap();
    let root = home.path().join("root");
    let root_arg = root.to_str().unwrap();
    let omp = |args: &[&str]| Command::new(BIN).env_clear().env("HOME", home.path()).env("PI_CODING_AGENT_DIR", &agent).arg("--root").arg(root_arg).args(args).output().unwrap();

    let out = omp(&["configure", "--clients", "omp", "--hooks-only"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let text = std::fs::read_to_string(extensions.join("herdr-projects.ts")).unwrap();
    assert!(text.starts_with("// HERDR_PROJECTS_OMP_VERSION=") && !text.contains("__HP_"), "{text}");
    assert!(text.contains(&format!("\"{}\"", std::fs::canonicalize(BIN).unwrap().display())));
    // The extension's hook bridge parses and never fails.
    assert!(omp(&["hook", "--agent", "omp"]).status.success());

    let out = omp(&["unconfigure"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(!extensions.join("herdr-projects.ts").exists());
    assert!(std::fs::symlink_metadata(agent.join("skills/autoproject")).is_err());
    assert_eq!(std::fs::read_to_string(extensions.join("other.ts")).unwrap(), "// someone else's\n");
}

#[test]
fn configure_for_omp_from_a_named_profile_session_covers_every_profile() {
    let home = tempfile::tempdir().unwrap();
    let default = home.path().join(".omp/agent");
    let neurable = home.path().join(".omp/profiles/neurable/agent");
    std::fs::create_dir_all(&default).unwrap();
    std::fs::create_dir_all(&neurable).unwrap();
    let root = home.path().join("root");
    // What an OMP session under `--profile neurable` exports to its children.
    let omp = |args: &[&str]| Command::new(BIN).env_clear().env("HOME", home.path()).env("OMP_PROFILE", "neurable").env("PI_CODING_AGENT_DIR", &neurable).arg("--root").arg(&root).args(args).output().unwrap();

    let out = omp(&["configure", "--clients", "omp", "--hooks-only"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    for dir in [&default, &neurable] {
        assert!(dir.join("extensions/herdr-projects.ts").is_file(), "{}", dir.display());
    }
    let out = omp(&["unconfigure"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    for dir in [&default, &neurable] {
        assert!(!dir.join("extensions/herdr-projects.ts").exists(), "{}", dir.display());
    }
}
