//! Proves that every read-only command leaves the filesystem and process state untouched.
//!
//! The unit suite asserts this at the planning boundary. This suite asserts it for the shipped
//! executable, including the error paths, because that is what an operator actually runs.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

const ASM: &str = env!("CARGO_BIN_EXE_asm");

/// A fixture holding a project, a Skill source, and a private home directory.
///
/// The home directory is redirected so a Claude staging plan resolves inside the fixture. That
/// makes "the session root was not created" an assertion this suite can actually make, instead of
/// a claim about the developer's real home.
struct Fixture {
    root: PathBuf,
    project: PathBuf,
    sources: PathBuf,
    home: PathBuf,
    launch_sentinel: PathBuf,
    agent_bin: PathBuf,
}

impl Fixture {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "skillmount-readonly-{label}-{}-{nonce}",
            std::process::id()
        ));
        let project = root.join("project");
        let sources = root.join("sources");
        let home = root.join("home");
        for path in [&project, &sources, &home, &root.join("codex-home")] {
            fs::create_dir_all(path).expect("fixture directory");
        }
        let launch_sentinel = root.join("agent-was-launched");
        let agent_bin = fake_agent_executable(&root, &launch_sentinel);
        Self {
            root,
            project,
            sources,
            home,
            launch_sentinel,
            agent_bin,
        }
    }

    fn skill(&self, name: &str) -> PathBuf {
        let path = self.sources.join(name);
        fs::create_dir_all(&path).expect("skill directory");
        fs::write(
            path.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {name} description\n---\n"),
        )
        .expect("SKILL.md");
        path
    }

    fn command(&self, arguments: &[&str]) -> Command {
        let mut command = Command::new(ASM);
        command
            .current_dir(&self.project)
            .args(arguments)
            .env("HOME", &self.home)
            .env("USERPROFILE", &self.home)
            .env("SKILLMOUNT_TEST_CODEX_USER_HOME", &self.home)
            .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "absent")
            .env("LOCALAPPDATA", self.home.join("AppData/Local"))
            .env("CODEX_HOME", self.root.join("codex-home"))
            .env("SKILLMOUNT_TEST_CODEX_VERSION", "codex-cli 0.146.0")
            .env("SKILLMOUNT_STATE_DIR", self.root.join("state"))
            .env(
                "SKILLMOUNT_CODEX_ADMIN_SKILLS_DIR",
                self.root.join("admin-skills"),
            );
        command
    }

    fn run(&self, arguments: &[&str]) -> Output {
        self.command(arguments).output().expect("asm should run")
    }

    /// Roots that must be byte-identical before and after a read-only command.
    fn watched(&self) -> Vec<PathBuf> {
        vec![
            self.project.clone(),
            self.sources.clone(),
            self.home.clone(),
        ]
    }

    /// Roots holding data that belongs to the operator rather than to `SkillMount`.
    ///
    /// A mutating session legitimately creates `SkillMount`'s own state directories — journals and
    /// lock files — before it can decide whether it may proceed. Those are not the operator's data,
    /// so a test about a failing session watches only the project and the Skill sources.
    fn user_data(&self) -> Vec<PathBuf> {
        vec![self.project.clone(), self.sources.clone()]
    }

    fn assert_unchanged(&self, arguments: &[&str]) -> Output {
        self.assert_roots_unchanged(&self.watched(), arguments)
    }

    fn assert_roots_unchanged(&self, roots: &[PathBuf], arguments: &[&str]) -> Output {
        let before = roots
            .iter()
            .map(|root| (root.clone(), snapshot(root)))
            .collect::<Vec<_>>();

        let output = self.run(arguments);

        for (root, expected) in before {
            assert_eq!(
                snapshot(&root),
                expected,
                "`asm {}` modified {}",
                arguments.join(" "),
                root.display()
            );
        }
        assert!(
            !self.launch_sentinel.exists(),
            "`asm {}` launched a child process",
            arguments.join(" ")
        );
        output
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

/// Provides a native executable for the mutation-boundary launch sentinel.
fn fake_agent_executable(root: &Path, sentinel: &Path) -> PathBuf {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        let path = root.join("fake-agent");
        fs::write(&path, format!("#!/bin/sh\ntouch {}\n", sentinel.display())).expect("fake agent");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("fake agent mode");
        path
    }
    #[cfg(windows)]
    {
        let _ = (root, sentinel);
        std::env::var_os("COMSPEC")
            .map(PathBuf::from)
            .expect("Windows provides a native command interpreter")
    }
}

/// Records a tree without following links, so a replaced link shows up as a difference.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, String> {
    let mut entries = BTreeMap::new();
    collect(root, root, &mut entries);
    entries
}

fn collect(root: &Path, current: &Path, entries: &mut BTreeMap<PathBuf, String>) {
    let Ok(metadata) = fs::symlink_metadata(current) else {
        return;
    };
    let file_type = metadata.file_type();
    let descriptor = if file_type.is_symlink() {
        format!(
            "link -> {}",
            fs::read_link(current).map_or_else(|_| "?".into(), |t| t.display().to_string())
        )
    } else if file_type.is_dir() {
        "dir".to_owned()
    } else {
        format!("file {}", metadata.len())
    };
    if let Ok(relative) = current.strip_prefix(root) {
        entries.insert(relative.to_path_buf(), descriptor);
    }
    if file_type.is_dir() && !file_type.is_symlink() {
        let Ok(children) = fs::read_dir(current) else {
            return;
        };
        for child in children.flatten() {
            collect(root, &child.path(), entries);
        }
    }
}

#[test]
fn inspect_reports_both_agents_without_touching_anything() {
    let fixture = Fixture::new("inspect");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&["inspect", "--skills-dir", &sources]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("Agent:          codex"));
    assert!(rendered.contains("Agent:          claude"));
    assert!(rendered.contains("Overlay: 1 Skill(s)"));
}

#[test]
fn inspect_can_be_filtered_to_one_agent() {
    let fixture = Fixture::new("inspect-filter");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output =
        fixture.assert_unchanged(&["inspect", "--skills-dir", &sources, "--agent", "claude"]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("Agent:          claude"));
    assert!(!rendered.contains("Agent:          codex"));
}

#[test]
fn a_codex_dry_run_plans_the_whole_layout_without_creating_it() {
    let fixture = Fixture::new("dry-run-codex");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("MKDIR  .agents"));
    assert!(
        rendered.contains("MKDIR  .agents/skills") || rendered.contains("MKDIR  .agents\\skills")
    );
    assert!(
        rendered.contains("LINK   .agents/skills/alpha")
            || rendered.contains("LINK   .agents\\skills\\alpha")
    );
    assert!(!fixture.project.join(".codex").exists());
    assert!(!fixture.project.join(".agents").exists());
}

#[test]
fn codex_rejects_every_plugin_namespace_spelling_above_a_selected_source() {
    for manifest_directory in [".codex-plugin", ".claude-plugin", ".cursor-plugin"] {
        let fixture = Fixture::new(manifest_directory);
        fixture.skill("alpha");
        let manifest = fixture.sources.join(manifest_directory).join("plugin.json");
        fs::create_dir_all(manifest.parent().expect("manifest parent"))
            .expect("manifest directory");
        fs::write(&manifest, r#"{"name":"fixture-plugin"}"#).expect("plugin manifest");
        let canonical_manifest = fs::canonicalize(&manifest).expect("canonical plugin manifest");
        let sources = fixture.sources.to_string_lossy().into_owned();

        let output = fixture.assert_unchanged(&[
            "codex",
            "--skills-dir",
            &sources,
            "--dry-run",
            "--",
            "exec",
            "fixture",
        ]);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(73),
            "{manifest_directory}: {stderr}"
        );
        assert!(stderr.contains("namespace-qualify"), "{stderr}");
        assert!(
            stderr.contains(canonical_manifest.to_string_lossy().as_ref()),
            "{stderr}"
        );
    }
}

#[test]
fn an_invalid_higher_precedence_plugin_manifest_masks_lower_spellings_like_codex() {
    let fixture = Fixture::new("invalid-plugin-manifest-precedence");
    fixture.skill("alpha");
    let invalid = fixture.sources.join(".codex-plugin/plugin.json");
    let valid_lower = fixture.sources.join(".claude-plugin/plugin.json");
    fs::create_dir_all(invalid.parent().expect("invalid manifest parent"))
        .expect("invalid manifest directory");
    fs::create_dir_all(valid_lower.parent().expect("valid manifest parent"))
        .expect("valid manifest directory");
    fs::write(invalid, r#"{"name":42}"#).expect("invalid higher-precedence manifest");
    fs::write(valid_lower, r#"{"name":"ignored-plugin"}"#)
        .expect("valid lower-precedence manifest");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn an_oversized_plugin_manifest_fails_closed_without_launching_codex() {
    let fixture = Fixture::new("oversized-plugin-manifest");
    fixture.skill("alpha");
    let manifest = fixture.sources.join(".codex-plugin/plugin.json");
    fs::create_dir_all(manifest.parent().expect("manifest parent")).expect("manifest directory");
    let contents = format!(
        r#"{{"padding":"{}","name":"fixture-plugin"}}"#,
        "a".repeat(64 * 1024)
    );
    fs::write(&manifest, contents).expect("oversized plugin manifest");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(73), "{stderr}");
    assert!(stderr.contains("exceeds 65536 bytes"), "{stderr}");
}

// APFS rejects an unpaired native filename before the adapter can observe it. Linux and NTFS
// permit the respective byte/WTF-16 spellings, so exercise the shipped boundary there; the
// platform-independent predicate also has unit coverage on every Unix and Windows target.
#[cfg(any(target_os = "linux", windows))]
#[test]
fn a_non_unicode_codex_directory_entry_fails_closed_without_launching_codex() {
    let fixture = Fixture::new("non-unicode-codex-entry");
    fixture.skill("alpha");
    let discovery = fixture.project.join(".agents/skills");
    fs::create_dir_all(&discovery).expect("Codex discovery root");
    fs::create_dir(discovery.join(non_unicode_entry_name()))
        .expect("native non-Unicode directory entry");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(73), "{stderr}");
    assert!(
        stderr.contains("non-Unicode directory-entry name"),
        "{stderr}"
    );
}

#[cfg(target_os = "linux")]
fn non_unicode_entry_name() -> std::ffi::OsString {
    use std::os::unix::ffi::OsStringExt as _;

    std::ffi::OsString::from_vec(vec![b's', b'k', b'i', b'l', b'l', 0xff])
}

#[cfg(windows)]
fn non_unicode_entry_name() -> std::ffi::OsString {
    use std::os::windows::ffi::OsStringExt as _;

    std::ffi::OsString::from_wide(&[
        u16::from(b's'),
        u16::from(b'k'),
        u16::from(b'i'),
        u16::from(b'l'),
        u16::from(b'l'),
        0xd800,
    ])
}

#[test]
fn a_claude_dry_run_never_creates_a_session_root() {
    let fixture = Fixture::new("dry-run-claude");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&["claude", "--skills-dir", &sources, "--dry-run"]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(rendered.contains("<session-id>"), "{rendered}");
    assert!(rendered.contains("argv[1] = --add-dir"));
    assert!(
        !fixture.home.join("Library/Caches/skillmount").exists()
            && !fixture.home.join("AppData/Local/skillmount").exists(),
        "no session or transaction storage may be created"
    );
}

#[test]
fn a_destination_conflict_fails_without_changing_the_project() {
    let fixture = Fixture::new("conflict");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.project.join(".agents/skills/alpha")).expect("conflicting entry");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);

    assert_eq!(
        output.status.code(),
        Some(73),
        "a destination conflict is a filesystem-state failure"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("conflicts with"));
}

#[test]
fn a_skip_policy_preserves_the_existing_entry_and_reports_it() {
    let fixture = Fixture::new("skip");
    fixture.skill("alpha");
    fs::create_dir_all(fixture.project.join(".agents/skills/alpha")).expect("existing entry");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--conflict",
        "skip",
        "--",
        "exec",
        "fixture",
    ]);

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("KEEP"));
}

#[test]
fn input_and_catalog_failures_keep_their_stable_codes_and_change_nothing() {
    let fixture = Fixture::new("failures");
    let missing = fixture.root.join("absent");
    let missing_output =
        fixture.assert_unchanged(&["inspect", "--skills-dir", &missing.to_string_lossy()]);
    assert_eq!(missing_output.status.code(), Some(66));

    let invalid = fixture.root.join("invalid");
    fs::create_dir_all(&invalid).expect("invalid fixture");
    fs::write(invalid.join("SKILL.md"), "no frontmatter\n").expect("invalid SKILL.md");
    let invalid_output =
        fixture.assert_unchanged(&["inspect", "--skills-dir", &invalid.to_string_lossy()]);
    assert_eq!(invalid_output.status.code(), Some(65));
}

#[test]
fn a_skill_disabling_claude_argument_is_rejected_before_planning() {
    let fixture = Fixture::new("claude-rejected-args");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "claude",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "--bare",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--bare"));
}

#[test]
fn codex_root_changing_arguments_are_rejected_before_discovery_can_diverge() {
    let fixture = Fixture::new("codex-rejected-root-args");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        vec!["-C", "other"],
        vec!["-Cother"],
        vec!["--cd", "other"],
        vec!["--cd=other"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());

        let output = fixture.assert_unchanged(&arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(64), "{forwarded:?}: {stderr}");
        assert!(
            stderr.contains("changes the child discovery root"),
            "{stderr}"
        );
        assert!(stderr.contains("--cwd"), "{stderr}");
    }
}

#[test]
fn mutating_codex_rejects_root_changing_arguments_before_creating_state() {
    let fixture = Fixture::new("codex-rejected-root-args-mutation");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--",
        "-Cother",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("changes the child discovery root"));
    assert!(
        !fixture.root.join("state").exists(),
        "argument validation must precede lock and journal storage"
    );
}

#[test]
fn codex_remote_arguments_are_rejected_before_local_discovery_can_diverge() {
    let fixture = Fixture::new("codex-rejected-remote-args");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        vec!["--remote", "ws://127.0.0.1:9"],
        vec!["--remote=ws://127.0.0.1:9"],
        vec!["--remote-auth-token-env", "CODEX_REMOTE_TOKEN"],
        vec!["--remote-auth-token-env=CODEX_REMOTE_TOKEN"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());

        let output = fixture.assert_unchanged(&arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(64), "{forwarded:?}: {stderr}");
        assert!(stderr.contains("remote app server"), "{stderr}");
    }
}

#[test]
fn mutating_codex_rejects_remote_before_creating_state() {
    let fixture = Fixture::new("codex-rejected-remote-mutation");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--",
        "--remote=ws://127.0.0.1:9",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("remote app server"));
    assert!(
        !fixture.root.join("state").exists(),
        "remote validation must precede lock and journal storage"
    );
}

#[test]
fn codex_config_profile_and_resume_arguments_are_rejected() {
    let fixture = Fixture::new("codex-rejected-discovery-contract-args");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        vec!["-c", "project_root_markers=[]"],
        vec!["-cskills.bundled.enabled=false"],
        vec!["--config", "skills.bundled.enabled=false"],
        vec!["--config=skills.bundled.enabled=false"],
        vec!["-p", "alternate"],
        vec!["-palternate"],
        vec!["--profile", "alternate"],
        vec!["--profile=alternate"],
        vec!["--enable", "plugins"],
        vec!["--enable=plugins"],
        vec!["--disable", "plugins"],
        vec!["--disable=plugins"],
        vec!["exec", "--ignore-user-config", "prompt"],
        vec!["resume", "session-id"],
        vec!["fork", "session-id"],
        vec!["exec", "resume", "session-id"],
        vec!["exec", "--color", "always", "resume", "session-id"],
        vec!["exec", "--image=fixture.png", "resume", "session-id"],
        vec!["exec", "-ifixture.png", "resume", "session-id"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());

        let output = fixture.assert_unchanged(&arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(64), "{forwarded:?}: {stderr}");
        assert!(
            stderr.contains("discovery contract") || stderr.contains("discovery CWD"),
            "{forwarded:?}: {stderr}"
        );
    }
}

#[test]
fn command_shaped_option_values_and_prompts_are_not_misclassified() {
    let fixture = Fixture::new("codex-command-shaped-values");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        vec!["review", "--base", "resume"],
        vec!["review", "resume"],
        vec!["exec", "--output-schema", "login"],
        vec!["-m", "doctor", "review", "resume"],
        vec!["--image=fixture.png", "exec", "prompt"],
        vec!["-ifixture.png", "exec", "prompt"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());
        let output = fixture.assert_unchanged(&arguments);
        assert!(
            output.status.success(),
            "{forwarded:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn bare_variadic_images_are_rejected_before_they_can_hide_a_nested_resume() {
    let fixture = Fixture::new("codex-rejected-bare-image");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        vec!["exec", "-i", "foo", "--json", "resume", "--help"],
        vec!["exec", "--image", "foo", "--ephemeral", "resume", "--help"],
        vec!["exec", "-i", "foo", "-m", "gpt-5.2", "resume", "--help"],
        vec!["exec", "-i", "foo"],
        vec!["-i", "foo", "exec", "prompt"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());
        let output = fixture.assert_unchanged(&arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(64), "{forwarded:?}: {stderr}");
        assert!(stderr.contains("variadic"), "{forwarded:?}: {stderr}");
        assert!(stderr.contains("--image=VALUE"), "{forwarded:?}: {stderr}");
    }
}

#[test]
fn interactive_codex_tui_modes_are_rejected_before_planning() {
    let fixture = Fixture::new("codex-rejected-interactive-tui");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for forwarded in [
        Vec::<&str>::new(),
        vec!["initial prompt"],
        vec!["-m", "fixture-model", "initial prompt"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());
        let output = fixture.assert_unchanged(&arguments);
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(output.status.code(), Some(64), "{forwarded:?}: {stderr}");
        assert!(
            stderr.contains("interactive Codex TUI"),
            "{forwarded:?}: {stderr}"
        );
        assert!(!fixture.root.join("state").exists());
    }
}

#[test]
fn codex_service_and_operator_commands_are_rejected() {
    let fixture = Fixture::new("codex-rejected-non-session-command");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    for command in [
        "login",
        "mcp-server",
        "app-server",
        "remote-control",
        "doctor",
        "sandbox",
        "apply",
        "archive",
        "cloud",
        "cloud-tasks",
        "exec-server",
        "execpolicy",
        "responses-api-proxy",
        "stdio-to-uds",
        "features",
        "help",
    ] {
        let output = fixture.assert_unchanged(&[
            "codex",
            "--skills-dir",
            &sources,
            "--dry-run",
            "--",
            command,
        ]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert_eq!(output.status.code(), Some(64), "{command}: {stderr}");
        assert!(stderr.contains("single bounded"), "{command}: {stderr}");
    }

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "-m",
        "doctor",
        "app-server",
    ]);
    assert_eq!(output.status.code(), Some(64));

    for forwarded in [
        vec!["exec", "help"],
        vec!["--help"],
        vec!["--version"],
        vec!["-Vattached", "exec", "prompt"],
        vec!["-hattached", "exec", "prompt"],
        vec!["exec", "-Vattached"],
        vec!["exec", "-hattached"],
    ] {
        let mut arguments = vec!["codex", "--skills-dir", &sources, "--dry-run", "--"];
        arguments.extend(forwarded.iter().copied());
        let output = fixture.assert_unchanged(&arguments);
        assert_eq!(output.status.code(), Some(64), "{forwarded:?}");
    }
}

#[test]
fn mutating_codex_rejects_higher_precedence_managed_configuration_before_state() {
    let fixture = Fixture::new("codex-rejected-managed-config");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();
    let arguments = [
        "codex",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--",
        "exec",
        "fixture",
    ];
    let before = snapshot(&fixture.project);

    let output = fixture
        .command(&arguments)
        .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "present")
        .output()
        .expect("asm should reject managed configuration");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(64), "{stderr}");
    assert!(stderr.contains("managed configuration"), "{stderr}");
    assert_eq!(snapshot(&fixture.project), before);
    assert!(!fixture.root.join("state").exists());
    assert!(!fixture.launch_sentinel.exists());
}

#[test]
fn mutating_codex_rejects_config_overrides_before_creating_state() {
    let fixture = Fixture::new("codex-rejected-config-mutation");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--agent-bin",
        &agent_bin,
        "--",
        "--config=skills.bundled.enabled=false",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("discovery contract"));
    assert!(
        !fixture.root.join("state").exists(),
        "config validation must precede lock and journal storage"
    );
}

#[test]
fn interactive_prompt_after_codex_option_termination_is_rejected() {
    let fixture = Fixture::new("codex-option-termination");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "--",
        "-Cprompt-text",
    ]);

    assert_eq!(output.status.code(), Some(64));
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive Codex TUI"));
}

#[test]
fn a_session_that_fails_before_the_mutation_boundary_changes_nothing() {
    let fixture = Fixture::new("mutation-boundary");
    fixture.skill("alpha");
    // A destination the plan cannot resolve, so the run fails while it is still read-only. A
    // session that gets past planning is no longer a read-only path and belongs to
    // `tests/transaction.rs`; what this suite still owns is the guarantee that a failure on the
    // way there costs nothing.
    fs::create_dir_all(fixture.project.join(".agents/skills/alpha")).expect("conflicting entry");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let agent_bin = fixture.agent_bin.to_string_lossy().into_owned();

    let mut arguments = vec!["codex", "--skills-dir", &sources, "--agent-bin", &agent_bin];
    #[cfg(windows)]
    let sentinel_command = format!("type nul > \"{}\"", fixture.launch_sentinel.display());
    #[cfg(windows)]
    let agent_arguments = ["--", "exec", "/d", "/c", sentinel_command.as_str()];
    #[cfg(not(windows))]
    let agent_arguments = ["--", "exec", "fixture"];
    arguments.extend(agent_arguments);

    let output = fixture.assert_roots_unchanged(&fixture.user_data(), &arguments);

    assert_eq!(output.status.code(), Some(73));
    assert!(String::from_utf8_lossy(&output.stderr).contains("conflicts with"));
}

#[test]
fn verbose_output_names_the_rightmost_winner_and_every_shadowed_origin() {
    let fixture = Fixture::new("provenance");
    let base = fixture.root.join("base-skills");
    let team = fixture.root.join("team-skills");
    for root in [&base, &team] {
        let skill = root.join("alpha");
        fs::create_dir_all(&skill).expect("source");
        fs::write(
            skill.join("SKILL.md"),
            "---\nname: alpha\ndescription: alpha description\n---\n",
        )
        .expect("SKILL.md");
    }

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &base.to_string_lossy(),
        "--skills-dir",
        &team.to_string_lossy(),
        "--dry-run",
        "--verbose",
        "--",
        "exec",
    ]);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    assert!(
        rendered.contains("Overlay: 1 Skill(s), 1 source override(s)"),
        "{rendered}"
    );
    assert!(rendered.contains("OVERRIDE  alpha"), "{rendered}");
    assert!(
        rendered.contains("different source"),
        "the displaced origin and its reason must be listed: {rendered}"
    );
    // The selected origin is reported canonically, so the expectation is canonicalized too: a
    // temporary directory sits behind a symbolic link on macOS.
    let winner = fs::canonicalize(team.join("alpha")).expect("canonical winner");
    assert!(
        rendered.contains(&format!("-> [2] {}", winner.display())),
        "the rightmost occurrence wins: {rendered}"
    );
}

#[test]
fn an_existing_transaction_record_is_reported_and_left_alone() {
    let fixture = Fixture::new("recovery");
    fixture.skill("alpha");
    let transactions = fixture.root.join("state/transactions");
    fs::create_dir_all(&transactions).expect("transaction directory");
    let record = transactions.join("01JEXAMPLE.journal");
    fs::write(&record, "not a journal this build wrote\n").expect("transaction record");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let output = fixture.assert_unchanged(&[
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--",
        "exec",
        "fixture",
    ]);

    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("WOULD RETAIN"),
        "a journal this build cannot interpret must be reported rather than ignored: {}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert_eq!(
        fs::read_to_string(&record).expect("record still readable"),
        "not a journal this build wrote\n",
        "a dry run must not recover, rewrite, or remove a transaction journal"
    );
}

#[test]
fn passthrough_values_with_shell_metacharacters_stay_separate_indexed_values() {
    let fixture = Fixture::new("metacharacters");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();
    let awkward = ["--flag", "a b\"c'd;e", "$(id)", "x && y", "back\\slash"];

    let mut arguments = vec![
        "codex",
        "--skills-dir",
        &sources,
        "--dry-run",
        "--verbose",
        "--",
        "exec",
    ];
    arguments.extend(awkward);
    let output = fixture.assert_unchanged(&arguments);

    assert!(output.status.success());
    let rendered = String::from_utf8_lossy(&output.stdout);
    let effective_passthrough_offset = 1 + 6; // executable plus the pinned session arguments
    for (index, value) in awkward.iter().enumerate() {
        let forwarded_index = index + 1; // `exec` is passthrough argv[0]
        assert!(
            rendered.contains(&format!("[{forwarded_index}] {value}")),
            "forwarded value {index} must appear verbatim on its own line: {rendered}"
        );
        assert!(
            rendered.contains(&format!(
                "argv[{}] = {value}",
                forwarded_index + effective_passthrough_offset
            )),
            "effective argv must index the same value: {rendered}"
        );
    }
    assert!(
        !rendered
            .lines()
            .any(|line| line.contains(awkward[0]) && line.contains(awkward[1])),
        "two forwarded values must never share a line, which is what a joined command string \
         would produce: {rendered}"
    );
}

#[test]
fn read_only_output_is_identical_across_runs() {
    let fixture = Fixture::new("deterministic");
    fixture.skill("gamma");
    fixture.skill("alpha");
    let sources = fixture.sources.to_string_lossy().into_owned();

    let first = fixture.run(&["inspect", "--skills-dir", &sources]);
    let second = fixture.run(&["inspect", "--skills-dir", &sources]);

    assert_eq!(first.stdout, second.stdout);
    assert_eq!(first.stderr, second.stderr);
}
