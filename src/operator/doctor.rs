//! Read-only environment diagnosis with isolated capability probes.

use std::fmt::Write as _;
use std::fs::OpenOptions;
use std::io::Write as IoWrite;
use std::path::{Path, PathBuf};

use crate::agent::claude::ClaudeAdapter;
use crate::agent::codex::CodexAdapter;
use crate::agent::{AgentAdapter, DiscoverySnapshot};
use crate::cli::DoctorInput;
use crate::domain::{AgentId, LinkMode, RunContext};
use crate::error::{AppError, ExitCategory};
use crate::journal::{TransactionId, TransactionStatus, store};
use crate::link::{CreatedLink, LinkRequest, OwnedDirectory, RemoveOutcome, platform_backend};
use crate::lock::acquire::{AdvisoryLockState, observe};
use crate::mount::resolve::{PathKind, ResolvedEntry, classify};
use crate::paths::{resolve_operator_context, resolve_operator_project_root};
use crate::render::path_value;

use super::CommandOutcome;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FindingSeverity {
    Pass,
    Warning,
    Failure,
    Unverified,
}

impl FindingSeverity {
    const fn label(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Warning => "WARN",
            Self::Failure => "FAIL",
            Self::Unverified => "UNVERIFIED",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorFinding {
    severity: FindingSeverity,
    component: String,
    message: String,
}

impl DoctorFinding {
    fn new(
        severity: FindingSeverity,
        component: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            severity,
            component: component.into(),
            message: message.into(),
        }
    }
}

/// Runs every read-only doctor check and returns its stable rendering.
pub(crate) fn run(input: &DoctorInput, invocation_cwd: &Path) -> Result<CommandOutcome, AppError> {
    let project_root =
        resolve_operator_project_root(input.project_root.as_deref(), invocation_cwd)?;
    let mut findings = Vec::new();

    let codex = check_agent(
        AgentId::Codex,
        input.codex_bin.as_deref(),
        &project_root,
        invocation_cwd,
        &mut findings,
    );
    let claude = check_agent(
        AgentId::Claude,
        input.claude_bin.as_deref(),
        &project_root,
        invocation_cwd,
        &mut findings,
    );

    for (label, relative) in [
        ("project .agents/skills", ".agents/skills"),
        ("project .codex/skills", ".codex/skills"),
        ("project .claude/skills", ".claude/skills"),
    ] {
        check_layout(label, &project_root.join(relative), &mut findings);
    }

    if let Some(context) = codex.as_ref() {
        check_discovery(context, &CodexAdapter, &mut findings);
    }
    if let Some(context) = claude.as_ref() {
        check_discovery(context, &ClaudeAdapter, &mut findings);
    }
    check_link_capabilities(&mut findings);
    check_transactions(&mut findings);

    let output = render_report(&project_root, &findings);
    let code = if findings
        .iter()
        .any(|finding| finding.severity == FindingSeverity::Failure)
    {
        ExitCategory::Data.code()
    } else {
        0
    };
    Ok(CommandOutcome { output, code })
}

fn check_link_capabilities(findings: &mut Vec<DoctorFinding>) {
    findings.push(probe_link_capability(
        LinkMode::Symlink,
        "symlink capability",
    ));
    #[cfg(windows)]
    findings.push(probe_link_capability(
        LinkMode::Junction,
        "junction capability",
    ));
    #[cfg(not(windows))]
    findings.push(DoctorFinding::new(
        FindingSeverity::Unverified,
        "junction capability",
        "junctions are a Windows-only link implementation and were not probed on this host",
    ));
}

fn probe_link_capability(mode: LinkMode, component: &str) -> DoctorFinding {
    let backend = platform_backend();
    let transaction = TransactionId::mint();
    let root_path = std::env::temp_dir().join(format!("skillmount-doctor-{transaction}"));
    let (root, source) = match create_probe_directories(&root_path, mode, component) {
        Ok(directories) => directories,
        Err(finding) => return finding,
    };
    let (sentinel_path, sentinel) =
        match create_probe_sentinel(&root, &source, mode, component, &transaction) {
            Ok(sentinel) => sentinel,
            Err(finding) => return finding,
        };
    let source_canonical = match backend.canonical_directory(&source.path) {
        Ok(source) => source,
        Err(error) => {
            let cleanup =
                cleanup_probe(&root, Some(&source), Some(&sentinel_path), None, &sentinel);
            return probe_failure(
                mode,
                component,
                &format!("cannot identify the isolated source directory: {error}"),
                &cleanup,
            );
        }
    };
    let link_path = root.path.join("probe-link");
    let created = match backend.create_directory_link(&LinkRequest {
        source: source_canonical,
        staged_path: link_path,
        mode,
    }) {
        Ok(created) => Some(created),
        Err(error) => {
            let cleanup =
                cleanup_probe(&root, Some(&source), Some(&sentinel_path), None, &sentinel);
            return probe_failure(
                mode,
                component,
                &format!("the isolated link probe failed: {error}"),
                &cleanup,
            );
        }
    };

    let cleanup = cleanup_probe(
        &root,
        Some(&source),
        Some(&sentinel_path),
        created.as_ref(),
        &sentinel,
    );
    if cleanup.errors.is_empty() {
        DoctorFinding::new(
            FindingSeverity::Pass,
            component,
            format!(
                "created and ownership-verified a {} only inside {}, removed it, and confirmed the source sentinel survived",
                created
                    .as_ref()
                    .map_or("directory link", |link| link.kind.label()),
                path_value(&root_path, true)
            ),
        )
    } else {
        probe_failure(
            mode,
            component,
            "the isolated link was created but verified cleanup did not complete",
            &cleanup,
        )
    }
}

fn create_probe_directories(
    root_path: &Path,
    mode: LinkMode,
    component: &str,
) -> Result<(OwnedDirectory, OwnedDirectory), DoctorFinding> {
    let backend = platform_backend();
    let root = match backend.create_directory(root_path) {
        Ok(root) => root,
        Err(error) => {
            return Err(DoctorFinding::new(
                FindingSeverity::Failure,
                component,
                format!(
                    "cannot create the isolated probe directory {}: {error}; the project was not touched",
                    path_value(root_path, true)
                ),
            ));
        }
    };
    if let Err(error) = crate::state::restrict_to_owner(root_path) {
        let cleanup = cleanup_probe(&root, None, None, None, b"");
        return Err(probe_failure(
            mode,
            component,
            &format!("cannot restrict the isolated probe directory to its owner: {error}"),
            &cleanup,
        ));
    }

    let source_path = root_path.join("source");
    let source = match backend.create_directory(&source_path) {
        Ok(source) => source,
        Err(error) => {
            let cleanup = cleanup_probe(&root, None, None, None, b"");
            return Err(probe_failure(
                mode,
                component,
                &format!("cannot create the isolated probe source: {error}"),
                &cleanup,
            ));
        }
    };
    Ok((root, source))
}

fn create_probe_sentinel(
    root: &OwnedDirectory,
    source: &OwnedDirectory,
    mode: LinkMode,
    component: &str,
    transaction: &TransactionId,
) -> Result<(PathBuf, Vec<u8>), DoctorFinding> {
    let sentinel_path = source.path.join("source-sentinel");
    let sentinel = format!("skillmount-doctor-source-{transaction}\n").into_bytes();
    let mut sentinel_file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&sentinel_path)
    {
        Ok(file) => file,
        Err(error) => {
            let cleanup = cleanup_probe(root, Some(source), None, None, &sentinel);
            return Err(probe_failure(
                mode,
                component,
                &format!("cannot create the isolated source sentinel: {error}"),
                &cleanup,
            ));
        }
    };
    if let Err(error) = sentinel_file.write_all(&sentinel) {
        drop(sentinel_file);
        let cleanup = cleanup_probe(root, Some(source), Some(&sentinel_path), None, &sentinel);
        return Err(probe_failure(
            mode,
            component,
            &format!("cannot write the isolated source sentinel: {error}"),
            &cleanup,
        ));
    }
    drop(sentinel_file);
    if let Err(error) = crate::state::restrict_to_owner(&sentinel_path) {
        let cleanup = cleanup_probe(root, Some(source), Some(&sentinel_path), None, &sentinel);
        return Err(probe_failure(
            mode,
            component,
            &format!("cannot restrict the isolated source sentinel to its owner: {error}"),
            &cleanup,
        ));
    }
    Ok((sentinel_path, sentinel))
}

struct ProbeCleanup {
    errors: Vec<String>,
    retained_root: bool,
}

fn cleanup_probe(
    root: &OwnedDirectory,
    source: Option<&OwnedDirectory>,
    sentinel_path: Option<&Path>,
    link: Option<&CreatedLink>,
    expected_sentinel: &[u8],
) -> ProbeCleanup {
    let backend = platform_backend();
    let mut errors = Vec::new();
    if let Some(link) = link {
        match backend.remove_link_entry(link) {
            Ok(RemoveOutcome::Removed | RemoveOutcome::AlreadyAbsent) => {}
            Ok(other) => errors.push(format!(
                "probe link {} was retained after ownership verification returned {other:?}",
                path_value(&link.path, true)
            )),
            Err(error) => errors.push(format!(
                "probe link {} could not be removed: {error}",
                path_value(&link.path, true)
            )),
        }
    }

    if let Some(sentinel_path) = sentinel_path {
        match std::fs::symlink_metadata(sentinel_path) {
            Ok(metadata)
                if metadata.file_type().is_file() && !metadata.file_type().is_symlink() =>
            {
                match std::fs::read(sentinel_path) {
                    Ok(contents) if contents == expected_sentinel => {
                        if let Err(error) = std::fs::remove_file(sentinel_path) {
                            errors.push(format!(
                                "verified source sentinel {} could not be removed: {error}",
                                path_value(sentinel_path, true)
                            ));
                        }
                    }
                    Ok(_) => errors.push(format!(
                        "source sentinel {} changed and was retained",
                        path_value(sentinel_path, true)
                    )),
                    Err(error) => errors.push(format!(
                        "source sentinel {} could not be verified and was retained: {error}",
                        path_value(sentinel_path, true)
                    )),
                }
            }
            Ok(_) => errors.push(format!(
                "source sentinel {} is no longer the created regular file and was retained",
                path_value(sentinel_path, true)
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => errors.push(format!(
                "source sentinel {} disappeared during the probe",
                path_value(sentinel_path, true)
            )),
            Err(error) => errors.push(format!(
                "source sentinel {} could not be inspected: {error}",
                path_value(sentinel_path, true)
            )),
        }
    }

    if let Some(source) = source {
        match backend.remove_empty_directory(source) {
            Ok(RemoveOutcome::Removed | RemoveOutcome::AlreadyAbsent) => {}
            Ok(other) => errors.push(format!(
                "probe source {} was retained after ownership verification returned {other:?}",
                path_value(&source.path, true)
            )),
            Err(error) => errors.push(format!(
                "probe source {} could not be removed: {error}",
                path_value(&source.path, true)
            )),
        }
    }
    let retained_root = match backend.remove_empty_directory(root) {
        Ok(RemoveOutcome::Removed | RemoveOutcome::AlreadyAbsent) => false,
        Ok(other) => {
            errors.push(format!(
                "probe root {} was retained after ownership verification returned {other:?}",
                path_value(&root.path, true)
            ));
            true
        }
        Err(error) => {
            errors.push(format!(
                "probe root {} could not be removed: {error}",
                path_value(&root.path, true)
            ));
            true
        }
    };
    ProbeCleanup {
        errors,
        retained_root,
    }
}

fn probe_failure(
    mode: LinkMode,
    component: &str,
    reason: &str,
    cleanup: &ProbeCleanup,
) -> DoctorFinding {
    let severity = if cfg!(windows) && mode == LinkMode::Symlink && !cleanup.retained_root {
        FindingSeverity::Warning
    } else {
        FindingSeverity::Failure
    };
    let cleanup_detail = if cleanup.errors.is_empty() {
        "all verified probe entries were removed".to_owned()
    } else {
        cleanup.errors.join("; ")
    };
    let next_action = if cfg!(windows) && mode == LinkMode::Symlink {
        "SkillMount will not elevate privileges; use an evidenced junction policy or enable symlink capability outside SkillMount"
    } else {
        "inspect any retained probe path before removing it manually"
    };
    DoctorFinding::new(
        severity,
        component,
        format!("{reason}; {cleanup_detail}; {next_action}"),
    )
}

fn check_agent(
    agent: AgentId,
    explicit: Option<&Path>,
    project_root: &Path,
    invocation_cwd: &Path,
    findings: &mut Vec<DoctorFinding>,
) -> Option<RunContext> {
    let component = format!("{} executable", agent.label());
    let context = match resolve_operator_context(agent, project_root, explicit, invocation_cwd) {
        Ok(context) => context,
        Err(error) => {
            findings.push(DoctorFinding::new(
                FindingSeverity::Failure,
                component,
                format!(
                    "{}; install the pinned agent or pass its explicit binary path",
                    render_error(&error)
                ),
            ));
            return None;
        }
    };

    let result = match agent {
        AgentId::Codex => crate::agent::codex::verify_managed_configuration(&context)
            .and_then(|()| crate::agent::codex::reported_version(&context))
            .and_then(|version| {
                crate::agent::codex::verify_version_text(&version).map(|()| version)
            }),
        AgentId::Claude => crate::agent::claude::verify_environment()
            .and_then(|()| crate::agent::claude::reported_version(&context))
            .and_then(|version| {
                crate::agent::claude::verify_version_text(&version).map(|()| version)
            }),
    };
    match result {
        Ok(version) => findings.push(DoctorFinding::new(
            FindingSeverity::Pass,
            component,
            format!("{} reports {version}", path_value(&context.agent_bin, true)),
        )),
        Err(error) => findings.push(DoctorFinding::new(
            FindingSeverity::Failure,
            component,
            format!(
                "{}: {error}; install the pinned release before starting a mounted session",
                path_value(&context.agent_bin, true)
            ),
        )),
    }
    Some(context)
}

fn render_error(error: &AppError) -> String {
    match error {
        AppError::MissingInput { path, reason } => {
            format!("{}: {reason}", path_value(path, true))
        }
        _ => error.to_string(),
    }
}

fn check_layout(component: &str, entry: &Path, findings: &mut Vec<DoctorFinding>) {
    match classify(entry) {
        Ok(resolved) if resolved.kind.is_ambiguous() => {
            findings.push(DoctorFinding::new(
                FindingSeverity::Failure,
                component,
                format!(
                    "{}; exact chain: {}; no changes were made—account for the entry, then repair or remove only that discovery link",
                    resolved.kind.label(),
                    render_chain(&resolved)
                ),
            ));
        }
        Ok(resolved) => {
            let detail = if resolved.kind == PathKind::Missing {
                format!(
                    "{} is missing and available for session-owned creation",
                    path_value(entry, true)
                )
            } else {
                format!(
                    "{} resolves as {}",
                    render_chain(&resolved),
                    resolved.kind.label()
                )
            };
            findings.push(DoctorFinding::new(FindingSeverity::Pass, component, detail));
        }
        Err(error) => findings.push(DoctorFinding::new(
            FindingSeverity::Failure,
            component,
            format!("{error}; no changes were made—restore inspectable permissions and retry"),
        )),
    }
}

fn check_discovery(
    context: &RunContext,
    adapter: &dyn AgentAdapter,
    findings: &mut Vec<DoctorFinding>,
) {
    let component = format!("{} discovery", context.agent.label());
    match adapter.inspect_discovery(context) {
        Ok(snapshot) => {
            findings.push(DoctorFinding::new(
                FindingSeverity::Pass,
                component.clone(),
                format!(
                    "inspected {} scope(s) and {} visible logical Skill name(s)",
                    snapshot.scopes.len(),
                    snapshot.visible_skills.len()
                ),
            ));
            discovery_observations(&component, &snapshot, findings);
            check_resource_locks(&component, &snapshot, findings);
        }
        Err(error) => findings.push(DoctorFinding::new(
            FindingSeverity::Failure,
            component,
            format!("{error}; no discovery entry was changed"),
        )),
    }
}

fn check_resource_locks(
    component: &str,
    snapshot: &DiscoverySnapshot,
    findings: &mut Vec<DoctorFinding>,
) {
    match observe(&snapshot.lock_resources) {
        Ok(observations) => {
            let held = observations
                .iter()
                .filter(|entry| matches!(&entry.state, AdvisoryLockState::Held { .. }))
                .count();
            if held == 0 {
                findings.push(DoctorFinding::new(
                    FindingSeverity::Pass,
                    format!("{component} locks"),
                    format!(
                        "{} advisory resource key(s) are free; no lock files were created",
                        observations.len()
                    ),
                ));
            } else {
                for entry in observations
                    .into_iter()
                    .filter(|entry| matches!(&entry.state, AdvisoryLockState::Held { .. }))
                {
                    let holder = match entry.state {
                        AdvisoryLockState::Held {
                            holder: Some(holder),
                        } => {
                            format!("; holder text (diagnostic only): {holder}")
                        }
                        _ => String::new(),
                    };
                    findings.push(DoctorFinding::new(
                        FindingSeverity::Warning,
                        format!("{component} locks"),
                        format!(
                            "the OS advisory lock for {} is held{holder}; another SkillMount session is active and was left alone",
                            entry
                                .resources
                                .iter()
                                .map(|path| path_value(path, true))
                                .collect::<Vec<_>>()
                                .join(", ")
                        ),
                    ));
                }
            }
        }
        Err(error) => findings.push(DoctorFinding::new(
            FindingSeverity::Failure,
            format!("{component} locks"),
            format!("{error}; no lock state was changed"),
        )),
    }
}

fn discovery_observations(
    component: &str,
    snapshot: &DiscoverySnapshot,
    findings: &mut Vec<DoctorFinding>,
) {
    for warning in snapshot
        .warnings
        .iter()
        .chain(snapshot.scopes.iter().flat_map(|scope| &scope.warnings))
    {
        findings.push(DoctorFinding::new(
            FindingSeverity::Warning,
            component,
            warning.message.clone(),
        ));
    }
    for (name, visible) in &snapshot.visible_skills {
        if visible.len() > 1 {
            findings.push(DoctorFinding::new(
                FindingSeverity::Warning,
                component,
                format!(
                    "logical Skill {name} is visible from {} discovery entries; review scope precedence before mounting another copy",
                    visible.len()
                ),
            ));
        }
    }
}

fn check_transactions(findings: &mut Vec<DoctorFinding>) {
    let scan = match store::scan() {
        Ok(scan) => scan,
        Err(error) => {
            findings.push(DoctorFinding::new(
                FindingSeverity::Failure,
                "transaction state",
                format!("{error}; no transaction state was changed"),
            ));
            return;
        }
    };
    if scan.journals.is_empty() && scan.rejected.is_empty() {
        findings.push(DoctorFinding::new(
            FindingSeverity::Pass,
            "transaction state",
            "no SkillMount journals found",
        ));
        return;
    }
    for rejected in scan.rejected {
        findings.push(DoctorFinding::new(
            FindingSeverity::Failure,
            "transaction state",
            format!(
                "journal {} is unreadable or corrupt: {}; retain it and account for every recorded path before manual action",
                path_value(&rejected.path, true),
                rejected.reason
            ),
        ));
    }
    for scanned in scan.journals {
        let observations = match observe(&scanned.journal.lock_resources()) {
            Ok(observations) => observations,
            Err(error) => {
                findings.push(DoctorFinding::new(
                    FindingSeverity::Failure,
                    "transaction state",
                    format!(
                        "cannot observe locks for {}: {error}; the journal and its entries were left alone",
                        path_value(&scanned.path, true)
                    ),
                ));
                continue;
            }
        };
        if let Some(active) = observations
            .iter()
            .find(|entry| matches!(&entry.state, AdvisoryLockState::Held { .. }))
        {
            let holder = match &active.state {
                AdvisoryLockState::Held {
                    holder: Some(holder),
                } => {
                    format!("; holder text (diagnostic only): {holder}")
                }
                _ => String::new(),
            };
            findings.push(DoctorFinding::new(
                FindingSeverity::Warning,
                "transaction state",
                format!(
                    "{} is {} and its OS advisory lock is held{holder}; the session is active and was left alone",
                    path_value(&scanned.path, true),
                    scanned.journal.status.label()
                ),
            ));
            continue;
        }
        let (severity, action) = match scanned.journal.status {
            TransactionStatus::Completed => (
                FindingSeverity::Warning,
                "terminal completed journal remains; it owns no pending cleanup",
            ),
            TransactionStatus::Kept => (
                FindingSeverity::Warning,
                "mounts were intentionally kept; run asm cleanup for this project when finished",
            ),
            TransactionStatus::Supervising => (
                FindingSeverity::Unverified,
                "a child process domain may still use these mounts even though wrapper locks are free; confirm every related process has exited before explicit cleanup",
            ),
            _ => (
                FindingSeverity::Warning,
                "transaction is incomplete; run asm cleanup for its project after confirming no session is active",
            ),
        };
        findings.push(DoctorFinding::new(
            severity,
            "transaction state",
            format!(
                "{} is {}: {action}",
                path_value(&scanned.path, true),
                scanned.journal.status.label()
            ),
        ));
    }
}

fn render_chain(resolved: &ResolvedEntry) -> String {
    let mut values = Vec::with_capacity(resolved.link_chain.len() + 1);
    values.push(path_value(&resolved.entry, true));
    values.extend(
        resolved
            .link_chain
            .iter()
            .map(|target| path_value(target, true)),
    );
    values.join(" -> ")
}

fn render_report(project_root: &Path, findings: &[DoctorFinding]) -> String {
    let mut output = String::new();
    let _ = writeln!(output, "SkillMount doctor");
    let _ = writeln!(output, "Project root: {}\n", path_value(project_root, true));
    for finding in findings {
        let _ = writeln!(
            output,
            "[{}] {}: {}",
            finding.severity.label(),
            finding.component,
            finding.message
        );
    }
    let count = |severity| {
        findings
            .iter()
            .filter(|finding| finding.severity == severity)
            .count()
    };
    let _ = writeln!(
        output,
        "\nSummary: {} pass, {} warning, {} failure, {} unverified",
        count(FindingSeverity::Pass),
        count(FindingSeverity::Warning),
        count(FindingSeverity::Failure),
        count(FindingSeverity::Unverified)
    );
    output
}
