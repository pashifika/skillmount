//! Latest-agent boundary regressions use isolated roots and never run a real agent.

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("skillmount-latest-{}-{nonce}", std::process::id()));
        for relative in ["project/.git", "sources/alpha", "home", "codex-home"] {
            fs::create_dir_all(root.join(relative)).unwrap();
        }
        fs::write(
            root.join("sources/alpha/SKILL.md"),
            "---\nname: alpha\ndescription: alpha test\n---\nTest skill.\n",
        )
        .unwrap();
        Self(root)
    }

    fn run(&self, agent: &str, args: &[&str], dry_run: bool) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_asm"));
        command
            .current_dir(self.0.join("project"))
            .arg(agent)
            .arg("--skills-dir")
            .arg(self.0.join("sources"))
            .arg("--project-root")
            .arg(self.0.join("project"))
            .arg("--agent-bin")
            .arg(env!("CARGO_BIN_EXE_skillmount"))
            .env("HOME", self.0.join("home"))
            .env("USERPROFILE", self.0.join("home"))
            .env("SKILLMOUNT_TEST_CODEX_USER_HOME", self.0.join("home"))
            .env("SKILLMOUNT_TEST_CODEX_MANAGED_CONFIG", "absent")
            .env("CODEX_HOME", self.0.join("codex-home"))
            .env("CLAUDE_CONFIG_DIR", self.0.join("home/.claude"))
            .env(
                "SKILLMOUNT_CLAUDE_MANAGED_SKILLS_DIR",
                self.0.join("managed"),
            )
            .env("SKILLMOUNT_STATE_DIR", self.0.join("state"))
            .env("SKILLMOUNT_CODEX_ADMIN_SKILLS_DIR", self.0.join("admin"))
            .env_remove("CLAUDE_CODE_SAFE_MODE")
            .env_remove("CLAUDE_CODE_SIMPLE")
            .env_remove("SKILLMOUNT_TEST_CLAUDE_ENVIRONMENT_CONTROL_PATH");
        if dry_run {
            command.arg("--dry-run");
        }
        command.arg("--").args(args).output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

#[test]
fn latest_lifecycle_controls_fail_before_state_creation() {
    let fixture = Fixture::new();
    for (agent, args) in [
        ("codex", vec!["exec", "fork", "session"]),
        ("codex", vec!["exec", "--worktree", "prompt"]),
        (
            "codex",
            vec!["exec", "--thread-source", "cli", "fork", "session"],
        ),
        ("claude", vec!["attach", "session"]),
        ("claude", vec!["--debug", "daemon"]),
        ("claude", vec!["--cloud", "prompt"]),
        ("claude", vec!["--environment=ccpool_test", "prompt"]),
        ("claude", vec!["--desktop"]),
        ("claude", vec!["--restricted", "prompt"]),
    ] {
        for dry_run in [false, true] {
            let output = fixture.run(agent, &args, dry_run);
            assert_eq!(
                output.status.code(),
                Some(64),
                "{agent} {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert!(!fixture.0.join("state").exists());
            assert!(!fixture.0.join("project/.agents").exists());
            assert!(!fixture.0.join("project/.claude").exists());
        }
    }
}

#[test]
fn linked_worktree_fallback_is_rejected_but_separate_git_directory_is_supported() {
    let fixture = Fixture::new();
    fs::remove_dir(fixture.0.join("project/.git")).unwrap();
    fs::create_dir_all(fixture.0.join("git-dir")).unwrap();
    fs::write(fixture.0.join("project/.git"), "gitdir: ../git-dir\n").unwrap();
    // A submodule or --separate-git-dir repository has a gitfile but no commondir.
    let separate = fixture.run("claude", &["prompt"], true);
    assert!(
        separate.status.success(),
        "{}",
        String::from_utf8_lossy(&separate.stderr)
    );
    fs::write(fixture.0.join("git-dir/commondir"), "../common\n").unwrap();
    for dry_run in [true, false] {
        let blocked = fixture.run("claude", &["prompt"], dry_run);
        assert_eq!(blocked.status.code(), Some(64));
        assert!(String::from_utf8_lossy(&blocked.stderr).contains("main-checkout Skills"));
        assert!(!fixture.0.join("state").exists());
    }
    fs::create_dir_all(fixture.0.join("project/.claude/skills")).unwrap();
    let local = fixture.run("claude", &["prompt"], true);
    assert!(
        local.status.success(),
        "{}",
        String::from_utf8_lossy(&local.stderr)
    );
}

#[test]
fn claude_reserved_skill_names_are_not_reported_as_loadable() {
    for name in ["synced", "anthropic-skills"] {
        let fixture = Fixture::new();
        let skill = fixture.0.join("sources").join(name);
        fs::rename(fixture.0.join("sources/alpha"), &skill).unwrap();
        fs::write(
            skill.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: test\n---\nTest\n"),
        )
        .unwrap();
        let output = fixture.run("claude", &["prompt"], true);
        assert_eq!(
            output.status.code(),
            Some(65),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("reserves Skill name"),
            "{name}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.0.join("state").exists());
    }
}

#[test]
fn codex_product_policy_must_leave_the_selected_skill_visible() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .run("codex", &["exec", "prompt"], true)
            .status
            .success()
    );
    let metadata = fixture.0.join("sources/alpha/agents/openai.yaml");
    fs::create_dir_all(metadata.parent().unwrap()).unwrap();
    for (yaml, succeeds) in [
        (
            "interface:\n  display_name: Alpha\nunknown: harmless\n",
            true,
        ),
        ("policy:\n  products: []\n", true),
        ("policy:\n  products: [codex, chatgpt]\n", true),
        ("policy:\n  products: [CODEX]\n", true),
        ("policy:\n  products: [chatgpt]\n", false),
        ("policy:\n  products: [atlas]\n", false),
        ("policy:\n  products: codex\n", false),
        ("policy:\n  products: [future-product]\n", false),
        ("policy: [\n", false),
    ] {
        fs::write(&metadata, yaml).unwrap();
        let output = fixture.run("codex", &["exec", "prompt"], true);
        assert_eq!(
            output.status.success(),
            succeeds,
            "{yaml}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if !succeeds {
            assert!(
                String::from_utf8_lossy(&output.stderr).contains("selected Codex Skill visibility")
            );
        }
        assert!(!fixture.0.join("state").exists());
    }
}

#[test]
fn claude_resume_keeps_the_settings_layer_that_prevents_background_autoattach() {
    let fixture = Fixture::new();
    for args in [
        vec!["--resume"],
        vec!["--resume", "session"],
        vec!["--resume=session"],
        vec!["-r", "session"],
        vec!["-rsession"],
        vec!["--from-pr", "123"],
        vec!["--continue"],
        vec!["-c"],
    ] {
        let output = fixture.run("claude", &args, true);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rendered = String::from_utf8(output.stdout).unwrap();
        let injected = rendered
            .split("Injected args:")
            .nth(1)
            .unwrap()
            .split("Forwarded args:")
            .next()
            .unwrap();
        assert!(injected.contains("--settings"), "{args:?}: {injected}");
        assert!(injected.contains("skillOverrides"), "{args:?}: {injected}");
        assert!(injected.contains("alpha"), "{args:?}: {injected}");
        assert!(!fixture.0.join("state").exists());
    }
}
