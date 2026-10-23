//! One answer to "which version is this record?", for every persisted structure.
//!
//! CONTRACT.md §6 C6.1 requires each durable structure to carry a schema
//! version, and requires the **read** side to check it — a version that is only
//! written is a label, not a compatibility boundary. Before this module the
//! workspace wrote the event envelope's `schema_version` and checked it
//! nowhere (YA-21), the subagent wire had no version at all (YA-20), and no
//! configuration layer had one (YA-67): a record from a future build was
//! either parsed on a best-effort basis or refused with a generic syntax
//! error that did not say "version".
//!
//! The pieces:
//!
//! * [`Version`] — a small, ordered version value. One to three dot-separated
//!   decimal components; `0.1.0`, `1`, and `2.4` are all versions, and
//!   `1` equals `1.0.0` in comparison even though [`Display`] echoes the
//!   spelling it was written with.
//! * [`check`] — the read-side rule with no migration table: equal is readable,
//!   newer is [`VersionError::Future`], older is [`VersionError::NoMigration`].
//!   Production uses this, and only this, today.
//! * [`MigrationChain`] — the engine a future build will use to bring an older
//!   record forward. The production chains are empty on purpose: there is no
//!   format at version 2 yet, so there is no step to write. The engine is
//!   nevertheless built and tested against a fixture v1→v2 pair, because the
//!   acceptance criterion is "a v1 record is read by v2 code without loss",
//!   and a claim about migration that has never migrated anything is not a
//!   claim anyone should rely on.
//!
//! What this module deliberately does **not** do: decide a version per record
//! from a global number. C6.2 says compatibility comes from behavioral
//! contracts, not from one workspace-wide version, so each format's current
//! version is named next to the format's readers (see [`formats`]) and moves
//! only when that format's shape moves.
//!
//! ## The one exception: events
//!
//! The event envelope's version already has a home, [`SchemaVersion::CURRENT`],
//! and a guard that forbids a second literal
//! (`agent-testkit/tests/schema_version_boundaries.rs`). [`event_current`]
//! therefore *parses* that home rather than restating its value; a bump there
//! moves the reader in the same commit or not at all.
//!
//! [`Display`]: std::fmt::Display

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ids::SchemaVersion;

/// A schema version: one to three dot-separated decimal components.
///
/// Comparison is over the numeric components, padded with zeros, so `1`,
/// `1.0`, and `1.0.0` are the same version. `Display` preserves the spelling
/// the value was parsed from (a `Version::new(1, 0, 0)` prints `1.0.0`), which
/// keeps an error message quoting a file's `schema_version = "1"` recognizable.
/// Equality, ordering, and hashing all use the numeric form, so a `BTreeSet`
/// holding `1` already contains `1.0.0`.
#[derive(Debug, Clone)]
pub struct Version {
    /// Major, minor, patch; missing components are zero.
    parts: [u32; 3],
    /// How many components the value was written with, for `Display` only.
    written: u8,
}

impl Version {
    /// A three-component version.
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self { parts: [major, minor, patch], written: 3 }
    }

    /// Parse `1`, `1.2`, or `1.2.3`.
    ///
    /// Only canonical decimal spellings are accepted: no empty components, no
    /// signs, no whitespace, and no leading zeros (`01` is refused rather than
    /// silently read as `1`). Two spellings of one number would make "the same
    /// version" depend on which file a reader happened to open.
    pub fn parse(text: &str) -> Result<Self, VersionError> {
        let malformed = |detail: &str| VersionError::Malformed {
            found: text.to_string(),
            detail: detail.to_string(),
        };

        if text.is_empty() {
            return Err(malformed("it is empty"));
        }

        let segments: Vec<&str> = text.split('.').collect();
        if segments.len() > 3 {
            return Err(malformed("it has more than three components"));
        }

        let mut parts = [0u32; 3];
        for (index, segment) in segments.iter().enumerate() {
            if segment.is_empty() {
                return Err(malformed("it has an empty component"));
            }
            if !segment.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(malformed("every component must be a decimal number"));
            }
            if segment.len() > 1 && segment.starts_with('0') {
                return Err(malformed("a component may not have a leading zero"));
            }
            parts[index] =
                segment.parse::<u32>().map_err(|_| malformed("a component is too large"))?;
        }

        Ok(Self { parts, written: segments.len() as u8 })
    }

    /// The major component.
    pub fn major(&self) -> u32 {
        self.parts[0]
    }

    /// The minor component.
    pub fn minor(&self) -> u32 {
        self.parts[1]
    }

    /// The patch component.
    pub fn patch(&self) -> u32 {
        self.parts[2]
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        // Numeric identity, not spelling: `1` and `1.0.0` are one version.
        self.parts == other.parts
    }
}

impl Eq for Version {}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.parts.cmp(&other.parts)
    }
}

impl std::hash::Hash for Version {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        // Must agree with `Eq`: the spelling is not part of the value.
        self.parts.hash(state);
    }
}

impl fmt::Display for Version {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let written = usize::from(self.written.clamp(1, 3));
        for (index, part) in self.parts[..written].iter().enumerate() {
            if index > 0 {
                formatter.write_str(".")?;
            }
            write!(formatter, "{part}")?;
        }
        Ok(())
    }
}

impl FromStr for Version {
    type Err = VersionError;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(serde::de::Error::custom)
    }
}

/// Why a version could not be read, or could not be brought forward.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum VersionError {
    #[error("'{found}' is not a valid version: {detail}")]
    Malformed { found: String, detail: String },
    /// The record was written by a build newer than this one. This is refused
    /// rather than parsed best-effort: a reader that does not know a field's
    /// meaning must not guess at it.
    #[error(
        "'{found}' is newer than this build understands (this build supports up to \
         '{supported}')"
    )]
    Future { found: String, supported: String },
    /// The record is older, and no chain in this build can bring it forward.
    #[error(
        "'{found}' is older than this build reads, and no migration from it exists (this \
         build reads '{supported}')"
    )]
    NoMigration { found: String, supported: String },
    /// A migration step existed but could not transform the record.
    #[error("migrating '{from}' to '{to}' failed: {detail}")]
    MigrationFailed { from: String, to: String, detail: String },
}

/// The current version of each persisted format this build reads.
///
/// One entry per format, deliberately not one number for the workspace
/// (CONTRACT.md §6 C6.2): these values move independently, because a change to
/// the wire says nothing about the event log.
///
/// Events are absent here — their home is [`SchemaVersion::CURRENT`], reached
/// through [`event_current`].
pub mod formats {
    use super::Version;

    /// `managed policy`, `user config`, and `workspace config`, all one shape
    /// family read by `agent-config` (YA-67).
    pub const CONFIG: Version = Version::new(1, 0, 0);

    /// The plugin manifest a registry accepts (YA-21's manifest half).
    pub const PLUGIN_MANIFEST: Version = Version::new(1, 0, 0);

    /// The durable decision trace `agent-decision` replays (YA-21's trace half).
    pub const DECISION_TRACE: Version = Version::new(1, 0, 0);

    /// The parent/child stdio wire (YA-20).
    pub const SUBAGENT_WIRE: Version = Version::new(1, 0, 0);
}

/// The event envelope's current version, parsed from its one home.
///
/// Kept a function rather than a constant because restating `0.1.0` here would
/// create the second answer `schema_version_boundaries.rs` exists to prevent.
/// The parse is total for the constant it is given; the test module asserts it.
pub fn event_current() -> Version {
    Version::parse(SchemaVersion::CURRENT)
        .expect("SchemaVersion::CURRENT is a version; see the versioning tests")
}

/// Serde defaults: a record that predates the field is treated as current.
///
/// This is a deliberate compatibility decision, not an oversight. Every format
/// here gained its version field after records already existed, so "absent"
/// can only mean "written by a build that did not stamp one" — and those
/// records were written by a build whose shape is today's. Rejecting them
/// would make the version field a flag day rather than a boundary.
pub fn current_plugin_manifest() -> Version {
    formats::PLUGIN_MANIFEST
}

/// See [`current_plugin_manifest`].
pub fn current_decision_trace() -> Version {
    formats::DECISION_TRACE
}

/// See [`current_plugin_manifest`].
pub fn current_subagent_wire() -> Version {
    formats::SUBAGENT_WIRE
}

/// Read-side check against a current version, with no migration table.
///
/// This is the whole rule production runs today: equal is readable, future is
/// [`VersionError::Future`], past is [`VersionError::NoMigration`]. An older
/// record could in principle be migrated, but no chain in this build has a
/// step for it, so saying "no migration exists" is the truth rather than a
/// placeholder.
pub fn check(found: &Version, current: &Version) -> Result<(), VersionError> {
    if found > current {
        return Err(VersionError::Future {
            found: found.to_string(),
            supported: current.to_string(),
        });
    }
    if found < current {
        return Err(VersionError::NoMigration {
            found: found.to_string(),
            supported: current.to_string(),
        });
    }
    Ok(())
}

/// Check the envelope version of one event (YA-21).
pub fn check_event(found: &SchemaVersion) -> Result<(), VersionError> {
    let found = Version::parse(found.as_str())?;
    check(&found, &event_current())
}

/// One step in a migration chain: transform a record from `from` to `to`.
///
/// The function receives and returns a JSON value. JSON is the interchange
/// shape every persisted format already has (config is TOML at rest but is
/// converted to a value before this runs), and a step that returned a typed
/// struct would make the chain know every format's Rust type.
struct MigrationStep {
    from: Version,
    to: Version,
    apply: fn(Value) -> Result<Value, String>,
}

/// An ordered set of migration steps a reader applies to an old record.
///
/// Production chains are empty — `MigrationChain::empty` is the only
/// constructor used by the formats, and it rejects every version below
/// current. The engine exists now, and is tested now, so that the first real
/// schema bump is a matter of adding a step rather than inventing the
/// mechanism under time pressure.
pub struct MigrationChain {
    format: &'static str,
    current: Version,
    steps: Vec<MigrationStep>,
}

impl MigrationChain {
    /// A chain with no steps. Its read-side rule is exactly [`check`].
    pub fn empty(format: &'static str, current: Version) -> Self {
        Self { format, current, steps: Vec::new() }
    }

    /// Add a step from `from` to `to`.
    ///
    /// A step that does not move forward is a chain bug, not input: assert it
    /// here, where the mistake is a fixture typo, rather than let [`migrate`]
    /// loop or claim a path it cannot walk.
    ///
    /// [`migrate`]: Self::migrate
    pub fn with_step(
        mut self,
        from: Version,
        to: Version,
        apply: fn(Value) -> Result<Value, String>,
    ) -> Self {
        assert!(to > from, "a migration step must move forward: {from} -> {to}");
        self.steps.push(MigrationStep { from, to, apply });
        self
    }

    /// The format this chain is named for, used in errors and tests.
    pub fn format(&self) -> &'static str {
        self.format
    }

    /// The version a record must reach.
    pub fn current(&self) -> &Version {
        &self.current
    }

    /// Whether `found` is readable without transforming it.
    ///
    /// Callers that only need to reject (rather than upgrade) may use this;
    /// [`migrate`](Self::migrate) runs the same checks and then the steps.
    pub fn check(&self, found: &Version) -> Result<(), VersionError> {
        if found > &self.current {
            return Err(VersionError::Future {
                found: found.to_string(),
                supported: self.current.to_string(),
            });
        }
        if found == &self.current {
            return Ok(());
        }
        if self.path(found).is_none() {
            return Err(VersionError::NoMigration {
                found: found.to_string(),
                supported: self.current.to_string(),
            });
        }
        Ok(())
    }

    /// Bring `value`, written at `found`, up to the current version.
    ///
    /// A missing path is refused **before** any step runs, so a failed
    /// migration never leaves a half-transformed value that a caller could
    /// mistake for a record at some intermediate version.
    pub fn migrate(&self, found: &Version, value: Value) -> Result<Value, VersionError> {
        self.check(found)?;
        let mut value = value;
        let mut cursor = found.clone();
        while cursor < self.current {
            let step = self
                .steps
                .iter()
                .find(|step| step.from == cursor)
                .expect("check() established that a path exists");
            let next = step.to.clone();
            value = (step.apply)(value).map_err(|detail| VersionError::MigrationFailed {
                from: cursor.to_string(),
                to: next.to_string(),
                detail,
            })?;
            cursor = next;
        }
        Ok(value)
    }

    /// Whether a forward path of steps connects `found` to current.
    ///
    /// The visited set bounds a malformed chain; `with_step` already refuses a
    /// non-forward step, so in a chain built through the public API this is a
    /// straight walk.
    fn path(&self, found: &Version) -> Option<()> {
        let mut cursor = found.clone();
        let mut visited = BTreeSet::new();
        while cursor < self.current {
            if !visited.insert(cursor.clone()) {
                return None;
            }
            let step = self.steps.iter().find(|step| step.from == cursor)?;
            cursor = step.to.clone();
        }
        (cursor == self.current).then_some(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn one_version_can_be_spelled_with_one_two_or_three_components() {
        assert_eq!(Version::parse("1").unwrap(), Version::new(1, 0, 0));
        assert_eq!(Version::parse("1.2").unwrap(), Version::new(1, 2, 0));
        assert_eq!(Version::parse("1.2.3").unwrap(), Version::new(1, 2, 3));
        assert_eq!(Version::parse("0.1.0").unwrap(), Version::new(0, 1, 0));
    }

    #[test]
    fn display_echoes_the_spelling_but_equality_is_numeric() {
        let short = Version::parse("1").unwrap();
        assert_eq!(short.to_string(), "1");
        assert_eq!(formats::CONFIG.to_string(), "1.0.0");
        assert_eq!(short, formats::CONFIG);
        // And a set that holds one spelling recognizes the other.
        let mut set = BTreeSet::new();
        set.insert(short);
        assert!(set.contains(&formats::CONFIG));
    }

    #[test]
    fn ordering_is_by_component_not_by_text() {
        assert!(Version::parse("0.1.0").unwrap() < Version::parse("1").unwrap());
        assert!(Version::parse("1.10.0").unwrap() > Version::parse("1.9.9").unwrap());
        assert!(Version::parse("2").unwrap() > Version::new(1, 99, 99));
    }

    #[test]
    fn a_malformed_version_says_what_is_wrong() {
        for text in ["", ".", "1.", ".1", "1..2", "1.2.3.4", "1.a", "-1", " 1", "1 ", "01", "1.00"]
        {
            let error = Version::parse(text).expect_err(text);
            assert!(matches!(error, VersionError::Malformed { .. }), "{text}: {error}");
        }
        // A number too large for the component type is malformed, not a panic.
        assert!(Version::parse("4294967296").is_err());
    }

    #[test]
    fn a_version_serializes_as_a_string_and_reads_back() {
        let value = serde_json::to_value(Version::parse("2.4").unwrap()).unwrap();
        assert_eq!(value, json!("2.4"));
        let decoded: Version = serde_json::from_value(value).unwrap();
        assert_eq!(decoded, Version::new(2, 4, 0));
        assert!(serde_json::from_value::<Version>(json!("nope")).is_err());
    }

    #[test]
    fn equal_is_readable_newer_is_future_and_older_has_no_migration() {
        let current = formats::CONFIG;
        assert_eq!(check(&Version::parse("1.0.0").unwrap(), &current), Ok(()));
        assert!(matches!(
            check(&Version::parse("99").unwrap(), &current),
            Err(VersionError::Future { .. })
        ));
        assert!(matches!(
            check(&Version::parse("0.9").unwrap(), &current),
            Err(VersionError::NoMigration { .. })
        ));
    }

    #[test]
    fn the_event_version_comes_from_its_one_home() {
        // `SchemaVersion::CURRENT` is `0.1.0`; the reader derives it rather than
        // restating it, and this probe makes the derivation visible.
        assert_eq!(event_current(), Version::new(0, 1, 0));
        assert_eq!(check_event(&SchemaVersion::current()), Ok(()));
        assert!(matches!(
            check_event(&SchemaVersion::from_string("99.0.0")),
            Err(VersionError::Future { .. })
        ));
        assert!(matches!(
            check_event(&SchemaVersion::from_string("0.0.9")),
            Err(VersionError::NoMigration { .. })
        ));
    }

    fn add_field(value: Value) -> Result<Value, String> {
        let mut object = value.as_object().cloned().ok_or("not an object")?;
        object.insert("added_in_v2".to_string(), json!(true));
        Ok(Value::Object(object))
    }

    fn refuse(_: Value) -> Result<Value, String> {
        Err("the fixture cannot be transformed".to_string())
    }

    #[test]
    fn a_v1_record_is_brought_to_v2_without_losing_a_field() {
        // The acceptance criterion for CONTRACT.md §9 #2, proved on the engine
        // itself while every production chain is empty: a record written by v1
        // code reaches v2 code, and the fields it already had survive.
        let chain = MigrationChain::empty("fixture", Version::new(2, 0, 0)).with_step(
            Version::new(1, 0, 0),
            Version::new(2, 0, 0),
            add_field,
        );

        let v1 = json!({ "kept": 7 });
        let migrated = chain.migrate(&Version::new(1, 0, 0), v1).expect("v1 is migratable");
        assert_eq!(migrated, json!({ "kept": 7, "added_in_v2": true }));
        assert_eq!(chain.check(&Version::new(1, 0, 0)), Ok(()));
    }

    #[test]
    fn an_empty_chain_rejects_an_older_record() {
        let chain = MigrationChain::empty("config", formats::CONFIG);
        let error = chain.migrate(&Version::parse("0.9").unwrap(), json!({})).unwrap_err();
        assert!(matches!(error, VersionError::NoMigration { .. }), "{error}");
    }

    #[test]
    fn a_chain_does_not_best_effort_an_unrelated_older_version() {
        let chain = MigrationChain::empty("fixture", Version::new(2, 0, 0)).with_step(
            Version::new(1, 0, 0),
            Version::new(2, 0, 0),
            add_field,
        );
        let error = chain.migrate(&Version::new(0, 5, 0), json!({})).unwrap_err();
        assert!(matches!(error, VersionError::NoMigration { .. }), "{error}");
    }

    #[test]
    fn a_step_that_fails_is_reported_as_a_migration_failure() {
        let chain = MigrationChain::empty("fixture", Version::new(2, 0, 0)).with_step(
            Version::new(1, 0, 0),
            Version::new(2, 0, 0),
            refuse,
        );
        let error = chain.migrate(&Version::new(1, 0, 0), json!({})).unwrap_err();
        assert!(
            matches!(&error, VersionError::MigrationFailed { from, to, .. } if from == "1.0.0" && to == "2.0.0"),
            "{error}"
        );
    }

    #[test]
    fn a_record_from_the_future_is_rejected_before_any_step_runs() {
        let chain = MigrationChain::empty("fixture", Version::new(2, 0, 0)).with_step(
            Version::new(1, 0, 0),
            Version::new(2, 0, 0),
            add_field,
        );
        let error = chain.migrate(&Version::new(99, 0, 0), json!({})).unwrap_err();
        assert!(matches!(error, VersionError::Future { .. }), "{error}");
    }
}
