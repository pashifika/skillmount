//! Rendering for the read-only commands.
//!
//! Every value that could come from the operating system is written as its own indexed line. A
//! joined command string is never produced: quoting inside one would be reinterpretable, and the
//! whole point of these commands is that the reader can trust what they see.

use std::ffi::OsStr;
use std::fmt::Write as _;
use std::path::Path;

use crate::agent::DiscoverySnapshot;
use crate::diagnostic::Diagnostic;
use crate::domain::{RunContext, ShadowReason, SkillCatalog};
use crate::journal::TransactionStatus;
use crate::lock::LockResource;
use crate::mount::{MountAction, MountPlan};

/// Everything a read-only command has observed, ready to render.
pub(crate) struct ReadOnlyReport<'a> {
    /// Resolved run context.
    pub(crate) context: &'a RunContext,
    /// Validated catalog with full provenance.
    pub(crate) catalog: &'a SkillCatalog,
    /// Adapter observation.
    pub(crate) snapshot: &'a DiscoverySnapshot,
    /// Deterministic plan built from the observation.
    pub(crate) plan: &'a MountPlan,
    /// Requested diagnostic verbosity.
    pub(crate) verbosity: u8,
}

impl ReadOnlyReport<'_> {
    fn verbose(&self) -> bool {
        self.verbosity > 0
    }

    /// Renders a path relative to the project root when it lies inside it.
    ///
    /// The project root is printed in full in the header, so repeating it on every line only
    /// pushes the part that differs off the edge of the terminal. Anything outside the project,
    /// such as a staging root or a Skill source, still prints in full because its location is the
    /// information the reader needs.
    fn path(&self, path: &Path) -> String {
        path.strip_prefix(&self.context.project_root).map_or_else(
            |_| os_value(path.as_os_str(), self.verbose()),
            |relative| os_value(relative.as_os_str(), self.verbose()),
        )
    }
}

/// Renders the full read-only report.
pub(crate) fn render(report: &ReadOnlyReport<'_>) -> String {
    let mut out = String::new();
    header(&mut out, report);
    sources(&mut out, report);
    overlay(&mut out, report);
    scopes(&mut out, report);
    plan(&mut out, report);
    locks(&mut out, report);
    recovery(&mut out);
    arguments(&mut out, report);
    out
}

/// Renders the brief session summary emitted immediately before the child starts.
pub(crate) fn render_session_start(report: &ReadOnlyReport<'_>) -> String {
    let selected = report
        .catalog
        .resolutions
        .iter()
        .filter(|resolution| {
            !report.plan.preserved.iter().any(|preserved| {
                preserved.comparison_key == resolution.selected.mount_name.comparison_key()
            })
        })
        .collect::<Vec<_>>();
    let skill_count = selected.len();
    let source_count = report.context.skill_sources.len();
    let override_count = report.catalog.override_count();
    let agent = match report.context.agent {
        crate::domain::AgentId::Codex => "Codex",
        crate::domain::AgentId::Claude => "Claude",
    };
    let mut output = String::new();
    let _ = writeln!(
        output,
        "Mounted {skill_count} {} from {source_count} {} for {agent} ({override_count} {}).",
        plural(skill_count, "skill", "skills"),
        plural(source_count, "source argument", "source arguments"),
        plural(override_count, "source override", "source overrides")
    );
    for resolution in selected {
        let _ = writeln!(output, "  {}", resolution.selected.mount_name);
    }
    let _ = writeln!(output, "Launching {}...", report.context.agent.label());
    output
}

const fn plural(count: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}

fn header(out: &mut String, report: &ReadOnlyReport<'_>) {
    let context = report.context;
    let field = |out: &mut String, label: &str, value: &Path| {
        let _ = writeln!(out, "{label:<15} {}", path_value(value, report.verbose()));
    };
    let _ = writeln!(
        out,
        "{:<15} {}",
        "Agent:",
        match context.agent {
            crate::domain::AgentId::Codex => "codex",
            crate::domain::AgentId::Claude => "claude",
        }
    );
    let _ = writeln!(
        out,
        "{:<15} {:?} (advisory evidence; executable not queried)",
        "Last tested:",
        crate::agent::version_spec(context.agent).last_tested_banner()
    );
    field(out, "Launch CWD:", &context.launch_cwd);
    field(out, "Project root:", &context.project_root);
    let _ = writeln!(
        out,
        "{:<15} {}",
        "Discovery:",
        report.path(&report.snapshot.discovery_entry)
    );
    let _ = writeln!(
        out,
        "{:<15} {}",
        "Backing store:",
        report.path(&report.snapshot.backing_store)
    );
    let _ = writeln!(
        out,
        "{:<15} {}",
        "Store state:",
        report.snapshot.backing_store_state.label()
    );
}

fn sources(out: &mut String, report: &ReadOnlyReport<'_>) {
    if report.context.skill_sources.is_empty() {
        return;
    }
    let _ = writeln!(out, "\nSources:");
    for source in &report.context.skill_sources {
        let _ = writeln!(
            out,
            "  [{}] {}",
            source.ordinal + 1,
            path_value(&source.input_path, report.verbose())
        );
    }
}

/// Renders which source won each logical name, and every origin it displaced.
fn overlay(out: &mut String, report: &ReadOnlyReport<'_>) {
    let overrides = report.catalog.override_count();
    let _ = writeln!(
        out,
        "\nOverlay: {} Skill(s), {overrides} source override(s)",
        report.catalog.resolutions.len()
    );
    for resolution in &report.catalog.resolutions {
        let selected = &resolution.selected;
        let displaced = resolution
            .shadowed
            .iter()
            .filter(|shadow| shadow.reason == ShadowReason::DifferentSourceOverride)
            .count();
        let marker = if displaced > 0 {
            "OVERRIDE"
        } else {
            "SELECT  "
        };
        let _ = writeln!(out, "  {marker}  {}", selected.mount_name);
        // Shadowed origins are only listed in verbose output; the summary above already reports
        // that precedence was applied, so normal output stays short enough to read before a TUI.
        if report.verbose() {
            for shadow in &resolution.shadowed {
                let _ = writeln!(
                    out,
                    "            [{}] {}  ({})",
                    shadow.origin.source_ordinal + 1,
                    path_value(&shadow.origin.source_entry, true),
                    match shadow.reason {
                        ShadowReason::DifferentSourceOverride => "different source",
                        ShadowReason::RepeatedCanonicalSource => "same canonical source",
                    }
                );
            }
        }
        let _ = writeln!(
            out,
            "         -> [{}] {}",
            selected.origin.source_ordinal + 1,
            path_value(&selected.origin.source_canonical, report.verbose())
        );
    }
}

/// Renders every namespace in the adapter's current discovery model, which makes conflicts
/// explainable.
fn scopes(out: &mut String, report: &ReadOnlyReport<'_>) {
    let _ = writeln!(out, "\nDiscovery scopes:");
    for scope in &report.snapshot.scopes {
        let _ = writeln!(
            out,
            "  {:<22} {:<24} {}",
            scope.kind.label(),
            scope.state.kind.label(),
            report.path(&scope.state.entry)
        );
        for alias in &scope.aliases {
            let _ = writeln!(
                out,
                "  {:<22} {:<24} {}",
                "",
                "also reached through",
                report.path(alias)
            );
        }
        if report.verbose() {
            for (index, target) in scope.state.link_chain.iter().enumerate() {
                let _ = writeln!(
                    out,
                    "      link[{index}]                    {}",
                    path_value(target, true)
                );
            }
            if let Some(terminal) = &scope.state.terminal {
                let _ = writeln!(
                    out,
                    "      terminal                     {}",
                    path_value(terminal, true)
                );
            }
            for existing in scope.existing_skills.values().flatten() {
                let _ = writeln!(
                    out,
                    "      {:<28} {}",
                    os_value(&existing.raw_name, true),
                    existing.kind.label()
                );
            }
        }
    }
}

fn plan(out: &mut String, report: &ReadOnlyReport<'_>) {
    let _ = writeln!(out, "\nPlan:");
    if report.plan.actions.is_empty() && report.plan.preserved.is_empty() {
        let _ = writeln!(out, "  (nothing to do)");
    }
    for action in &report.plan.actions {
        match &action.operation {
            MountAction::CreateDirectory { path } => {
                let _ = writeln!(out, "  MKDIR  {}", report.path(path));
            }
            MountAction::CreateDirectoryLink {
                source,
                destination,
                ..
            } => {
                let _ = writeln!(out, "  LINK   {}", report.path(destination));
                let _ = writeln!(out, "         -> {}", report.path(source));
            }
            MountAction::ReuseExistingLink {
                source,
                destination,
            } => {
                let _ = writeln!(out, "  REUSE  {}", report.path(destination));
                let _ = writeln!(out, "         -> {}", report.path(source));
            }
        }
        if report.verbose() {
            let _ = writeln!(
                out,
                "         #{} precondition={}",
                action.id,
                action.expected_precondition.label()
            );
        }
    }
    for preserved in &report.plan.preserved {
        let _ = writeln!(out, "  KEEP   {}", report.path(&preserved.existing));
        let _ = writeln!(
            out,
            "         omitted {}",
            report.path(&preserved.omitted_source)
        );
    }
}

/// Renders lock resources as observations, never as a promise that applying will not wait.
fn locks(out: &mut String, report: &ReadOnlyReport<'_>) {
    if report.snapshot.lock_resources.is_empty() {
        return;
    }
    let _ = writeln!(out, "\nLock resources:");
    for resource in &report.snapshot.lock_resources {
        let _ = writeln!(
            out,
            "  {:<16} {}",
            resource.kind.label(),
            report.path(&resource.path)
        );
        if report.verbose() {
            verbose_lock_identity(out, resource);
        }
    }
}

fn verbose_lock_identity(out: &mut String, resource: &LockResource) {
    let _ = writeln!(
        out,
        "                   anchor {}",
        path_value(&resource.identity.anchor, true)
    );
    let _ = writeln!(
        out,
        "                   suffix {}",
        path_value(&resource.identity.suffix, true)
    );
    match &resource.identity.physical {
        Some(identity) => {
            let _ = writeln!(out, "                   physical {identity}");
        }
        None => {
            let _ = writeln!(
                out,
                "                   physical (resource does not exist yet)"
            );
        }
    }
}

/// Reports the transaction journals a normal run would reconcile.
///
/// Reading is the only thing that happens here: nothing is locked, recovered, rewritten, or
/// removed, which is what keeps `--dry-run` and `inspect` read-only. A dry run therefore cannot
/// promise that a listed transaction is actually stale — eligibility needs the locks, and taking
/// them is a side effect. It reports what it can see and what it would examine.
fn recovery(out: &mut String) {
    let Ok(scan) = crate::journal::store::scan() else {
        return;
    };
    if scan.journals.is_empty() && scan.rejected.is_empty() {
        return;
    }

    let _ = writeln!(out, "\nRecovery:");
    for scanned in &scan.journals {
        let verb = match scanned.journal.status {
            TransactionStatus::Supervising => "WOULD QUARANTINE",
            status if status.is_terminal() => "WOULD KEEP      ",
            _ => "WOULD RECOVER   ",
        };
        let _ = writeln!(
            out,
            "  {verb}  {}  ({}, {} owned action(s))",
            path_value(&scanned.path, false),
            scanned.journal.status.label(),
            scanned.journal.reversible_actions().count()
        );
    }
    for rejected in &scan.rejected {
        let _ = writeln!(
            out,
            "  WOULD RETAIN   {}  (unreadable: {})",
            path_value(&rejected.path, false),
            text_value(&rejected.reason)
        );
    }
}

/// Renders each argument layer separately, then the effective argv by index.
fn arguments(out: &mut String, report: &ReadOnlyReport<'_>) {
    let launch = &report.plan.launch;
    let verbose = report.verbose();

    let _ = writeln!(out, "\nExecutable:");
    let _ = writeln!(out, "  {}", path_value(&launch.executable, verbose));

    let layer = |out: &mut String, label: &str, values: &[std::ffi::OsString]| {
        let _ = writeln!(out, "{label}:");
        if values.is_empty() {
            let _ = writeln!(out, "  (none)");
            return;
        }
        for (index, value) in values.iter().enumerate() {
            let _ = writeln!(out, "  [{index}] {}", os_value(value, verbose));
        }
    };
    layer(out, "Injected args", &launch.injected_args);
    layer(out, "Forwarded args", &launch.passthrough_args);

    let _ = writeln!(out, "Environment overrides:");
    if launch.environment_overrides.is_empty() {
        let _ = writeln!(out, "  (none)");
    } else {
        for (name, value) in &launch.environment_overrides {
            let _ = writeln!(
                out,
                "  {} = {}",
                os_value(name, verbose),
                os_value(value, verbose)
            );
        }
    }

    let _ = writeln!(out, "Effective argv:");
    for (index, value) in launch.effective_argv().iter().enumerate() {
        let _ = writeln!(out, "  argv[{index}] = {}", os_value(value, verbose));
    }
}

/// Renders warnings collected by the catalog and the adapter.
pub(crate) fn render_warnings(catalog: &SkillCatalog, snapshot: &DiscoverySnapshot) -> Vec<String> {
    catalog
        .warnings
        .iter()
        .chain(snapshot.warnings.iter())
        .map(|warning: &Diagnostic| warning.message.clone())
        .collect()
}

pub(crate) fn path_value(path: &Path, verbose: bool) -> String {
    os_value(path.as_os_str(), verbose)
}

/// Marks a value that had to be escaped because it is not representable as text.
const ESCAPED_PREFIX: &str = "escaped:";

/// Renders a platform-native value without allowing it to control the diagnostics stream.
///
/// A value that is already text is its own reversible representation, so it is printed verbatim.
/// Escaping it unconditionally would double every separator in a Windows path and make the common
/// case unreadable for no gain. Values that are not representable as text are escaped in verbose
/// output. Valid text is also escaped when it contains line, terminal-control, or bidirectional
/// formatting characters, even in concise output, so one path can never forge another finding. A
/// literal value that would collide with the escape prefix is escaped as well.
pub(crate) fn os_value(value: &OsStr, verbose: bool) -> String {
    let contains_display_control = value.to_string_lossy().chars().any(is_display_control);
    match value.to_str() {
        Some(text) if !text.starts_with(ESCAPED_PREFIX) && !contains_display_control => {
            text.to_owned()
        }
        None if !verbose && !contains_display_control => Path::new(value).display().to_string(),
        _ => format!("{ESCAPED_PREFIX}{}", escaped(value)),
    }
}

/// Escapes characters that could forge a line or alter terminal display in arbitrary text.
///
/// Unlike [`os_value`], this helper does not add an encoding marker or escape backslashes. It is
/// for already-rendered diagnostic prose whose structure must remain readable while control and
/// bidirectional formatting characters become visible.
pub(crate) fn text_value(value: &str) -> String {
    let mut rendered = String::with_capacity(value.len());
    for character in value.chars() {
        if is_display_control(character) {
            let _ = write!(rendered, "\\u{{{:X}}}", u32::from(character));
        } else {
            rendered.push(character);
        }
    }
    rendered
}

/// Escapes a value so the original bytes can be recovered from the text.
///
/// A backslash doubles, and anything that is not valid text is written as an explicit escape.
/// The result is never a shell word: it is a display form, and nothing in this crate feeds it
/// back into a command line.
#[cfg(unix)]
fn escaped(value: &OsStr) -> String {
    use std::os::unix::ffi::OsStrExt;

    let mut out = String::new();
    for chunk in value.as_bytes().utf8_chunks() {
        for character in chunk.valid().chars() {
            if character == '\\' {
                out.push_str("\\\\");
            } else if is_display_control(character) {
                let _ = write!(out, "\\u{{{:X}}}", u32::from(character));
            } else {
                out.push(character);
            }
        }
        for byte in chunk.invalid() {
            let _ = write!(out, "\\x{byte:02X}");
        }
    }
    out
}

/// Escapes a value so the original UTF-16 units can be recovered from the text.
#[cfg(windows)]
fn escaped(value: &OsStr) -> String {
    use std::os::windows::ffi::OsStrExt;

    let units = value.encode_wide().collect::<Vec<_>>();
    let mut out = String::new();
    for decoded in char::decode_utf16(units) {
        match decoded {
            Ok('\\') => out.push_str("\\\\"),
            Ok(character) if is_display_control(character) => {
                let _ = write!(out, "\\u{{{:X}}}", u32::from(character));
            }
            Ok(character) => out.push(character),
            Err(error) => {
                let _ = write!(out, "\\u{:04X}", error.unpaired_surrogate());
            }
        }
    }
    out
}

fn is_display_control(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061C}'
                | '\u{200E}'
                | '\u{200F}'
                | '\u{2028}'..='\u{202E}'
                | '\u{2066}'..='\u{206F}'
        )
}

#[cfg(test)]
mod tests {
    use super::{ESCAPED_PREFIX, escaped, os_value, text_value};
    use std::ffi::OsString;

    #[test]
    fn ordinary_values_render_unchanged() {
        assert_eq!(os_value(&OsString::from("plain-name"), true), "plain-name");
        assert_eq!(os_value(&OsString::from("plain-name"), false), "plain-name");
    }

    #[test]
    fn a_windows_style_path_keeps_its_single_separators_in_verbose_output() {
        let value = OsString::from(r"C:\Users\example\.codex\skills");

        assert_eq!(os_value(&value, true), r"C:\Users\example\.codex\skills");
    }

    #[test]
    fn a_backslash_is_doubled_so_the_escape_stays_reversible() {
        assert_eq!(escaped(&OsString::from("a\\b")), "a\\\\b");
    }

    #[test]
    fn a_value_colliding_with_the_marker_is_escaped_so_the_forms_stay_distinct() {
        let value = OsString::from("escaped:already");

        let rendered = os_value(&value, true);

        assert_eq!(rendered, "escaped:escaped:already");
        assert!(rendered.starts_with(ESCAPED_PREFIX));
    }

    #[test]
    fn line_and_terminal_controls_are_escaped_in_every_output_mode() {
        let value = OsString::from("line\n\u{1B}]52;forged\u{7}");
        let expected = "escaped:line\\u{A}\\u{1B}]52;forged\\u{7}";

        assert_eq!(os_value(&value, true), expected);
        assert_eq!(os_value(&value, false), expected);
        assert!(!os_value(&value, true).contains('\n'));
        assert!(!os_value(&value, true).contains('\u{1B}'));
    }

    #[test]
    fn bidirectional_formatting_is_visible_and_reversible() {
        let value = OsString::from("safe\u{202E}txt");

        assert_eq!(os_value(&value, true), "escaped:safe\\u{202E}txt");
    }

    #[test]
    fn arbitrary_diagnostic_text_cannot_forge_lines_or_terminal_sequences() {
        assert_eq!(
            text_value("failure\n[PASS] forged\u{1B}]52;payload\u{7}"),
            "failure\\u{A}[PASS] forged\\u{1B}]52;payload\\u{7}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn invalid_bytes_survive_as_explicit_escapes() {
        use std::os::unix::ffi::OsStringExt;

        let value = OsString::from_vec(vec![b'a', 0xFF, b'b']);

        assert_eq!(escaped(&value), "a\\xFFb");
        assert_eq!(
            os_value(&value, true),
            "escaped:a\\xFFb",
            "a value that is not text is marked so a reader knows to decode it"
        );
        assert_ne!(
            os_value(&value, false),
            os_value(&value, true),
            "the lossy form is only used when verbose output was not requested"
        );
    }

    #[cfg(windows)]
    #[test]
    fn unpaired_surrogates_survive_as_explicit_escapes() {
        use std::os::windows::ffi::OsStringExt;

        // A lone high surrogate is the Windows counterpart of an invalid UTF-8 byte: the value is
        // a legal filename that no lossy rendering can round-trip.
        let value = OsString::from_wide(&[u16::from(b'a'), 0xD800, u16::from(b'b')]);

        assert_eq!(escaped(&value), "a\\uD800b");
        assert_eq!(
            os_value(&value, true),
            "escaped:a\\uD800b",
            "a value that is not text is marked so a reader knows to decode it"
        );
        assert_ne!(
            os_value(&value, false),
            os_value(&value, true),
            "the lossy form is only used when verbose output was not requested"
        );
    }

    #[test]
    fn shell_metacharacters_are_not_quoted_into_a_command_string() {
        let value = OsString::from("a b\"c'd;e");

        let rendered = escaped(&value);

        assert_eq!(rendered, "a b\"c'd;e");
        assert!(
            !rendered.starts_with('\'') && !rendered.starts_with('"'),
            "values are rendered as data, never as shell words"
        );
    }
}
