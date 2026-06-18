//! Language versioning and feature gating.

use core::fmt;

/// A version of the C# language.
///
/// Only [`LanguageVersion::CSharp1`] is implemented. Later variants exist so
/// that the feature table and diagnostics can name versions we have not built
/// yet; [`LanguageVersion::parse_flag`] refuses to select them.
///
/// Ordering follows release order, so `>=` is the natural way to ask whether a
/// version is recent enough for a given feature.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum LanguageVersion {
    /// C# 1.0, as standardised by ECMA-334 1st edition (December 2001).
    CSharp1,
    /// C# 2.0. Present only to gate post-1.0 features; not yet implemented.
    CSharp2,
}

impl LanguageVersion {
    /// The newest language version this compiler can process today.
    pub const LATEST_SUPPORTED: LanguageVersion = LanguageVersion::CSharp1;

    /// The version selected when the caller does not request one explicitly.
    pub const DEFAULT: LanguageVersion = LanguageVersion::LATEST_SUPPORTED;

    /// Returns `true` when `feature` is available in this language version.
    #[must_use]
    pub fn supports(self, feature: Feature) -> bool {
        self >= feature.introduced_in()
    }

    /// Returns `true` when this compiler can actually compile this version,
    /// rather than merely name it for gating.
    #[must_use]
    pub fn is_implemented(self) -> bool {
        self <= Self::LATEST_SUPPORTED
    }

    /// Parses a csc-compatible `/langversion` value such as `ISO-1`, `1`,
    /// `default`, or `latest`.
    ///
    /// Matching is case-insensitive and ignores surrounding whitespace. A value
    /// that names a real but unimplemented version yields
    /// [`LanguageVersionError::Unsupported`]; a value that names no known
    /// version yields [`LanguageVersionError::Invalid`]. Separating the two lets
    /// the driver explain the difference precisely.
    pub fn parse_flag(value: &str) -> Result<LanguageVersion, LanguageVersionError> {
        let value = value.trim();
        if value.eq_ignore_ascii_case("default") {
            return Ok(Self::DEFAULT);
        }
        if value.eq_ignore_ascii_case("latest") || value.eq_ignore_ascii_case("latestmajor") {
            return Ok(Self::LATEST_SUPPORTED);
        }
        if value.eq_ignore_ascii_case("iso-1") || value == "1" || value == "1.0" {
            return Ok(Self::CSharp1);
        }
        if is_known_future_version(value) {
            return Err(LanguageVersionError::Unsupported);
        }
        Err(LanguageVersionError::Invalid)
    }

    /// The csc-compatible flag value that selects this version.
    #[must_use]
    pub fn flag_value(self) -> &'static str {
        match self {
            LanguageVersion::CSharp1 => "ISO-1",
            LanguageVersion::CSharp2 => "ISO-2",
        }
    }

    /// A human-readable name such as `C# 1.0`.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            LanguageVersion::CSharp1 => "C# 1.0",
            LanguageVersion::CSharp2 => "C# 2.0",
        }
    }
}

impl fmt::Display for LanguageVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.display_name())
    }
}

/// Returns `true` when `value` names a C# version that exists in the wider
/// language but is beyond what we implement today.
///
/// This is a flat list rather than a parsed number so the driver can reject,
/// say, `-langversion:7.3` with a precise message long before that version's
/// [`LanguageVersion`] variant exists.
fn is_known_future_version(value: &str) -> bool {
    const KNOWN: &[&str] = &[
        "iso-2", "2", "2.0", "3", "4", "5", "6", "7", "7.1", "7.2", "7.3", "8", "9", "10", "11",
        "12", "13", "14", "preview",
    ];
    KNOWN.iter().any(|known| value.eq_ignore_ascii_case(known))
}

/// A language feature that the front end gates on a [`LanguageVersion`].
///
/// The table is seeded with a few post-1.0 features to fix the pattern: when we
/// implement a feature we add its variant and the version that introduced it,
/// and the parser or binder calls [`LanguageVersion::supports`] before accepting
/// the construct. C# 1.0 features need no gate and so do not appear here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Feature {
    /// Generic types and methods, for example `List<T>`. Introduced in C# 2.0.
    Generics,
    /// Anonymous methods, for example `delegate (int x) { return x; }`.
    /// Introduced in C# 2.0.
    AnonymousMethods,
    /// Nullable value types, for example `int?`. Introduced in C# 2.0.
    NullableValueTypes,
}

impl Feature {
    /// The first language version in which this feature is available.
    #[must_use]
    pub fn introduced_in(self) -> LanguageVersion {
        match self {
            Feature::Generics | Feature::AnonymousMethods | Feature::NullableValueTypes => {
                LanguageVersion::CSharp2
            }
        }
    }

    /// A short noun phrase for the feature, used in "feature requires C# N"
    /// diagnostics.
    #[must_use]
    pub fn description(self) -> &'static str {
        match self {
            Feature::Generics => "generics",
            Feature::AnonymousMethods => "anonymous methods",
            Feature::NullableValueTypes => "nullable value types",
        }
    }
}

/// The reason a `/langversion` value could not be turned into a
/// [`LanguageVersion`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanguageVersionError {
    /// The value names a real C# version that this compiler does not implement
    /// yet, for example `ISO-2` while only C# 1.0 is supported.
    Unsupported,
    /// The value names no known C# version.
    Invalid,
}

impl fmt::Display for LanguageVersionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LanguageVersionError::Unsupported => {
                f.write_str("that C# version is not supported by this compiler yet")
            }
            LanguageVersionError::Invalid => f.write_str("unrecognised C# language version"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_latest_are_csharp1() {
        assert_eq!(LanguageVersion::DEFAULT, LanguageVersion::CSharp1);
        assert_eq!(LanguageVersion::LATEST_SUPPORTED, LanguageVersion::CSharp1);
        assert!(LanguageVersion::CSharp1.is_implemented());
        assert!(!LanguageVersion::CSharp2.is_implemented());
    }

    #[test]
    fn csharp1_does_not_support_post_1_0_features() {
        let v1 = LanguageVersion::CSharp1;
        assert!(!v1.supports(Feature::Generics));
        assert!(!v1.supports(Feature::AnonymousMethods));
        assert!(!v1.supports(Feature::NullableValueTypes));
    }

    #[test]
    fn csharp2_label_supports_its_own_features() {
        assert!(LanguageVersion::CSharp2.supports(Feature::Generics));
    }

    #[test]
    fn parse_flag_accepts_csharp1_spellings() {
        for value in [
            "ISO-1",
            "iso-1",
            "1",
            "1.0",
            " 1 ",
            "default",
            "latest",
            "LATESTMAJOR",
        ] {
            assert_eq!(
                LanguageVersion::parse_flag(value),
                Ok(LanguageVersion::CSharp1),
                "value was {value:?}"
            );
        }
    }

    #[test]
    fn parse_flag_reports_unimplemented_versions_distinctly() {
        for value in ["ISO-2", "2", "2.0", "7.3", "14", "preview"] {
            assert_eq!(
                LanguageVersion::parse_flag(value),
                Err(LanguageVersionError::Unsupported),
                "value was {value:?}"
            );
        }
    }

    #[test]
    fn parse_flag_rejects_nonsense() {
        for value in ["", "csharp", "99", "1.5", "iso"] {
            assert_eq!(
                LanguageVersion::parse_flag(value),
                Err(LanguageVersionError::Invalid),
                "value was {value:?}"
            );
        }
    }

    #[test]
    fn flag_value_round_trips_for_supported_versions() {
        let v1 = LanguageVersion::CSharp1;
        assert_eq!(LanguageVersion::parse_flag(v1.flag_value()), Ok(v1));
    }
}
