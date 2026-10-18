use super::*;
use chrono::DateTime;

fn path(s: &str) -> Scope {
    Scope::Path(ScopePath::new(s).unwrap())
}

// --- MemoryName ---

#[test]
fn memory_name_valid() {
    let name = MemoryName::new("my-memory").unwrap();
    assert_eq!(name.as_str(), "my-memory");
    assert_eq!(name.into_inner(), "my-memory");
}

#[test]
fn memory_name_empty_rejected() {
    assert!(MemoryName::new("").is_err());
}

#[test]
fn memory_name_traversal_rejected() {
    assert!(MemoryName::new("..").is_err());
    assert!(MemoryName::new("../etc/passwd").is_err());
    assert!(MemoryName::new(".hidden").is_err());
}

#[test]
fn memory_name_invalid_chars_rejected() {
    assert!(MemoryName::new("has spaces").is_err());
    assert!(MemoryName::new("has@symbol").is_err());
}

#[test]
fn memory_name_nested_valid() {
    let name = MemoryName::new("sub/path").unwrap();
    assert_eq!(name.as_str(), "sub/path");
}

#[test]
fn memory_name_display() {
    let name = MemoryName::new("test-display").unwrap();
    assert_eq!(format!("{name}"), "test-display");
}

#[test]
fn memory_name_serde_round_trip() {
    let valid: MemoryName = serde_json::from_str("\"my-memory\"").unwrap();
    assert_eq!(valid.as_str(), "my-memory");

    let invalid: Result<MemoryName, _> = serde_json::from_str("\"../etc\"");
    assert!(invalid.is_err());

    let empty: Result<MemoryName, _> = serde_json::from_str("\"\"");
    assert!(empty.is_err());
}

// --- validate_name ---

#[test]
fn validate_name_accepts_valid() {
    assert!(MemoryName::validate("my-memory").is_ok());
    assert!(MemoryName::validate("some_memory").is_ok());
    assert!(MemoryName::validate("nested/path").is_ok());
    assert!(MemoryName::validate("v1.2.3").is_ok());
}

#[test]
fn validate_name_rejects_traversal() {
    assert!(MemoryName::validate("../../etc/passwd").is_err());
    assert!(MemoryName::validate("..").is_err());
    assert!(MemoryName::validate(".hidden").is_err());
    assert!(MemoryName::validate("a/../b").is_err());
}

#[test]
fn validate_name_rejects_empty() {
    assert!(MemoryName::validate("").is_err());
}

#[test]
fn validate_name_rejects_special_chars() {
    assert!(MemoryName::validate("foo;bar").is_err());
    assert!(MemoryName::validate("foo bar").is_err());
    assert!(MemoryName::validate("foo\0bar").is_err());
}

#[test]
fn validate_name_rejects_empty_component() {
    assert!(MemoryName::validate("foo//bar").is_err());
    assert!(MemoryName::validate("/absolute").is_err());
}

// --- Memory round-trip ---

fn make_memory() -> Memory {
    let meta = MemoryMetadata {
        tags: vec!["test".to_string(), "round-trip".to_string()],
        scope: path("my-project"),
        created_at: DateTime::from_timestamp(1_700_000_000, 0).unwrap(),
        updated_at: DateTime::from_timestamp(1_700_000_100, 0).unwrap(),
        source: Some("unit-test".to_string()),
    };
    Memory {
        id: "550e8400-e29b-41d4-a716-446655440000".to_string(),
        name: MemoryName::new("test-memory").unwrap(),
        content: "# Hello\n\nThis is a test memory.".to_string(),
        metadata: meta,
    }
}

#[test]
fn round_trip_markdown() {
    let original = make_memory();
    let rendered = original.to_markdown().expect("to_markdown should not fail");
    let parsed = Memory::from_markdown(&rendered).expect("from_markdown should not fail");

    assert_eq!(original.id, parsed.id);
    assert_eq!(original.name, parsed.name);
    assert_eq!(original.content, parsed.content);
    assert_eq!(original.metadata.tags, parsed.metadata.tags);
    assert_eq!(original.metadata.scope, parsed.metadata.scope);
    assert_eq!(
        original.metadata.created_at.timestamp(),
        parsed.metadata.created_at.timestamp()
    );
    assert_eq!(
        original.metadata.updated_at.timestamp(),
        parsed.metadata.updated_at.timestamp()
    );
    assert_eq!(original.metadata.source, parsed.metadata.source);
}

#[test]
fn round_trip_global_scope() {
    let meta = MemoryMetadata::new(Scope::Root, vec!["global-tag".to_string()], None);
    let mem = Memory::new("global-mem", "Some content.", meta).unwrap();
    let rendered = mem.to_markdown().unwrap();
    let parsed = Memory::from_markdown(&rendered).unwrap();

    assert_eq!(parsed.metadata.scope, Scope::Root);
    assert_eq!(parsed.metadata.source, None);
    assert_eq!(parsed.content, "Some content.");
}

#[test]
fn round_trip_no_source() {
    let meta = MemoryMetadata::new(path("proj"), vec![], None);
    let mem = Memory::new("no-src", "Body.", meta).unwrap();
    let md = mem.to_markdown().unwrap();
    assert!(!md.contains("source:"));
    let parsed = Memory::from_markdown(&md).unwrap();
    assert_eq!(parsed.metadata.source, None);
}

#[test]
fn legacy_frontmatter_preserves_metadata_and_body_after_rewrite() {
    let raw = r#"---
id: 550e8400-e29b-41d4-a716-446655440000
name: legacy-memory
tags:
  - 'on'
  - 'null'
  - '123'
  - 'tag: value # literal'
scope:
  type: Project
  name: org/team
created_at: 2023-11-14T22:13:20Z
updated_at: 2023-11-14T22:15:00Z
source: 'import: "old" # literal'
---

# Héllo

Body: on/null # not YAML
"#;
    let memory = Memory::from_markdown(raw).unwrap();
    assert_eq!(memory.id, "550e8400-e29b-41d4-a716-446655440000");
    assert_eq!(memory.name.as_str(), "legacy-memory");
    assert_eq!(memory.metadata.scope, path("org/team"));
    assert_eq!(
        memory.metadata.tags,
        ["on", "null", "123", "tag: value # literal"]
    );
    assert_eq!(memory.metadata.created_at.timestamp(), 1_700_000_000);
    assert_eq!(memory.metadata.updated_at.timestamp(), 1_700_000_100);
    assert_eq!(
        memory.metadata.source.as_deref(),
        Some("import: \"old\" # literal")
    );
    assert_eq!(memory.content, "# Héllo\n\nBody: on/null # not YAML\n");

    let rewritten = Memory::from_markdown(&memory.to_markdown().unwrap()).unwrap();
    assert_eq!(rewritten.id, memory.id);
    assert_eq!(rewritten.name, memory.name);
    assert_eq!(rewritten.metadata.tags, memory.metadata.tags);
    assert_eq!(rewritten.metadata.scope, memory.metadata.scope);
    assert_eq!(rewritten.metadata.created_at, memory.metadata.created_at);
    assert_eq!(rewritten.metadata.updated_at, memory.metadata.updated_at);
    assert_eq!(rewritten.metadata.source, memory.metadata.source);
    assert_eq!(rewritten.content, memory.content);
}

fn markdown_with_scope(scope_yaml: &str) -> String {
    format!(
        "---\nid: legacy-id\nname: legacy-memory\ntags: []\nscope:\n{scope_yaml}\n\
         created_at: 2023-11-14T22:13:20Z\nupdated_at: 2023-11-14T22:15:00Z\n\
         ---\n\nLegacy body.\n"
    )
}

#[test]
fn unquoted_legacy_scope_names_remain_strings() {
    let names = [
        "on", "On", "ON", "off", "Off", "OFF", "yes", "Yes", "YES", "no", "No", "NO", "y", "Y",
        "n", "N", "1_000",
    ];
    for scope_type in ["Path", "Project"] {
        for name in names {
            let raw = markdown_with_scope(&format!("  type: {scope_type}\n  name: {name}"));
            let memory = Memory::from_markdown(&raw)
                .unwrap_or_else(|error| panic!("legacy scope {scope_type}/{name}: {error}"));
            match &memory.metadata.scope {
                Scope::Path(scope) => assert_eq!(scope.as_str(), name),
                Scope::Root => panic!("legacy path scope {scope_type}/{name} became root"),
            }
            let rewritten = Memory::from_markdown(&memory.to_markdown().unwrap()).unwrap();
            assert_eq!(rewritten.metadata.scope, memory.metadata.scope);
            assert_eq!(rewritten.content, "Legacy body.\n");
        }
    }
}

#[test]
fn frontmatter_scope_adapter_retains_path_validation() {
    for scope_type in ["Path", "Project"] {
        for name in ["..", "../escape", "/absolute", "invalid//path"] {
            let raw = markdown_with_scope(&format!("  type: {scope_type}\n  name: {name}"));
            assert!(
                Memory::from_markdown(&raw).is_err(),
                "invalid scope {scope_type}/{name} was accepted"
            );
        }
        let missing_name = markdown_with_scope(&format!("  type: {scope_type}"));
        assert!(Memory::from_markdown(&missing_name).is_err());
    }
}

#[test]
fn from_markdown_missing_frontmatter_fails() {
    let result = Memory::from_markdown("just plain text");
    assert!(result.is_err());
}

// --- parse_qualified_name ---

#[test]
fn test_parse_qualified_name_global() {
    let r = parse_qualified_name("global/my-memory").unwrap();
    assert_eq!(r.scope, Scope::Root);
    assert_eq!(r.name.as_str(), "my-memory");
    assert_eq!(r.qualified_path(), "v1:scope=global;name=my-memory");
}

#[test]
fn test_parse_qualified_name_project() {
    let r = parse_qualified_name("projects/my-project/my-memory").unwrap();
    assert_eq!(r.scope, path("my-project"));
    assert_eq!(r.name.as_str(), "my-memory");
    assert_eq!(r.qualified_path(), "v1:scope=my-project;name=my-memory");
}

#[test]
fn test_parse_qualified_name_nested() {
    let r = parse_qualified_name("projects/my-project/nested/memory").unwrap();
    assert_eq!(r.scope, path("my-project"));
    assert_eq!(r.name.as_str(), "nested/memory");
}

#[test]
fn test_parse_qualified_name_canonical_form() {
    let r = parse_qualified_name("scope=org/team;name=my-memory").unwrap();
    assert_eq!(r.scope, path("org/team"));
    assert_eq!(r.name.as_str(), "my-memory");
    assert_eq!(r.qualified_path(), "v1:scope=org/team;name=my-memory");
}

#[test]
fn test_parse_qualified_name_canonical_global() {
    let r = parse_qualified_name("scope=global;name=foo").unwrap();
    assert_eq!(r.scope, Scope::Root);
    assert_eq!(r.name.as_str(), "foo");
}

#[test]
fn test_parse_qualified_name_versioned_form() {
    let r = parse_qualified_name("v1:scope=org/team;name=my-memory").unwrap();
    assert_eq!(r.scope, path("org/team"));
    assert_eq!(r.name.as_str(), "my-memory");
    assert_eq!(r.qualified_path(), "v1:scope=org/team;name=my-memory");
}

#[test]
fn test_parse_qualified_name_versioned_global() {
    let r = parse_qualified_name("v1:scope=global;name=foo").unwrap();
    assert_eq!(r.scope, Scope::Root);
    assert_eq!(r.name.as_str(), "foo");
    assert_eq!(r.qualified_path(), "v1:scope=global;name=foo");
}

#[test]
fn parse_qualified_name_rejects_empty_scope() {
    assert!(parse_qualified_name("v1:scope=;name=foo").is_err());
}

#[test]
fn parse_qualified_name_rejects_empty_name() {
    assert!(parse_qualified_name("v1:scope=foo;name=").is_err());
}

#[test]
fn parse_qualified_name_rejects_missing_name_separator() {
    assert!(parse_qualified_name("scope=foo").is_err());
}

// --- MemoryRef ---

#[test]
fn memory_ref_qualified_path_global() {
    let r = MemoryRef::new(Scope::Root, MemoryName::new("my-mem").unwrap());
    assert_eq!(r.qualified_path(), "v1:scope=global;name=my-mem");
    assert_eq!(r.file_path(), "global/my-mem");
}

#[test]
fn memory_ref_qualified_path_project() {
    let r = MemoryRef::new(path("proj"), MemoryName::new("my-mem").unwrap());
    assert_eq!(r.qualified_path(), "v1:scope=proj;name=my-mem");
    assert_eq!(r.file_path(), "projects/proj/my-mem");
}

#[test]
fn memory_ref_qualified_path_org_team() {
    let r = MemoryRef::new(path("org/team"), MemoryName::new("my-mem").unwrap());
    assert_eq!(r.qualified_path(), "v1:scope=org/team;name=my-mem");
    assert_eq!(r.file_path(), "projects/org/team/my-mem");
}

#[test]
fn memory_ref_display() {
    let r = MemoryRef::new(path("proj"), MemoryName::new("my-mem").unwrap());
    assert_eq!(format!("{r}"), "proj:my-mem");
}

#[test]
fn memory_ref_display_global() {
    let r = MemoryRef::new(Scope::Root, MemoryName::new("foo").unwrap());
    assert_eq!(format!("{r}"), "global:foo");
}

#[test]
fn memory_mem_ref_convenience() {
    let meta = MemoryMetadata::new(path("my-project"), vec![], None);
    let mem = Memory::new("some-mem", "body", meta).unwrap();
    let r = mem.mem_ref();
    assert_eq!(r.scope, path("my-project"));
    assert_eq!(r.name.as_str(), "some-mem");
    assert_eq!(r.qualified_path(), "v1:scope=my-project;name=some-mem");
}
