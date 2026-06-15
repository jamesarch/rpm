//! Dependency matching: deciding whether a `Requires:` is satisfied by a `Provides:`.
//!
//! This is the core of RPM dependency handling. Everything else (transaction
//! ordering, full SAT solving) is built on top of a single question: does a
//! capability (`Provides:`) satisfy a requirement (`Requires:`)?
//!
//! That question reduces to **version range overlap**. A dependency is a triple
//! of `(name, sense, evr)` where the sense is some combination of `<`, `=`, `>`
//! ([`DependencyFlags::LESS`], [`DependencyFlags::EQUAL`], [`DependencyFlags::GREATER`]).
//! [`dependency_matches`] implements the same comparison as librpm's
//! `rpmdsCompare()`.
//!
//! # Example
//!
//! ```
//! use rpm::{Dependency, dependency_matches};
//!
//! // A package provides exactly foo-3.0, and something requires foo >= 2.0.
//! let provide = Dependency::eq("foo", "3.0");
//! let require = Dependency::greater_eq("foo", "2.0");
//! assert!(dependency_matches(&require, &provide));
//!
//! // foo > 3.0 is not satisfied by foo = 3.0.
//! let require = Dependency::greater("foo", "3.0");
//! assert!(!dependency_matches(&require, &provide));
//! ```

use std::cmp::Ordering;
use std::collections::HashMap;

use rpm_version::Evr;

use crate::constants::DependencyFlags;

use super::headers::Dependency;

/// The bits of [`DependencyFlags`] that describe a version comparison.
///
/// A dependency carries other bits too (e.g. [`DependencyFlags::RPMLIB`],
/// [`DependencyFlags::CONFIG`], the scriptlet phase bits). Those say *what kind*
/// of dependency it is, not *which versions* it ranges over, so they must be
/// masked off before reasoning about version overlap.
const SENSE: DependencyFlags = DependencyFlags::LESS
    .union(DependencyFlags::GREATER)
    .union(DependencyFlags::EQUAL);

impl Dependency {
    /// Returns the version-comparison sense (`<` / `=` / `>` bits only).
    fn sense(&self) -> DependencyFlags {
        self.flags.intersection(SENSE)
    }

    /// Whether this dependency constrains a version range at all.
    ///
    /// An *unversioned* dependency (no sense bits, or an empty version string)
    /// matches purely by name. For example `Provides: webserver` (no version)
    /// satisfies `Requires: webserver >= 2`.
    pub fn is_versioned(&self) -> bool {
        !self.sense().is_empty() && !self.version.is_empty()
    }
}

/// Returns `true` if `provide` satisfies `require`.
///
/// This implements the same logic as librpm's `rpmdsCompare()`:
///
/// 1. The names must be exactly equal (string equality, not version compare).
/// 2. If either side is unversioned, a name match is enough — an unversioned
///    `Provides` satisfies any versioned `Requires` of the same name, and vice
///    versa.
/// 3. Otherwise the two version ranges must overlap. With
///    `cmp = rpmvercmp(provide.evr, require.evr)`:
///    - `cmp < 0`: overlap iff `provide` ranges upward or `require` ranges downward.
///    - `cmp > 0`: overlap iff `provide` ranges downward or `require` ranges upward.
///    - `cmp == 0`: overlap iff both share an open direction at the boundary.
///
/// Epoch handling follows [`rpm_version::Evr`]: a missing epoch is treated as `0`.
///
/// Note: this compares two capability expressions. It does **not** handle
/// file-based requirements (e.g. `Requires: /bin/sh`); those are satisfied by a
/// package's *file list*, which the caller should fold into the provides set as
/// unversioned dependencies (see [`DependencySet`]).
pub fn dependency_matches(require: &Dependency, provide: &Dependency) -> bool {
    // 1. Names must match exactly.
    if require.name != provide.name {
        return false;
    }

    // 2. An unversioned side matches by name alone.
    if !require.is_versioned() || !provide.is_versioned() {
        return true;
    }

    // 3. Both versioned: test range overlap.
    let p = provide.sense();
    let r = require.sense();
    let cmp = Evr::parse(&provide.version).cmp(&Evr::parse(&require.version));

    match cmp {
        Ordering::Less => p.contains(DependencyFlags::GREATER) || r.contains(DependencyFlags::LESS),
        Ordering::Greater => {
            p.contains(DependencyFlags::LESS) || r.contains(DependencyFlags::GREATER)
        }
        Ordering::Equal => {
            (p.contains(DependencyFlags::EQUAL) && r.contains(DependencyFlags::EQUAL))
                || (p.contains(DependencyFlags::LESS) && r.contains(DependencyFlags::LESS))
                || (p.contains(DependencyFlags::GREATER) && r.contains(DependencyFlags::GREATER))
        }
    }
}

/// An index of capabilities (`Provides:`, plus file paths) that can answer
/// "which owners satisfy this requirement?".
///
/// `T` identifies the owner of a capability — typically a package name, an index
/// into your own package list, or a borrowed package reference.
///
/// Capabilities are bucketed by name so a lookup only has to version-compare the
/// handful of provides that share the required name, rather than scanning every
/// capability in the set.
#[derive(Debug, Clone, Default)]
pub struct DependencySet<T> {
    by_name: HashMap<String, Vec<(Dependency, T)>>,
}

impl<T> DependencySet<T> {
    /// Create an empty set.
    pub fn new() -> Self {
        DependencySet {
            by_name: HashMap::new(),
        }
    }

    /// Register a single capability owned by `owner`.
    pub fn insert(&mut self, capability: Dependency, owner: T) {
        self.by_name
            .entry(capability.name.clone())
            .or_default()
            .push((capability, owner));
    }

    /// Register a bare name as an unversioned capability (e.g. a file path from a
    /// package's file list, which satisfies `Requires: /that/path`).
    pub fn insert_name(&mut self, name: impl Into<String>, owner: T) {
        self.insert(Dependency::any(name), owner);
    }

    /// Returns every owner whose capability satisfies `require`.
    ///
    /// Order follows insertion order within the matching name bucket, so results
    /// are deterministic for a given build order.
    pub fn providers(&self, require: &Dependency) -> Vec<&T> {
        let Some(bucket) = self.by_name.get(&require.name) else {
            return Vec::new();
        };
        bucket
            .iter()
            .filter(|(cap, _)| dependency_matches(require, cap))
            .map(|(_, owner)| owner)
            .collect()
    }

    /// Whether any registered capability satisfies `require`.
    pub fn is_satisfied(&self, require: &Dependency) -> bool {
        self.by_name.get(&require.name).is_some_and(|bucket| {
            bucket
                .iter()
                .any(|(cap, _)| dependency_matches(require, cap))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- dependency_matches: name handling ---

    #[test]
    fn name_mismatch_never_matches() {
        assert!(!dependency_matches(
            &Dependency::any("foo"),
            &Dependency::any("bar"),
        ));
        assert!(!dependency_matches(
            &Dependency::greater_eq("foo", "1.0"),
            &Dependency::eq("bar", "1.0"),
        ));
    }

    #[test]
    fn unversioned_either_side_matches_by_name() {
        // Unversioned provide satisfies a versioned require.
        assert!(dependency_matches(
            &Dependency::greater_eq("foo", "2.0"),
            &Dependency::any("foo"),
        ));
        // Unversioned require is satisfied by any provide of the same name.
        assert!(dependency_matches(
            &Dependency::any("foo"),
            &Dependency::eq("foo", "0.1"),
        ));
        // Both unversioned.
        assert!(dependency_matches(
            &Dependency::any("foo"),
            &Dependency::any("foo"),
        ));
    }

    // --- dependency_matches: range overlap ---

    #[test]
    fn greater_eq_require_against_exact_provide() {
        let provide = Dependency::eq("foo", "3.0");
        assert!(dependency_matches(
            &Dependency::greater_eq("foo", "2.0"),
            &provide
        ));
        assert!(dependency_matches(
            &Dependency::greater_eq("foo", "3.0"),
            &provide
        ));
        assert!(!dependency_matches(
            &Dependency::greater_eq("foo", "4.0"),
            &provide
        ));
    }

    #[test]
    fn strict_greater_excludes_boundary() {
        let provide = Dependency::eq("foo", "2.0");
        assert!(!dependency_matches(
            &Dependency::greater("foo", "2.0"),
            &provide
        ));
        assert!(dependency_matches(
            &Dependency::greater("foo", "1.0"),
            &provide
        ));
    }

    #[test]
    fn less_and_less_eq() {
        let provide = Dependency::eq("foo", "2.0");
        assert!(dependency_matches(
            &Dependency::less("foo", "3.0"),
            &provide
        ));
        assert!(!dependency_matches(
            &Dependency::less("foo", "2.0"),
            &provide
        ));
        assert!(dependency_matches(
            &Dependency::less_eq("foo", "2.0"),
            &provide
        ));
    }

    #[test]
    fn exact_equal_both_sides() {
        let provide = Dependency::eq("foo", "2.0");
        assert!(dependency_matches(&Dependency::eq("foo", "2.0"), &provide));
        assert!(!dependency_matches(&Dependency::eq("foo", "2.1"), &provide));
    }

    #[test]
    fn ranged_provide_overlaps_ranged_require() {
        // provide foo >= 2.0 overlaps require foo < 5.0 (their ranges intersect).
        assert!(dependency_matches(
            &Dependency::less("foo", "5.0"),
            &Dependency::greater_eq("foo", "2.0"),
        ));
        // provide foo >= 6.0 does NOT overlap require foo < 5.0.
        assert!(!dependency_matches(
            &Dependency::less("foo", "5.0"),
            &Dependency::greater_eq("foo", "6.0"),
        ));
    }

    #[test]
    fn epoch_dominates_version() {
        // An epoch-1 provide outranks an epoch-0 (implicit) require boundary.
        let provide = Dependency::eq("foo", "1:1.0");
        assert!(dependency_matches(
            &Dependency::greater_eq("foo", "9.0"),
            &provide
        ));
        // Provide without epoch is treated as epoch 0, below an epoch-1 require.
        let provide = Dependency::eq("foo", "2.0");
        assert!(!dependency_matches(
            &Dependency::greater_eq("foo", "1:1.0"),
            &provide
        ));
    }

    #[test]
    fn release_participates_in_comparison() {
        let provide = Dependency::eq("foo", "1.0-2");
        assert!(dependency_matches(
            &Dependency::greater("foo", "1.0-1"),
            &provide
        ));
        assert!(!dependency_matches(
            &Dependency::greater("foo", "1.0-2"),
            &provide
        ));
    }

    #[test]
    fn non_sense_flags_do_not_imply_versioning() {
        // A dependency with only context bits (e.g. RPMLIB) but no sense bits and
        // no version must behave as unversioned.
        let require = Dependency {
            name: "rpmlib(CompressedFileNames)".to_string(),
            flags: DependencyFlags::RPMLIB,
            version: String::new(),
        };
        let provide = Dependency::any("rpmlib(CompressedFileNames)");
        assert!(dependency_matches(&require, &provide));
    }

    // --- DependencySet ---

    #[test]
    fn dependency_set_finds_providers() {
        let mut set = DependencySet::new();
        set.insert(Dependency::eq("foo", "3.0"), "pkg-foo");
        set.insert(Dependency::any("webserver"), "pkg-nginx");
        set.insert(Dependency::any("webserver"), "pkg-apache");

        // Versioned lookup hits the one matching provide.
        let providers = set.providers(&Dependency::greater_eq("foo", "2.0"));
        assert_eq!(providers, vec![&"pkg-foo"]);

        // Unversioned virtual capability has two providers, in insertion order.
        let providers = set.providers(&Dependency::any("webserver"));
        assert_eq!(providers, vec![&"pkg-nginx", &"pkg-apache"]);

        // No provider for an out-of-range request.
        assert!(set.providers(&Dependency::greater("foo", "9.0")).is_empty());
        assert!(!set.is_satisfied(&Dependency::greater("foo", "9.0")));
        assert!(set.is_satisfied(&Dependency::greater_eq("foo", "2.0")));
    }

    #[test]
    fn dependency_set_handles_file_capabilities() {
        let mut set = DependencySet::new();
        set.insert_name("/bin/sh", "pkg-bash");

        assert!(set.is_satisfied(&Dependency::any("/bin/sh")));
        assert_eq!(
            set.providers(&Dependency::any("/bin/sh")),
            vec![&"pkg-bash"]
        );
        assert!(!set.is_satisfied(&Dependency::any("/bin/zsh")));
    }
}
