use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::{CatalogRequest, resolve_catalog};
use crate::domain::{AgentId, ShadowReason, SourceOccurrence, ValidationLevel};
use crate::error::{AppError, CatalogError, ExitCategory};
use crate::test_support::symlink_file_or_skip as guarded_symlink_file;

struct TestDir(PathBuf);

impl TestDir {
    fn new(label: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "skillmount-catalog-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("fixture should be created");
        Self(path)
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn valid_skill(parent: &Path, name: &str) -> PathBuf {
    skill_with(
        parent,
        name,
        &format!("---\nname: {name}\ndescription: {name} description\n---\n# {name}\n"),
    )
}

fn skill_with(parent: &Path, name: &str, contents: &str) -> PathBuf {
    let path = parent.join(name);
    fs::create_dir_all(&path).expect("Skill directory should be created");
    fs::write(path.join("SKILL.md"), contents).expect("SKILL.md should be written");
    path
}

fn occurrences(paths: &[PathBuf]) -> Vec<SourceOccurrence> {
    paths
        .iter()
        .enumerate()
        .map(|(ordinal, path)| SourceOccurrence {
            ordinal,
            input_path: path.clone(),
            resolved_path: path.clone(),
        })
        .collect()
}

fn resolve(
    paths: &[PathBuf],
    agent: AgentId,
    validation: ValidationLevel,
) -> Result<crate::domain::SkillCatalog, AppError> {
    resolve_catalog(
        &occurrences(paths),
        &CatalogRequest {
            agent,
            validation,
            destination_stores: &[],
        },
    )
}

fn selected_names(catalog: &crate::domain::SkillCatalog) -> Vec<&str> {
    catalog
        .resolutions
        .iter()
        .map(|resolution| resolution.selected.mount_name.as_str())
        .collect()
}

#[test]
fn direct_skill_and_catalog_classification_are_non_recursive_and_deterministic() {
    let fixture = TestDir::new("classification");
    let direct = valid_skill(&fixture.0, "direct");
    let catalog = fixture.0.join("catalog");
    fs::create_dir_all(&catalog).expect("catalog fixture");
    valid_skill(&catalog, "zeta");
    valid_skill(&catalog, "alpha");
    fs::create_dir_all(catalog.join("notes")).expect("notes fixture");
    valid_skill(&catalog.join("nested"), "ignored");

    let direct_result = resolve(&[direct], AgentId::Codex, ValidationLevel::Basic)
        .expect("direct Skill should resolve");
    let catalog_result = resolve(&[catalog], AgentId::Codex, ValidationLevel::Basic)
        .expect("catalog should resolve");

    assert_eq!(selected_names(&direct_result), ["direct"]);
    assert_eq!(selected_names(&catalog_result), ["alpha", "zeta"]);
}

#[test]
fn direct_skill_and_catalog_occurrences_share_one_rightmost_wins_overlay() {
    let fixture = TestDir::new("mixed-overlay");
    let catalog = fixture.0.join("catalog");
    fs::create_dir(&catalog).expect("catalog fixture");
    let catalog_alpha = valid_skill(&catalog, "alpha");
    valid_skill(&catalog, "beta");
    let direct_parent = fixture.0.join("direct");
    fs::create_dir(&direct_parent).expect("direct parent");
    let direct_alpha = valid_skill(&direct_parent, "alpha");

    let direct_wins = resolve(
        &[catalog.clone(), direct_alpha.clone()],
        AgentId::Codex,
        ValidationLevel::Basic,
    )
    .expect("a direct Skill may override one catalog entry");
    assert_eq!(selected_names(&direct_wins), ["alpha", "beta"]);
    assert_eq!(
        direct_wins.resolutions[0].selected.origin.source_canonical,
        fs::canonicalize(&direct_alpha).expect("canonical direct Skill")
    );
    assert_eq!(direct_wins.resolutions[0].shadowed.len(), 1);
    assert_eq!(direct_wins.override_count(), 1);

    let catalog_wins = resolve(
        &[direct_alpha, catalog],
        AgentId::Codex,
        ValidationLevel::Basic,
    )
    .expect("a later catalog entry may override a direct Skill");
    assert_eq!(
        catalog_wins.resolutions[0].selected.origin.source_canonical,
        fs::canonicalize(catalog_alpha).expect("canonical catalog Skill")
    );
    assert_eq!(catalog_wins.override_count(), 1);
}

#[test]
fn missing_input_wins_over_an_earlier_empty_catalog() {
    let fixture = TestDir::new("missing-priority");
    let empty = fixture.0.join("empty");
    fs::create_dir(&empty).expect("empty catalog");
    let missing = fixture.0.join("missing");

    let error = resolve(
        &[empty, missing.clone()],
        AgentId::Codex,
        ValidationLevel::Basic,
    )
    .expect_err("missing input should fail first");

    assert_eq!(error.category(), ExitCategory::MissingInput);
    assert!(matches!(error, AppError::MissingInput { path, .. } if path == missing));
}

#[test]
fn invalid_source_preceding_a_valid_source_still_fails_as_missing_input() {
    let fixture = TestDir::new("invalid-before-valid");
    let missing = fixture.0.join("missing");
    let valid = valid_skill(&fixture.0, "valid");

    let error = resolve(
        &[missing.clone(), valid],
        AgentId::Codex,
        ValidationLevel::Basic,
    )
    .expect_err("invalid first source should not be hidden");

    assert_eq!(error.category(), ExitCategory::MissingInput);
    assert!(matches!(error, AppError::MissingInput { path, .. } if path == missing));
}

#[test]
fn accessible_empty_catalog_is_data_error() {
    let fixture = TestDir::new("empty");
    let empty = fixture.0.join("empty");
    fs::create_dir(&empty).expect("empty catalog");

    let error = resolve(&[empty], AgentId::Codex, ValidationLevel::Basic)
        .expect_err("empty catalog should fail");
    assert_eq!(error.category(), ExitCategory::Data);
    assert!(matches!(
        error,
        AppError::Catalog(CatalogError::EmptyCatalog { .. })
    ));
}

#[test]
fn non_directory_input_is_missing_input_error() {
    let fixture = TestDir::new("non-directory-input");
    let input = fixture.0.join("not-a-directory");
    fs::write(&input, "file").expect("file fixture");

    let error = resolve(&[input], AgentId::Codex, ValidationLevel::Basic)
        .expect_err("file input should fail");
    assert_eq!(error.category(), ExitCategory::MissingInput);
}

#[cfg(unix)]
#[test]
fn unreadable_input_is_missing_input_error_when_permissions_are_enforced() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = TestDir::new("unreadable-input");
    let input = fixture.0.join("unreadable");
    fs::create_dir(&input).expect("directory fixture");
    fs::set_permissions(&input, fs::Permissions::from_mode(0o000)).expect("restrict fixture");
    if fs::read_dir(&input).is_ok() {
        fs::set_permissions(&input, fs::Permissions::from_mode(0o700)).expect("restore fixture");
        return;
    }
    let error = resolve(
        std::slice::from_ref(&input),
        AgentId::Codex,
        ValidationLevel::Basic,
    )
    .expect_err("unreadable input should fail");
    fs::set_permissions(&input, fs::Permissions::from_mode(0o700)).expect("restore fixture");

    assert_eq!(error.category(), ExitCategory::MissingInput);
}

#[test]
fn rightmost_overlay_retains_every_origin_and_stable_order() {
    let fixture = TestDir::new("overlay");
    let first = fixture.0.join("first");
    let second = fixture.0.join("second");
    let third = fixture.0.join("third");
    fs::create_dir_all(&first).expect("first catalog");
    fs::create_dir_all(&second).expect("second catalog");
    fs::create_dir_all(&third).expect("third catalog");
    valid_skill(&first, "alpha");
    valid_skill(&first, "beta");
    valid_skill(&second, "alpha");
    valid_skill(&second, "gamma");
    valid_skill(&third, "alpha");

    let catalog = resolve(
        &[first, second, third],
        AgentId::Codex,
        ValidationLevel::Basic,
    )
    .expect("overlay should resolve");

    assert_eq!(selected_names(&catalog), ["alpha", "beta", "gamma"]);
    let alpha = &catalog.resolutions[0];
    assert_eq!(alpha.selected.origin.source_ordinal, 2);
    assert_eq!(alpha.shadowed.len(), 2);
    assert_eq!(alpha.shadowed[0].origin.source_ordinal, 0);
    assert_eq!(alpha.shadowed[1].origin.source_ordinal, 1);
    assert!(
        alpha
            .shadowed
            .iter()
            .all(|item| item.reason == ShadowReason::DifferentSourceOverride)
    );
    assert_eq!(catalog.override_count(), 1);
}

#[test]
fn repeated_canonical_source_collapses_without_logical_override() {
    let fixture = TestDir::new("repeated");
    let direct = valid_skill(&fixture.0, "demo");

    let catalog = resolve(
        &[direct.clone(), direct],
        AgentId::Codex,
        ValidationLevel::Basic,
    )
    .expect("repeat should resolve");

    assert_eq!(catalog.resolutions.len(), 1);
    assert_eq!(catalog.resolutions[0].selected.origin.source_ordinal, 1);
    assert_eq!(catalog.resolutions[0].shadowed.len(), 1);
    assert_eq!(
        catalog.resolutions[0].shadowed[0].reason,
        ShadowReason::RepeatedCanonicalSource
    );
    assert_eq!(catalog.override_count(), 0);
}

#[cfg(unix)]
#[test]
fn same_source_ascii_case_variants_are_rejected_before_winner_validation() {
    let fixture = TestDir::new("case-duplicate");
    let catalog = fixture.0.join("catalog");
    fs::create_dir(&catalog).expect("catalog fixture");
    valid_skill(&catalog, "alpha");
    if fs::create_dir(catalog.join("ALPHA")).is_err() {
        return;
    }
    fs::write(catalog.join("ALPHA/SKILL.md"), "invalid").expect("case fixture");

    let error = resolve(&[catalog], AgentId::Codex, ValidationLevel::Basic)
        .expect_err("duplicate logical keys should fail");
    assert!(matches!(
        error,
        AppError::Catalog(CatalogError::DuplicateLogicalName { .. })
    ));
}

#[test]
fn invalid_shadowed_candidate_is_ignored_but_invalid_winner_has_no_fallback() {
    let fixture = TestDir::new("winner-validation");
    let invalid = fixture.0.join("invalid");
    let valid = fixture.0.join("valid");
    fs::create_dir(&invalid).expect("invalid catalog");
    fs::create_dir(&valid).expect("valid catalog");
    skill_with(&invalid, "demo", "not frontmatter\n");
    valid_skill(&valid, "demo");

    let successful = resolve(
        &[invalid.clone(), valid.clone()],
        AgentId::Codex,
        ValidationLevel::Basic,
    )
    .expect("invalid shadow should not fail");
    assert_eq!(successful.resolutions[0].selected.origin.source_ordinal, 1);

    let error = resolve(&[valid, invalid], AgentId::Codex, ValidationLevel::Basic)
        .expect_err("invalid winner should fail without fallback");
    assert_eq!(error.category(), ExitCategory::Data);
}

#[test]
fn a_child_without_skill_md_is_ignored_but_a_present_invalid_candidate_fails() {
    let fixture = TestDir::new("candidate-boundary");
    let catalog = fixture.0.join("catalog");
    fs::create_dir(&catalog).expect("catalog fixture");
    fs::create_dir(catalog.join("missing-metadata")).expect("non-Skill child");
    valid_skill(&catalog, "valid");

    let successful = resolve(
        std::slice::from_ref(&catalog),
        AgentId::Codex,
        ValidationLevel::Basic,
    )
    .expect("a directory without SKILL.md is not a candidate");
    assert_eq!(selected_names(&successful), ["valid"]);

    skill_with(&catalog, "invalid", "not frontmatter\n");
    let error = resolve(&[catalog], AgentId::Codex, ValidationLevel::Basic)
        .expect_err("a structurally present selected candidate must be validated");
    assert!(matches!(
        error,
        AppError::Catalog(CatalogError::InvalidSelectedSkill { .. })
    ));
}

#[test]
fn validation_levels_follow_adapter_metadata_rules() {
    let fixture = TestDir::new("metadata-levels");
    let claude_only = skill_with(
        &fixture.0,
        "demo",
        "---\ndescription: Claude-compatible description\n---\nbody\n",
    );

    resolve(
        std::slice::from_ref(&claude_only),
        AgentId::Claude,
        ValidationLevel::Basic,
    )
    .expect("Claude basic should accept missing name");
    assert!(
        resolve(
            std::slice::from_ref(&claude_only),
            AgentId::Codex,
            ValidationLevel::Basic,
        )
        .is_err()
    );
    assert!(
        resolve(
            std::slice::from_ref(&claude_only),
            AgentId::Claude,
            ValidationLevel::Strict,
        )
        .is_err()
    );

    assert!(
        resolve(
            std::slice::from_ref(&claude_only),
            AgentId::Codex,
            ValidationLevel::None,
        )
        .is_err(),
        "Codex's injected name-enable rules require an adapter-proved metadata name even when optional validation is disabled"
    );
    let valid_codex = valid_skill(&fixture.0, "codex");
    let none = resolve(&[valid_codex], AgentId::Codex, ValidationLevel::None)
        .expect("Codex keeps its adapter-required metadata boundary");
    assert!(none.warnings.is_empty());
    assert_eq!(
        none.resolutions[0].selected.metadata.name.as_deref(),
        Some("codex")
    );
}

#[test]
fn metadata_none_never_disables_safe_name_or_regular_skill_checks() {
    let fixture = TestDir::new("always-on");
    let unsafe_name = skill_with(&fixture.0, "Uppercase", "invalid metadata");
    let error = resolve(&[unsafe_name], AgentId::Claude, ValidationLevel::None)
        .expect_err("unsafe name must fail");
    assert!(matches!(
        error,
        AppError::Catalog(CatalogError::InvalidSkillName { .. })
    ));

    let catalog = fixture.0.join("catalog");
    let non_regular = catalog.join("demo/SKILL.md");
    fs::create_dir_all(&non_regular).expect("non-regular SKILL entry");
    let error = resolve(&[catalog], AgentId::Claude, ValidationLevel::None)
        .expect_err("directory SKILL.md must fail");
    assert!(matches!(
        error,
        AppError::Catalog(CatalogError::InvalidSelectedSkill { .. })
    ));
}

#[test]
fn codex_source_discovery_requires_the_exact_skill_filename() {
    let fixture = TestDir::new("codex-exact-skill-filename");
    let skill = fixture.0.join("demo");
    fs::create_dir(&skill).expect("Skill directory");
    fs::write(
        skill.join("skill.md"),
        "---\nname: demo\ndescription: wrong-case filename\n---\n",
    )
    .expect("wrong-case Skill metadata");

    let error = resolve(&[skill], AgentId::Codex, ValidationLevel::Basic)
        .expect_err("Codex compares the discovered directory-entry basename exactly");

    assert!(matches!(
        error,
        AppError::Catalog(CatalogError::EmptyCatalog { .. })
    ));
}

#[cfg(unix)]
#[test]
fn metadata_none_rejects_unreadable_skill_files_when_permissions_are_enforced() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = TestDir::new("unreadable-skill-none");
    let skill = valid_skill(&fixture.0, "demo");
    let skill_md = skill.join("SKILL.md");
    fs::set_permissions(&skill_md, fs::Permissions::from_mode(0o000))
        .expect("restrict SKILL.md fixture");
    if fs::File::open(&skill_md).is_ok() {
        fs::set_permissions(&skill_md, fs::Permissions::from_mode(0o600))
            .expect("restore SKILL.md fixture");
        return;
    }

    let error = resolve(&[skill], AgentId::Claude, ValidationLevel::None)
        .expect_err("unreadable SKILL.md must fail even with metadata disabled");
    fs::set_permissions(&skill_md, fs::Permissions::from_mode(0o600))
        .expect("restore SKILL.md fixture");

    assert_eq!(error.category(), ExitCategory::Data);
    assert!(error.to_string().contains("not readable"));
}

#[cfg(windows)]
#[test]
fn metadata_none_rejects_skill_files_locked_against_reading() {
    use std::fs::OpenOptions;
    use std::os::windows::fs::OpenOptionsExt;

    let fixture = TestDir::new("locked-skill-none");
    let skill = valid_skill(&fixture.0, "demo");
    let skill_md = skill.join("SKILL.md");
    let exclusive = OpenOptions::new()
        .read(true)
        .share_mode(0)
        .open(&skill_md)
        .expect("lock SKILL.md fixture");

    let error = resolve(&[skill], AgentId::Claude, ValidationLevel::None)
        .expect_err("unreadable SKILL.md must fail even with metadata disabled");
    drop(exclusive);

    assert_eq!(error.category(), ExitCategory::Data);
    assert!(error.to_string().contains("not readable"));
}

#[test]
fn metadata_none_rejects_a_traversal_mount_name() {
    let fixture = TestDir::new("traversal-name");
    fs::write(
        fixture.0.join("SKILL.md"),
        "---\nname: ignored\ndescription: ignored\n---\n",
    )
    .expect("SKILL.md fixture");
    let child = fixture.0.join("child");
    fs::create_dir(&child).expect("child fixture");
    let traversal = child.join("..");

    let error = resolve(&[traversal], AgentId::Claude, ValidationLevel::None)
        .expect_err("traversal mount name must fail");
    assert_eq!(error.category(), ExitCategory::Data);
    assert!(matches!(
        error,
        AppError::Catalog(CatalogError::InvalidSkillName { .. })
    ));
}

#[test]
fn child_directory_links_are_structural_candidates_when_links_are_available() {
    let fixture = TestDir::new("child-directory-link");
    let target = valid_skill(&fixture.0, "target");
    let catalog = fixture.0.join("catalog");
    fs::create_dir(&catalog).expect("catalog fixture");
    if !symlink_directory(&target, &catalog.join("linked")) {
        return;
    }

    let resolved = resolve(&[catalog], AgentId::Claude, ValidationLevel::None)
        .expect("directory link should be discovered");
    assert_eq!(selected_names(&resolved), ["linked"]);
}

#[test]
fn child_file_links_are_not_mistaken_for_directory_links_when_links_are_available() {
    let fixture = TestDir::new("child-file-link");
    let catalog = fixture.0.join("catalog");
    fs::create_dir(&catalog).expect("catalog fixture");
    valid_skill(&catalog, "demo");
    let file = fixture.0.join("notes.txt");
    fs::write(&file, "notes").expect("file fixture");
    if !symlink_file(&file, &catalog.join("not-a-directory")) {
        return;
    }

    let resolved = resolve(&[catalog], AgentId::Codex, ValidationLevel::Basic)
        .expect("file link should be ignored");
    assert_eq!(selected_names(&resolved), ["demo"]);
}

#[test]
fn whitespace_only_required_metadata_is_empty() {
    let fixture = TestDir::new("blank-metadata");
    let blank = skill_with(
        &fixture.0,
        "demo",
        "---\nname: demo\ndescription: '   '\n---\nbody\n",
    );
    let error = resolve(&[blank], AgentId::Codex, ValidationLevel::Basic)
        .expect_err("blank description should fail");
    assert!(error.to_string().contains("description"));
}

#[test]
fn codex_rejects_a_file_link_named_skill_md_that_claude_can_read() {
    let fixture = TestDir::new("codex-file-linked-skill-md");
    let catalog = fixture.0.join("catalog");
    let skill = catalog.join("demo");
    fs::create_dir_all(&skill).expect("Skill fixture");
    let metadata = skill.join("metadata.md");
    fs::write(
        &metadata,
        "---\nname: demo\ndescription: linked metadata fixture\n---\n",
    )
    .expect("metadata fixture");
    if !guarded_symlink_file(&metadata, &skill.join("SKILL.md")) {
        return;
    }

    resolve(
        std::slice::from_ref(&catalog),
        AgentId::Claude,
        ValidationLevel::Basic,
    )
    .expect("Claude catalog validation may follow a contained metadata link");
    let error = resolve(&[catalog], AgentId::Codex, ValidationLevel::Basic)
        .expect_err("Codex's discovery walk skips file-linked SKILL.md entries");

    assert!(error.to_string().contains("only regular SKILL.md entries"));
}

#[test]
fn broken_and_cyclic_skill_entries_are_retained_then_rejected_when_links_are_available() {
    let fixture = TestDir::new("broken-skill-link");
    let broken_catalog = fixture.0.join("broken-catalog");
    let broken_skill = broken_catalog.join("demo");
    fs::create_dir_all(&broken_skill).expect("broken Skill fixture");
    if !symlink_file(Path::new("missing.md"), &broken_skill.join("SKILL.md")) {
        return;
    }
    let broken_error = resolve(
        std::slice::from_ref(&broken_catalog),
        AgentId::Claude,
        ValidationLevel::None,
    )
    .expect_err("broken SKILL link should be selected then rejected");
    assert!(matches!(
        broken_error,
        AppError::Catalog(CatalogError::InvalidSelectedSkill { .. })
    ));

    let cycle_catalog = fixture.0.join("cycle-catalog");
    let cycle_skill = cycle_catalog.join("demo");
    fs::create_dir_all(&cycle_skill).expect("cycle Skill fixture");
    if !symlink_file(Path::new("other.md"), &cycle_skill.join("SKILL.md"))
        || !symlink_file(Path::new("SKILL.md"), &cycle_skill.join("other.md"))
    {
        return;
    }
    let cycle_error = resolve(&[cycle_catalog], AgentId::Claude, ValidationLevel::None)
        .expect_err("SKILL link cycle should fail");
    assert!(cycle_error.to_string().contains("cycle"));
}

#[test]
fn skill_entry_must_remain_contained_when_file_links_are_available() {
    let fixture = TestDir::new("skill-containment");
    let catalog = fixture.0.join("catalog");
    let skill = catalog.join("demo");
    fs::create_dir_all(&skill).expect("Skill fixture");
    let outside = fixture.0.join("outside.md");
    fs::write(&outside, "outside").expect("outside fixture");
    if !symlink_file(&outside, &skill.join("SKILL.md")) {
        return;
    }

    let error = resolve(&[catalog], AgentId::Claude, ValidationLevel::None)
        .expect_err("outside SKILL target should fail");
    assert!(error.to_string().contains("outside"));
}

#[test]
fn source_destination_overlap_is_rejected() {
    let fixture = TestDir::new("destination-cycle");
    let catalog = fixture.0.join("catalog");
    fs::create_dir(&catalog).expect("catalog fixture");
    valid_skill(&catalog, "demo");
    let destination = catalog.join(".agents/skills");
    let error = resolve_catalog(
        &occurrences(std::slice::from_ref(&catalog)),
        &CatalogRequest {
            agent: AgentId::Codex,
            validation: ValidationLevel::Basic,
            destination_stores: &[destination],
        },
    )
    .expect_err("source/destination overlap should fail");
    assert!(matches!(
        error,
        AppError::Catalog(CatalogError::SourceDestinationCycle { .. })
    ));
}

#[test]
fn success_and_late_failure_leave_the_filesystem_unchanged() {
    let fixture = TestDir::new("side-effects");
    let valid = fixture.0.join("valid");
    let invalid = fixture.0.join("invalid");
    fs::create_dir(&valid).expect("valid catalog");
    fs::create_dir(&invalid).expect("invalid catalog");
    valid_skill(&valid, "demo");
    skill_with(&invalid, "demo", "invalid metadata\n");
    let before = snapshot(&fixture.0);

    resolve(&[valid], AgentId::Codex, ValidationLevel::Basic)
        .expect("successful read-only resolution");
    assert_eq!(snapshot(&fixture.0), before);

    resolve(&[invalid], AgentId::Codex, ValidationLevel::Basic)
        .expect_err("late metadata validation should fail");
    assert_eq!(snapshot(&fixture.0), before);
}

#[test]
fn different_names_for_one_canonical_directory_are_rejected_when_links_are_available() {
    let fixture = TestDir::new("aliases");
    let target = valid_skill(&fixture.0, "target");
    let catalog = fixture.0.join("catalog");
    fs::create_dir(&catalog).expect("catalog fixture");
    if !symlink_directory(&target, &catalog.join("alpha"))
        || !symlink_directory(&target, &catalog.join("beta"))
    {
        return;
    }

    let error = resolve(&[catalog], AgentId::Codex, ValidationLevel::Basic)
        .expect_err("canonical aliases should fail before metadata");
    assert!(matches!(
        error,
        AppError::Catalog(CatalogError::CanonicalAlias { .. })
    ));
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, (bool, u64)> {
    fn visit(root: &Path, current: &Path, result: &mut BTreeMap<PathBuf, (bool, u64)>) {
        let mut entries = fs::read_dir(current)
            .expect("snapshot directory")
            .collect::<Result<Vec<_>, _>>()
            .expect("snapshot entries");
        entries.sort_by_key(fs::DirEntry::file_name);
        for entry in entries {
            let metadata = fs::symlink_metadata(entry.path()).expect("snapshot metadata");
            let relative = entry
                .path()
                .strip_prefix(root)
                .expect("fixture relative path")
                .to_path_buf();
            result.insert(relative, (metadata.is_dir(), metadata.len()));
            if metadata.is_dir() {
                visit(root, &entry.path(), result);
            }
        }
    }

    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

#[cfg(unix)]
fn symlink_directory(source: &Path, destination: &Path) -> bool {
    std::os::unix::fs::symlink(source, destination).is_ok()
}

#[cfg(unix)]
fn symlink_file(source: &Path, destination: &Path) -> bool {
    std::os::unix::fs::symlink(source, destination).is_ok()
}

#[cfg(windows)]
fn symlink_directory(source: &Path, destination: &Path) -> bool {
    std::os::windows::fs::symlink_dir(source, destination).is_ok()
}

#[cfg(windows)]
fn symlink_file(source: &Path, destination: &Path) -> bool {
    std::os::windows::fs::symlink_file(source, destination).is_ok()
}

#[test]
fn exit_categories_remain_stable() {
    assert_eq!(ExitCategory::Usage.code(), 64);
    assert_eq!(ExitCategory::Data.code(), 65);
    assert_eq!(ExitCategory::MissingInput.code(), 66);
    assert_eq!(ExitCategory::Internal.code(), 70);
    assert_eq!(ExitCategory::Filesystem.code(), 73);
    assert_eq!(ExitCategory::Temporary.code(), 75);
    assert_eq!(ExitCategory::Interrupted.code(), 130);
}
