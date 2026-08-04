//! Release workflow and version-tag ruleset contract tests.

use std::collections::BTreeMap;

use serde_yaml_ng::{Mapping, Value};

const CHECKOUT_REVISION: &str = "3d3c42e5aac5ba805825da76410c181273ba90b1";
const UPLOAD_REVISION: &str = "043fb46d1a93c77aae656e7c1c64a875d1fc6a0a";
const DOWNLOAD_REVISION: &str = "3e5f45b2cfb9172054b4087a40e8e0b5a5461e7c";

fn mapping(value: &Value) -> &Mapping {
    value.as_mapping().expect("expected a YAML mapping")
}

fn field<'value>(value: &'value Mapping, key: &str) -> &'value Value {
    value
        .get(Value::String(key.to_owned()))
        .unwrap_or_else(|| panic!("missing YAML field {key:?}"))
}

fn string_field<'value>(value: &'value Mapping, key: &str) -> &'value str {
    field(value, key)
        .as_str()
        .unwrap_or_else(|| panic!("YAML field {key:?} is not a string"))
}

fn collect_action_uses(value: &Value, uses: &mut Vec<String>) {
    match value {
        Value::Mapping(entries) => {
            for (key, child) in entries {
                if key.as_str() == Some("uses") {
                    uses.push(
                        child
                            .as_str()
                            .expect("workflow uses value must be a string")
                            .to_owned(),
                    );
                }
                collect_action_uses(child, uses);
            }
        }
        Value::Sequence(entries) => {
            for child in entries {
                collect_action_uses(child, uses);
            }
        }
        _ => {}
    }
}

#[test]
fn release_workflow_is_manual_safe_and_least_privilege() {
    let workflow: Value = serde_yaml_ng::from_str(include_str!("../.github/workflows/release.yml"))
        .expect("release workflow must be valid YAML");
    let workflow = mapping(&workflow);

    let triggers = mapping(field(workflow, "on"));
    let push = mapping(field(triggers, "push"));
    let tags = field(push, "tags")
        .as_sequence()
        .expect("push tags must be a sequence");
    assert_eq!(tags, &[Value::String("v*".to_owned())]);
    assert!(field(triggers, "workflow_dispatch").is_mapping());

    let workflow_permissions = mapping(field(workflow, "permissions"));
    assert_eq!(workflow_permissions.len(), 1);
    assert_eq!(string_field(workflow_permissions, "contents"), "read");

    let environment = mapping(field(workflow, "env"));
    assert_eq!(string_field(environment, "SKILLMOUNT_REQUIRE_LINKS"), "1");

    let jobs = mapping(field(workflow, "jobs"));
    let expected_jobs = ["aggregate", "build", "preflight", "publish"];
    let mut actual_jobs = jobs
        .keys()
        .map(|key| key.as_str().expect("job key must be a string"))
        .collect::<Vec<_>>();
    actual_jobs.sort_unstable();
    assert_eq!(actual_jobs, expected_jobs);

    for (name, value) in jobs {
        let name = name.as_str().expect("job key must be a string");
        let job = mapping(value);
        if name == "publish" {
            let permissions = mapping(field(job, "permissions"));
            assert_eq!(permissions.len(), 1);
            assert_eq!(string_field(permissions, "contents"), "write");
        } else {
            assert!(
                job.get(Value::String("permissions".to_owned())).is_none(),
                "only publish may override workflow permissions"
            );
        }
    }

    let publish = mapping(field(jobs, "publish"));
    let publish_condition = string_field(publish, "if");
    for required_clause in [
        "needs.preflight.result == 'success'",
        "needs.build.result == 'success'",
        "needs.aggregate.result == 'success'",
        "needs.preflight.outputs.publish == 'true'",
        "github.event_name == 'push'",
        "github.ref == format('refs/tags/{0}', needs.preflight.outputs.tag)",
    ] {
        assert!(
            publish_condition.contains(required_clause),
            "publish condition is missing {required_clause:?}"
        );
    }
    let concurrency = mapping(field(publish, "concurrency"));
    assert_eq!(
        string_field(concurrency, "group"),
        "release-${{ needs.preflight.outputs.tag }}"
    );
    assert_eq!(
        field(concurrency, "cancel-in-progress").as_bool(),
        Some(false)
    );
}

#[test]
fn release_workflow_uses_only_reviewed_immutable_action_revisions() {
    let workflow: Value = serde_yaml_ng::from_str(include_str!("../.github/workflows/release.yml"))
        .expect("release workflow must be valid YAML");
    let mut uses = Vec::new();
    collect_action_uses(&workflow, &mut uses);

    let expected = BTreeMap::from([
        ("actions/checkout", CHECKOUT_REVISION),
        ("actions/download-artifact", DOWNLOAD_REVISION),
        ("actions/upload-artifact", UPLOAD_REVISION),
    ]);
    let mut counts = BTreeMap::<&str, usize>::new();
    for action in &uses {
        let (name, revision) = action
            .split_once('@')
            .expect("action use must contain an immutable revision");
        let expected_revision = expected
            .get(name)
            .unwrap_or_else(|| panic!("unreviewed action dependency {name:?}"));
        assert_eq!(
            revision, *expected_revision,
            "unexpected revision for {name}"
        );
        assert_eq!(revision.len(), 40, "action revision must be a full SHA-1");
        assert!(
            revision.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "action revision must be hexadecimal"
        );
        *counts.entry(name).or_default() += 1;
    }
    assert_eq!(
        counts,
        BTreeMap::from([
            ("actions/checkout", 4),
            ("actions/download-artifact", 2),
            ("actions/upload-artifact", 2),
        ])
    );
}

#[test]
fn version_tag_ruleset_is_active_immutable_and_has_no_bypass() {
    let ruleset: serde_json::Value =
        serde_json::from_str(include_str!("../.github/rulesets/version-tags.json"))
            .expect("version-tag ruleset must be valid JSON");

    assert_eq!(ruleset["name"], "Protect version tags");
    assert_eq!(ruleset["target"], "tag");
    assert_eq!(ruleset["enforcement"], "active");
    assert_eq!(ruleset["bypass_actors"], serde_json::json!([]));
    assert_eq!(
        ruleset["conditions"]["ref_name"]["include"],
        serde_json::json!(["refs/tags/v*"])
    );
    assert_eq!(
        ruleset["conditions"]["ref_name"]["exclude"],
        serde_json::json!([])
    );

    let rules = ruleset["rules"]
        .as_array()
        .expect("ruleset rules must be an array");
    let rule_types = rules
        .iter()
        .map(|rule| {
            rule["type"]
                .as_str()
                .expect("ruleset type must be a string")
        })
        .collect::<Vec<_>>();
    assert_eq!(rule_types, ["update", "deletion", "non_fast_forward"]);
    assert_eq!(
        rules[0]["parameters"]["update_allows_fetch_and_merge"],
        false
    );
    assert!(!rule_types.contains(&"creation"));
}
