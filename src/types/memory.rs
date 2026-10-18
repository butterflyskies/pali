use std::{fmt, ops::Deref, str::FromStr};

use chrono::{DateTime, Utc};
use rmcp::schemars;
use serde::{Deserialize, Deserializer, Serialize};
use uuid::Uuid;

use crate::error::MemoryError;

use super::{
    scope::{Scope, ScopePath},
    validated::{deserialize_validated, ValidatedString},
};

// ---------------------------------------------------------------------------
// MemoryName newtype
// ---------------------------------------------------------------------------

/// A validated memory name.
///
/// Wraps a `String` that has been validated by `ValidatedString::validate`. Constructing
/// a `MemoryName` is the sole path to a valid name — once you hold one, you
/// can use it as `&str` without re-validation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
pub struct MemoryName(String);

impl ValidatedString for MemoryName {
    fn validate(s: &str) -> Result<(), MemoryError> {
        if s.is_empty() {
            return Err(MemoryError::InvalidInput {
                reason: "name must not be empty".to_string(),
            });
        }

        if s.split('/').count() > 3 {
            return Err(MemoryError::InvalidInput {
                reason: format!("name '{}' exceeds maximum nesting depth of 3", s),
            });
        }

        for component in s.split('/') {
            if component.is_empty() {
                return Err(MemoryError::InvalidInput {
                    reason: format!("name '{}' contains an empty path component", s),
                });
            }
            if component.starts_with('.') {
                return Err(MemoryError::InvalidInput {
                    reason: format!(
                        "name '{}' contains a dot-prefixed component '{}'",
                        s, component
                    ),
                });
            }
            if !component
                .chars()
                .all(|c| c.is_alphanumeric() || c == '-' || c == '_' || c == '.')
            {
                return Err(MemoryError::InvalidInput {
                    reason: format!(
                        "name '{}' contains disallowed characters in component '{}'",
                        s, component
                    ),
                });
            }
        }

        Ok(())
    }

    fn wrap(s: String) -> Self {
        Self(s)
    }
}

impl MemoryName {
    /// Validate `name` and wrap it.
    pub fn new(name: impl Into<String>) -> Result<Self, MemoryError> {
        <Self as ValidatedString>::new(name)
    }

    /// Consume and return the inner `String`.
    pub fn into_inner(self) -> String {
        self.0
    }

    /// Borrow as `&str`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for MemoryName {
    type Err = MemoryError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

impl fmt::Display for MemoryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Deref for MemoryName {
    type Target = str;

    fn deref(&self) -> &str {
        &self.0
    }
}

impl AsRef<str> for MemoryName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for MemoryName {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserialize_validated(deserializer)
    }
}

impl schemars::JsonSchema for MemoryName {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("MemoryName")
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        String::json_schema(generator)
    }

    fn inline_schema() -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// MemoryMetadata
// ---------------------------------------------------------------------------

/// Metadata attached to every [`Memory`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryMetadata {
    /// Free-form tags for categorisation and filtering.
    pub tags: Vec<String>,
    /// Where this memory lives (global or namespace-scoped).
    pub scope: Scope,
    /// When this memory was first created.
    pub created_at: DateTime<Utc>,
    /// When this memory was last modified.
    pub updated_at: DateTime<Utc>,
    /// Optional hint about where this memory came from (e.g. a tool name).
    pub source: Option<String>,
}

impl MemoryMetadata {
    /// Create new metadata with the current timestamp for both `created_at` and `updated_at`.
    pub fn new(scope: Scope, tags: Vec<String>, source: Option<String>) -> Self {
        let now = Utc::now();
        Self {
            tags,
            scope,
            created_at: now,
            updated_at: now,
            source,
        }
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// A single memory unit, stored on disk as a markdown file with YAML frontmatter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Memory {
    /// Stable UUID for vector-index keying.
    pub id: String,
    /// Human-readable name / filename stem.
    pub name: MemoryName,
    /// Markdown body (no frontmatter).
    pub content: String,
    /// Associated metadata (tags, scope, timestamps, source).
    pub metadata: MemoryMetadata,
}

impl Memory {
    /// Create a new memory with a random UUID, validating the name.
    pub fn new(
        name: impl Into<String>,
        content: impl Into<String>,
        metadata: MemoryMetadata,
    ) -> Result<Self, MemoryError> {
        Ok(Self {
            id: Uuid::new_v4().to_string(),
            name: MemoryName::new(name)?,
            content: content.into(),
            metadata,
        })
    }

    /// Create a [`MemoryRef`] pointing at this memory.
    ///
    /// Convenience over `MemoryRef::new(memory.metadata.scope.clone(), memory.name.clone())`.
    pub fn mem_ref(&self) -> MemoryRef {
        MemoryRef::new(self.metadata.scope.clone(), self.name.clone())
    }

    /// Create a new memory from an already-validated [`MemoryName`].
    pub(crate) fn from_validated(
        name: MemoryName,
        content: impl Into<String>,
        metadata: MemoryMetadata,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            name,
            content: content.into(),
            metadata,
        }
    }

    /// Create a memory preserving an existing ID (for cross-repo moves).
    pub(crate) fn from_validated_with_id(
        id: String,
        name: MemoryName,
        content: impl Into<String>,
        metadata: MemoryMetadata,
    ) -> Self {
        Self {
            id,
            name,
            content: content.into(),
            metadata,
        }
    }

    /// Render to the on-disk format: YAML frontmatter + markdown body.
    ///
    /// Format:
    /// ```text
    /// ---
    /// <yaml>
    /// ---
    ///
    /// <content>
    /// ```
    pub fn to_markdown(&self) -> Result<String, MemoryError> {
        #[derive(Serialize)]
        struct Frontmatter<'a> {
            id: &'a str,
            name: &'a str,
            tags: &'a [String],
            scope: &'a Scope,
            created_at: &'a DateTime<Utc>,
            updated_at: &'a DateTime<Utc>,
            #[serde(skip_serializing_if = "Option::is_none")]
            source: Option<&'a str>,
        }

        let fm = Frontmatter {
            id: &self.id,
            name: self.name.as_str(),
            tags: &self.metadata.tags,
            scope: &self.metadata.scope,
            created_at: &self.metadata.created_at,
            updated_at: &self.metadata.updated_at,
            source: self.metadata.source.as_deref(),
        };

        let yaml = serde_saphyr::to_string(&fm)?;
        Ok(format!("---\n{}---\n\n{}", yaml, self.content))
    }

    /// Parse from on-disk markdown format.
    pub fn from_markdown(raw: &str) -> Result<Self, MemoryError> {
        // Must start with "---\n"
        let rest = raw
            .strip_prefix("---\n")
            .ok_or_else(|| MemoryError::InvalidInput {
                reason: "missing opening frontmatter delimiter".to_string(),
            })?;

        // Find the closing "---"
        let end_marker = rest
            .find("\n---\n")
            .ok_or_else(|| MemoryError::InvalidInput {
                reason: "missing closing frontmatter delimiter".to_string(),
            })?;

        let yaml_str = &rest[..end_marker];
        // +5 = "\n---\n".len(); skip optional leading newline in body
        let body = rest[end_marker + 5..].trim_start_matches('\n');

        // Decode scalar names directly. Internally tagged enums buffer their
        // fields and can reinterpret legacy unquoted names such as `on`.
        #[derive(Deserialize)]
        enum FrontmatterScopeKind {
            Root,
            Path,
            Global,
            Project,
        }

        #[derive(Deserialize)]
        struct FrontmatterScope {
            #[serde(rename = "type")]
            kind: FrontmatterScopeKind,
            name: Option<String>,
        }

        #[derive(Deserialize)]
        struct Frontmatter {
            id: String,
            name: String,
            tags: Vec<String>,
            scope: FrontmatterScope,
            created_at: DateTime<Utc>,
            updated_at: DateTime<Utc>,
            source: Option<String>,
        }

        let fm: Frontmatter = serde_saphyr::from_str(yaml_str)?;
        let scope =
            match fm.scope.kind {
                FrontmatterScopeKind::Root | FrontmatterScopeKind::Global => Scope::Root,
                FrontmatterScopeKind::Path | FrontmatterScopeKind::Project => {
                    let name = fm.scope.name.ok_or_else(|| {
                        <serde_saphyr::Error as serde::de::Error>::missing_field("name")
                    })?;
                    Scope::Path(ScopePath::new(name).map_err(|error| {
                        <serde_saphyr::Error as serde::de::Error>::custom(error)
                    })?)
                }
            };

        Ok(Memory {
            id: fm.id,
            name: MemoryName::new(fm.name)?,
            content: body.to_string(),
            metadata: MemoryMetadata {
                tags: fm.tags,
                scope,
                created_at: fm.created_at,
                updated_at: fm.updated_at,
                source: fm.source,
            },
        })
    }
}

// ---------------------------------------------------------------------------
// MemoryRef — a scope+name pair
// ---------------------------------------------------------------------------

/// A reference to a specific memory: scope + validated name.
///
/// Serde support exists for wire shapes that embed a parent reference
/// (e.g. the crate-internal `FactRecord` catalog entry); deserialization
/// stays strict via the components' own validating `Deserialize` impls.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MemoryRef {
    /// Where this memory lives (global or namespace-scoped).
    pub scope: Scope,
    /// Validated memory name.
    pub name: MemoryName,
}

impl MemoryRef {
    /// Create a reference from a scope and a validated name.
    pub fn new(scope: Scope, name: MemoryName) -> Self {
        Self { scope, name }
    }

    /// The on-disk file path: `"global/<name>"` or `"projects/<path>/<name>"`.
    ///
    /// Used for file I/O (repo reads/writes). For index/telemetry keys that
    /// need a stable string, use [`Self::qualified_path`] instead.
    pub fn file_path(&self) -> String {
        format!("{}/{}", self.scope.dir_prefix(), self.name)
    }

    /// Canonical key encoding for index/telemetry: `"v1:scope=<scope>;name=<name>"`.
    ///
    /// The `v1:` prefix allows future format changes to be detected and
    /// migrated. Unversioned `scope=...;name=...` keys written by older
    /// versions are still accepted by [`parse_qualified_name`] as v1.
    pub fn qualified_path(&self) -> String {
        format!("v1:scope={};name={}", self.scope, self.name)
    }
}

impl fmt::Display for MemoryRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.scope, self.name)
    }
}

// ---------------------------------------------------------------------------
// Helper functions
// ---------------------------------------------------------------------------

/// Parse a qualified name back into a [`MemoryRef`].
///
/// Accepts three forms:
/// - Versioned canonical key: `"v1:scope=<scope>;name=<name>"` (current, preferred)
/// - Unversioned canonical key: `"scope=<scope>;name=<name>"` (treated as v1 for compat)
/// - On-disk path: `"global/<name>"` or `"projects/<path>/<name>"` (legacy on-disk layout)
///
/// # On-disk path form and multi-segment scope ambiguity
///
/// The on-disk path form (`"projects/<path>/<name>"`) is **ambiguous for
/// multi-segment (hierarchical) scopes**. For example, `"projects/org/team/mem"`
/// could mean scope=`"org/team"`, name=`"mem"` — or scope=`"org"`,
/// name=`"team/mem"`. This function always splits at the **first slash**, so
/// the above example yields scope=`"org"`, name=`"team/mem"`.
///
/// This is correct for all single-segment scope paths (the only form that
/// existed before hierarchical scopes were introduced). For hierarchical scopes,
/// callers should use the canonical `"v1:scope=...;name=..."` form, which is
/// unambiguous and is what [`MemoryRef::qualified_path`] produces. All new index
/// entries use the canonical form.
///
/// Incremental reindexing never parses on-disk paths: changed memories are
/// resolved from their frontmatter by `MemoryRepo::diff_changed_refs`
/// (returning `ResolvedChanges`, the same authority `list_memories` uses), so
/// this ambiguity is confined to legacy index entries from
/// pre-hierarchical-scope builds — a full reindex resolves any such stale
/// entries. The public `MemoryRepo::diff_changed_memories` is a distinct
/// surface: it returns raw repo paths to preserve the 0.16.0 value contract
/// and is not the frontmatter-safe incremental-reindex path.
pub fn parse_qualified_name(qualified: &str) -> Result<MemoryRef, MemoryError> {
    // Versioned canonical key form: "v1:scope=...;name=..."
    // Unversioned form "scope=...;name=..." is also accepted as v1 for backward compat.
    let key_body = qualified
        .strip_prefix("v1:")
        .and_then(|rest| rest.strip_prefix("scope=").map(|_| &rest["scope=".len()..]))
        .or_else(|| qualified.strip_prefix("scope="));

    if let Some(rest) = key_body {
        if let Some(semi) = rest.find(";name=") {
            let scope_str = &rest[..semi];
            let name_str = &rest[semi + 6..];
            if scope_str.is_empty() || name_str.is_empty() {
                return Err(MemoryError::InvalidInput {
                    reason: format!(
                        "malformed qualified name '{}': scope or name is empty",
                        qualified
                    ),
                });
            }
            let scope = scope_str.parse::<Scope>()?;
            let name = MemoryName::new(name_str)?;
            return Ok(MemoryRef::new(scope, name));
        }
    }

    // On-disk path form (legacy): "global/<name>" or "projects/<path>/<name>"
    //
    // WARNING: This branch is ambiguous for multi-segment scopes. See the
    // function-level doc comment for details. The canonical "v1:scope=...;name=..."
    // form should be preferred for all new entries.
    if let Some(rest) = qualified.strip_prefix("global/") {
        let name = MemoryName::new(rest)?;
        return Ok(MemoryRef::new(Scope::Root, name));
    }
    if let Some(rest) = qualified.strip_prefix("projects/") {
        // rest = "<scope-path>/<memory_name>"
        // The scope is always the first path segment after "projects/"; the
        // memory name is everything after that first slash.  This mirrors the
        // original on-disk layout where namespace names never contained slashes.
        // Using find (first slash) rather than rfind preserves that semantics
        // and avoids misparses like scope="proj/nested" name="mem" for the
        // path "projects/proj/nested/mem".
        //
        // For hierarchical scopes, this will produce an incorrect scope
        // (first segment only) and an incorrect name (remaining segments +
        // actual name). This is acceptable because all new index entries use
        // the canonical "v1:scope=...;name=..." form; this branch only handles
        // legacy keys from pre-hierarchical-scope builds.
        if let Some(first_slash) = rest.find('/') {
            let scope_path = &rest[..first_slash];
            let name_str = &rest[first_slash + 1..];
            if scope_path.is_empty() || name_str.is_empty() {
                return Err(MemoryError::InvalidInput {
                    reason: format!(
                        "malformed qualified name '{}': scope path or memory name is empty",
                        qualified
                    ),
                });
            }
            let name = MemoryName::new(name_str)?;
            return Ok(MemoryRef::new(
                Scope::Path(ScopePath::new(scope_path)?),
                name,
            ));
        }
        return Err(MemoryError::InvalidInput {
            reason: format!(
                "malformed qualified name '{}': missing memory name after scope path",
                qualified
            ),
        });
    }
    Err(MemoryError::InvalidInput {
        reason: format!(
            "malformed qualified name '{}': must start with 'v1:scope=', 'scope=', 'global/', or 'projects/'",
            qualified
        ),
    })
}

#[cfg(test)]
#[path = "memory_tests.rs"]
mod tests;
