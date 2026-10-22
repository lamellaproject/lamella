//! The packages a C# program names, and which build of each it gets: NuGet's version rule over the
//! versions Lamella builds, with NuGet's diagnostic codes.

use std::cmp::Ordering;
use std::fmt;

/// A package version as NuGet writes it: `major.minor[.patch[.revision]][-label][+metadata]`.
///
/// Two spellings of one version are the same version, as they are to NuGet: `4.2` is `4.2.0`, and
/// build metadata after `+` is ignored.
#[derive(Clone, Debug)]
pub struct Version {
    /// Major, minor, patch and revision, the missing ones zero.
    numbers: [u64; 4],
    /// The pre-release label's dot-separated identifiers; empty for a stable version.
    label: Vec<String>,
}

impl Version {
    /// `text` as a version, or `None` when it is not one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = text.split_once('+').map_or(text, |(version, _)| version);
        let (numbers, label) = match text.split_once('-') {
            Some((numbers, label)) => (numbers, Some(label)),
            None => (text, None),
        };
        let parts: Vec<&str> = numbers.split('.').collect();
        if parts.len() > 4 {
            return None;
        }
        let mut parsed = [0_u64; 4];
        for (slot, part) in parsed.iter_mut().zip(&parts) {
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            *slot = part.parse().ok()?;
        }
        let label = match label {
            None => Vec::new(),
            Some(label) => {
                let identifiers: Vec<String> = label.split('.').map(str::to_owned).collect();
                let valid = |one: &String| {
                    !one.is_empty() && one.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                };
                if !identifiers.iter().all(valid) {
                    return None;
                }
                identifiers
            }
        };
        Some(Self { numbers: parsed, label })
    }

    /// Whether this is a pre-release: it carries a label.
    #[must_use]
    pub fn is_prerelease(&self) -> bool {
        !self.label.is_empty()
    }
}

impl Ord for Version {
    /// SemVer 2.0's order, as NuGet applies it: the numbers first, then a stable version above every
    /// pre-release of the same numbers, then the labels identifier by identifier, a numeric one below
    /// an alphanumeric one and alphanumeric ones compared without regard to case.
    fn cmp(&self, other: &Self) -> Ordering {
        self.numbers.cmp(&other.numbers).then_with(|| match (self.label.is_empty(), other.label.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => compare_labels(&self.label, &other.label),
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Version {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Version {}

impl fmt::Display for Version {
    /// The normalized form NuGet prints: three numbers, a fourth only when it is not zero.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let [major, minor, patch, revision] = self.numbers;
        write!(formatter, "{major}.{minor}.{patch}")?;
        if revision != 0 {
            write!(formatter, ".{revision}")?;
        }
        if !self.label.is_empty() {
            write!(formatter, "-{}", self.label.join("."))?;
        }
        Ok(())
    }
}

/// Two pre-release labels in SemVer 2.0's order.
fn compare_labels(left: &[String], right: &[String]) -> Ordering {
    for (one, two) in left.iter().zip(right) {
        let order = match (one.parse::<u64>(), two.parse::<u64>()) {
            (Ok(one), Ok(two)) => one.cmp(&two),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => one.to_ascii_lowercase().cmp(&two.to_ascii_lowercase()),
        };
        if order != Ordering::Equal {
            return order;
        }
    }
    left.len().cmp(&right.len())
}

/// One end of a version range.
#[derive(Clone, Debug)]
struct Bound {
    version: Version,
    inclusive: bool,
}

/// A floating version: the highest build whose leading numbers, and label where it names one,
/// match.
#[derive(Clone, Debug)]
struct Float {
    /// The numbers written before the `*`: none for `*`, one for `4.*`.
    fixed: Vec<u64>,
    /// For `4.2.0-rc.*`, the label prefix (`rc.`); for `4.2.0-*`, the empty prefix.
    label_prefix: Option<String>,
    /// Whether pre-releases float too: `-*` after the numbers, or a label float.
    prerelease: bool,
}

impl Float {
    /// Whether `version` is one this float floats over.
    fn matches(&self, version: &Version) -> bool {
        if !version.numbers.iter().zip(&self.fixed).all(|(one, two)| one == two) {
            return false;
        }
        match &self.label_prefix {
            Some(prefix) => {
                !version.is_prerelease()
                    || version.label.join(".").to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase())
            }
            None => self.prerelease || !version.is_prerelease(),
        }
    }
}

/// A NuGet version range: `4.2.0` (that version or later), `[4.2.0]` (exactly), an interval such as
/// `[4.2.0,5.0.0)`, or a floating version such as `4.*`.
#[derive(Clone, Debug)]
pub struct Range {
    min: Option<Bound>,
    max: Option<Bound>,
    float: Option<Float>,
}

impl Range {
    /// `text` as a range, or `None` when it is not one.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if text.contains('*') {
            return Self::parse_float(text);
        }
        let opens = text.starts_with('[') || text.starts_with('(');
        if !opens {
            let version = Version::parse(text)?;
            return Some(Self { min: Some(Bound { version, inclusive: true }), max: None, float: None });
        }
        let low_inclusive = text.starts_with('[');
        let high_inclusive = match text.chars().last() {
            Some(']') => true,
            Some(')') => false,
            _ => return None,
        };
        let inner = &text[1..text.len() - 1];
        let Some((low, high)) = inner.split_once(',') else {
            let version = Version::parse(inner)?;
            if !(low_inclusive && high_inclusive) {
                return None;
            }
            return Some(Self {
                min: Some(Bound { version: version.clone(), inclusive: true }),
                max: Some(Bound { version, inclusive: true }),
                float: None,
            });
        };
        let bound = |text: &str, inclusive: bool| -> Option<Option<Bound>> {
            let text = text.trim();
            if text.is_empty() {
                return Some(None);
            }
            Some(Some(Bound { version: Version::parse(text)?, inclusive }))
        };
        let min = bound(low, low_inclusive)?;
        let max = bound(high, high_inclusive)?;
        if min.is_none() && max.is_none() {
            return None;
        }
        Some(Self { min, max, float: None })
    }

    /// A floating version: `*`, `*-*`, `4.*`, `4.*-*`, `4.2.*`, `4.2.0-*`, `4.2.0-rc.*`.
    fn parse_float(text: &str) -> Option<Self> {
        let (numbers, label) = match text.split_once('-') {
            Some((numbers, label)) => (numbers, Some(label)),
            None => (text, None),
        };
        let mut fixed = Vec::new();
        let mut numbers_float = false;
        for part in numbers.split('.') {
            if part == "*" {
                numbers_float = true;
                break;
            }
            fixed.push(part.parse::<u64>().ok()?);
        }
        let (label_prefix, prerelease) = match label {
            None if numbers_float => (None, false),
            Some("*") if numbers_float => (None, true),
            Some(label) if !numbers_float && label.ends_with('*') && fixed.len() >= 3 => {
                (Some(label[..label.len() - 1].to_owned()), true)
            }
            _ => return None,
        };
        if fixed.len() > 4 {
            return None;
        }
        let mut lowest = [0_u64; 4];
        for (slot, number) in lowest.iter_mut().zip(&fixed) {
            *slot = *number;
        }
        let floor = Version {
            numbers: lowest,
            label: if label_prefix.is_some() { vec!["0".to_owned()] } else { Vec::new() },
        };
        Some(Self {
            min: Some(Bound { version: floor, inclusive: true }),
            max: None,
            float: Some(Float { fixed, label_prefix, prerelease }),
        })
    }

    /// Whether `version` lies between the range's bounds, the float aside.
    fn contains(&self, version: &Version) -> bool {
        let above = self.min.as_ref().is_none_or(|bound| match version.cmp(&bound.version) {
            Ordering::Greater => true,
            Ordering::Equal => bound.inclusive,
            Ordering::Less => false,
        });
        let below = self.max.as_ref().is_none_or(|bound| match version.cmp(&bound.version) {
            Ordering::Less => true,
            Ordering::Equal => bound.inclusive,
            Ordering::Greater => false,
        });
        above && below
    }

    /// Whether a pre-release may be chosen: the range names one at either end, or floats over them.
    fn admits_prerelease(&self) -> bool {
        self.float.as_ref().is_some_and(|float| float.prerelease)
            || self.min.as_ref().is_some_and(|bound| bound.version.is_prerelease() && self.float.is_none())
            || self.max.as_ref().is_some_and(|bound| bound.version.is_prerelease())
    }

    /// Whether the range is exactly one version.
    fn exact(&self) -> Option<&Version> {
        match (&self.min, &self.max) {
            (Some(min), Some(max)) if min.inclusive && max.inclusive && min.version == max.version => {
                Some(&min.version)
            }
            _ => None,
        }
    }

    /// The range's inclusive lower bound, which is the version a simple reference asks for.
    fn floor(&self) -> Option<&Version> {
        self.min.as_ref().filter(|bound| bound.inclusive && self.float.is_none()).map(|bound| &bound.version)
    }

    /// The build NuGet would choose from `available`, and whether it is the version asked for.
    ///
    /// **THE LOWEST BUILD THAT SATISFIES A RANGE, AND THE HIGHEST THAT MATCHES A FLOAT**, as NuGet
    /// chooses: a project that names 4.1.0 gets the nearest build above it, never a newer one that
    /// happens to exist, and a float takes the newest of what it floats over.
    #[must_use]
    pub fn select<'a>(&self, available: &'a [Version]) -> Option<&'a Version> {
        let candidates = available
            .iter()
            .filter(|version| self.contains(version))
            .filter(|version| !version.is_prerelease() || self.admits_prerelease());
        match &self.float {
            Some(float) => {
                let candidates: Vec<&Version> = candidates.collect();
                candidates
                    .iter()
                    .copied()
                    .filter(|version| float.matches(version))
                    .max()
                    .or_else(|| candidates.iter().copied().min())
            }
            None => candidates.min(),
        }
    }
}

impl fmt::Display for Range {
    /// The range as NuGet prints it in a message: `>= 4.2.0`, `= 4.2.0`, `>= 4.0.0 && < 5.0.0`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(exact) = self.exact() {
            return write!(formatter, "= {exact}");
        }
        let mut wrote = false;
        if let Some(min) = &self.min {
            write!(formatter, "{} {}", if min.inclusive { ">=" } else { ">" }, min.version)?;
            wrote = true;
        }
        if let Some(max) = &self.max {
            if wrote {
                formatter.write_str(" && ")?;
            }
            write!(formatter, "{} {}", if max.inclusive { "<=" } else { "<" }, max.version)?;
        }
        Ok(())
    }
}

/// A package a program names: a project's `<PackageReference>` or a file's `#:package` line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PackageReference {
    /// The package id, as written.
    pub id: String,
    /// The version text, as written; `None` when the reference gives none.
    pub version: Option<String>,
    /// The 1-based line that names it, for a message that sends the reader to it.
    pub line: u32,
}

impl PackageReference {
    /// A `#:package` directive's argument, `Id@Version` or `Id` alone, read at `line`.
    #[must_use]
    pub fn from_directive(argument: &str, line: u32) -> Self {
        match argument.split_once('@') {
            Some((id, version)) => Self {
                id: id.trim().to_owned(),
                version: Some(version.trim().to_owned()).filter(|version| !version.is_empty()),
                line,
            },
            None => Self { id: argument.trim().to_owned(), version: None, line },
        }
    }
}

/// The package Lamella builds `id` as, and the versions it builds.
///
/// **A PROVIDED PACKAGE IS BUILT HERE, NEVER DOWNLOADED.** `System.Device.Gpio` keeps Microsoft's id so
/// a project file carries between Lamella, Lamella RHU and a Raspberry Pi unchanged; on Lamella it is
/// Lamella's own build of the library, standing in for Microsoft's. Lamella's own libraries go under
/// their own ids, `Lamella.Hardware` at the version line it shares with Lamella RHU, the others at
/// this release's version.
#[must_use]
pub fn provided(id: &str) -> Option<(String, Vec<Version>)> {
    const NAMED: &[(&str, &str)] = &[("System.Device.Gpio", "4.2.0"), ("Lamella.Hardware", "1.0.0-alpha.1")];
    if let Some((canonical, version)) = NAMED.iter().find(|(named, _)| named.eq_ignore_ascii_case(id)) {
        return Some(((*canonical).to_owned(), Version::parse(version).into_iter().collect()));
    }
    let lamella = id.len() > "Lamella.".len() && id[.."Lamella.".len()].eq_ignore_ascii_case("Lamella.");
    lamella.then(|| (id.to_owned(), Version::parse(env!("CARGO_PKG_VERSION")).into_iter().collect()))
}

/// The ids Lamella provides by name, for a refusal that lists them.
const LISTED: &str = "System.Device.Gpio, Lamella.Hardware, and the Lamella.* libraries in the class library";

/// How a diagnostic reads to MSBuild: a warning or an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Severity {
    /// The build goes on.
    Warning,
    /// The build stops.
    Error,
}

/// One diagnostic about a package reference, with NuGet's code and Lamella's words.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    /// Warning or error.
    pub severity: Severity,
    /// NuGet's code for the same situation, so a search for it finds NuGet's documentation.
    pub code: &'static str,
    /// The sentence.
    pub text: String,
    /// The line of the reference it is about.
    pub line: u32,
}

impl Diagnostic {
    /// The diagnostic in MSBuild's canonical form, `file(line): error CODE: text`, which `dotnet
    /// build`, an editor's problem list and MSBuild's `Exec` task all read as a diagnostic.
    #[must_use]
    pub fn render(&self, file: &str) -> String {
        let severity = match self.severity {
            Severity::Warning => "warning",
            Severity::Error => "error",
        };
        if self.line == 0 {
            return format!("{file} : {severity} {}: {}", self.code, self.text);
        }
        format!("{file}({}): {severity} {}: {}", self.line, self.code, self.text)
    }
}

/// The build a reference resolved to.
#[derive(Clone, Debug)]
pub struct Selected {
    /// The package id, as Lamella spells it.
    pub id: String,
    /// The version chosen.
    pub version: Version,
    /// NU1603 when the version chosen is not the one asked for, NU1604 when the reference names no
    /// lowest version it accepts.
    pub warning: Option<Diagnostic>,
}

/// The build `reference` gets, or why it gets none.
///
/// # Errors
/// An id Lamella does not provide (NU1101), a reference with no version (NU1015), a version that is
/// not one (NU1105), a version no build satisfies (NU1102), and a stable version only a pre-release
/// satisfies (NU1103).
pub fn select(reference: &PackageReference) -> Result<Selected, Diagnostic> {
    let error = |code: &'static str, text: String| Diagnostic {
        severity: Severity::Error,
        code,
        text,
        line: reference.line,
    };
    let id = &reference.id;
    let Some((canonical, available)) = provided(id) else {
        return Err(error("NU1101", not_provided(id)));
    };
    let builds = available.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ");
    let Some(text) = &reference.version else {
        return Err(error(
            "NU1015",
            format!("the reference to {canonical} gives no version. Lamella builds {builds}: name one, as Version=\"{builds}\"."),
        ));
    };
    let Some(range) = Range::parse(text) else {
        return Err(error(
            "NU1105",
            format!("{text:?} is not a version or a version range, so no build of {canonical} can be chosen. Lamella builds {builds}."),
        ));
    };
    let Some(version) = range.select(&available).cloned() else {
        let prerelease = available.iter().filter(|one| one.is_prerelease() && range.contains(one)).min();
        if let Some(prerelease) = prerelease {
            return Err(error(
                "NU1103",
                format!(
                    "Lamella builds {canonical} ({range}) only as a pre-release, {prerelease}, and a reference takes a pre-release only when it names one, as Version=\"{prerelease}\"."
                ),
            ));
        }
        let text = match (range.exact(), range.min.as_ref().filter(|min| min.inclusive), &range.max) {
            (Some(exact), _, _) => {
                format!("the project asks for {canonical} {exact} exactly, and Lamella builds only {builds}.")
            }
            (None, Some(min), None) => {
                format!("Lamella builds no {canonical} at or above {}. It builds {builds}.", min.version)
            }
            _ => format!("Lamella builds no {canonical} in {range}. It builds {builds}."),
        };
        return Err(error("NU1102", text));
    };
    let unbounded = range.float.is_none() && !range.min.as_ref().is_some_and(|min| min.inclusive);
    let warning = if unbounded {
        Some(Diagnostic {
            severity: Severity::Warning,
            code: "NU1604",
            text: format!(
                "the reference to {canonical} ({range}) names no lowest version it accepts, so the build it gets can change when Lamella builds another. It gets {version}; name it, as Version=\"{version}\"."
            ),
            line: reference.line,
        })
    } else {
        range.floor().filter(|floor| **floor != version).map(|floor| Diagnostic {
            severity: Severity::Warning,
            code: "NU1603",
            text: format!(
                "Lamella builds no {canonical} {floor}, so the project gets {version}, the lowest version Lamella builds above it."
            ),
            line: reference.line,
        })
    };
    Ok(Selected { id: canonical, version, warning })
}

/// Why `id` is not provided, with what to do instead.
fn not_provided(id: &str) -> String {
    let known = |name: &str| id.eq_ignore_ascii_case(name);
    if known("System.Net.NetworkInformation") {
        return format!(
            "{id} is part of Lamella's framework, beside the corlib, so a program uses it without naming it. Remove the reference."
        );
    }
    let assembly = ["System.IO.Ports", "System.Device.Model", "System.Device.Analog"].into_iter().find(|name| known(name));
    if let Some(assembly) = assembly {
        return format!(
            "Lamella provides no package {assembly} yet. Its assembly is in the class library: name it with\n    <Reference Include=\"{assembly}\"><HintPath>path\\to\\{assembly}.dll</HintPath></Reference>"
        );
    }
    format!("Lamella provides no package {id}. A program built for Lamella names the packages Lamella builds: {LISTED}.")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap_or_else(|| panic!("{text} is a version"))
    }

    fn pick(range: &str, available: &[&str]) -> Option<String> {
        let available: Vec<Version> = available.iter().map(|one| v(one)).collect();
        Range::parse(range).expect("a range").select(&available).map(ToString::to_string)
    }

    #[test]
    fn versions_order_as_semver_with_nuget_spellings() {
        assert_eq!(v("4.2"), v("4.2.0"));
        assert_eq!(v("4.2.0.0"), v("4.2.0"));
        assert_eq!(v("4.2.0+build.7"), v("4.2.0"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0"));
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.2") < v("1.0.0-alpha.10"));
        assert!(v("1.0.0-1") < v("1.0.0-alpha"));
        assert_eq!(v("1.0.0-ALPHA.1"), v("1.0.0-alpha.1"));
        assert_eq!(v("4.2.0.1").to_string(), "4.2.0.1");
        assert!(Version::parse("banana").is_none());
        assert_eq!(v("4"), v("4.0.0"));
        assert!(Version::parse("4..2").is_none());
    }

    #[test]
    fn a_bare_version_takes_the_lowest_build_at_or_above_it() {
        assert_eq!(pick("4.2.0", &["4.2.0"]).as_deref(), Some("4.2.0"));
        assert_eq!(pick("4.1.0", &["4.2.0"]).as_deref(), Some("4.2.0"));
        assert_eq!(pick("4.1.0", &["4.3.0", "4.2.0"]).as_deref(), Some("4.2.0"));
        assert_eq!(pick("4.3.0", &["4.2.0", "4.3.0"]).as_deref(), Some("4.3.0"));
        assert_eq!(pick("5.0.0", &["4.2.0", "4.3.0"]), None);
    }

    #[test]
    fn brackets_and_intervals_are_nugets() {
        assert_eq!(pick("[4.2.0]", &["4.2.0", "4.3.0"]).as_deref(), Some("4.2.0"));
        assert_eq!(pick("[4.1.0]", &["4.2.0"]), None);
        assert_eq!(pick("[4.0.0,5.0.0)", &["4.2.0", "5.0.0"]).as_deref(), Some("4.2.0"));
        assert_eq!(pick("(4.2.0,)", &["4.2.0", "4.3.0"]).as_deref(), Some("4.3.0"));
        assert_eq!(pick("[,4.3.0)", &["4.2.0", "4.3.0"]).as_deref(), Some("4.2.0"));
        assert!(Range::parse("(4.2.0)").is_none());
        assert!(Range::parse("[,]").is_none());
    }

    #[test]
    fn a_float_takes_the_highest_build_it_matches() {
        assert_eq!(pick("4.*", &["4.2.0", "4.3.0"]).as_deref(), Some("4.3.0"));
        assert_eq!(pick("4.2.*", &["4.2.0", "4.3.0"]).as_deref(), Some("4.2.0"));
        assert_eq!(pick("*", &["4.2.0", "5.0.0-beta.1"]).as_deref(), Some("4.2.0"));
        assert_eq!(pick("*-*", &["4.2.0", "5.0.0-beta.1"]).as_deref(), Some("5.0.0-beta.1"));
        assert_eq!(pick("1.0.0-*", &["1.0.0-alpha.1"]).as_deref(), Some("1.0.0-alpha.1"));
        assert_eq!(pick("4.*", &["3.0.0"]), None);
        assert_eq!(pick("4.*", &["5.0.0"]).as_deref(), Some("5.0.0"));
    }

    #[test]
    fn a_pre_release_is_chosen_only_when_the_reference_names_one() {
        assert_eq!(pick("1.0.0", &["1.0.0-alpha.1"]), None);
        assert_eq!(pick("1.0.0-alpha.1", &["1.0.0-alpha.1"]).as_deref(), Some("1.0.0-alpha.1"));
        assert_eq!(pick("[1.0.0-alpha.1]", &["1.0.0-alpha.1"]).as_deref(), Some("1.0.0-alpha.1"));
        assert_eq!(pick("1.0.0-alpha", &["1.0.0-alpha.1"]).as_deref(), Some("1.0.0-alpha.1"));
        assert_eq!(pick("0.9.0", &["0.9.0", "1.0.0-alpha.1"]).as_deref(), Some("0.9.0"));
        assert_eq!(pick("0.9.5", &["0.9.0", "1.0.0-alpha.1"]), None);
        assert_eq!(pick("1.*", &["1.0.0-alpha.1"]), None);
    }

    fn reference(id: &str, version: Option<&str>) -> PackageReference {
        PackageReference { id: id.to_owned(), version: version.map(str::to_owned), line: 9 }
    }

    #[test]
    fn the_messages_are_nugets_codes_in_lamellas_words() {
        let older = select(&reference("System.Device.Gpio", Some("4.1.0"))).expect("selected");
        assert_eq!(older.version.to_string(), "4.2.0");
        assert_eq!(
            older.warning.expect("NU1603").render("App.csproj"),
            "App.csproj(9): warning NU1603: Lamella builds no System.Device.Gpio 4.1.0, so the project \
             gets 4.2.0, the lowest version Lamella builds above it."
        );
        let exact = select(&reference("system.device.gpio", Some("4.2.0"))).expect("selected");
        assert_eq!((exact.id.as_str(), exact.warning), ("System.Device.Gpio", None));
        let newer = select(&reference("System.Device.Gpio", Some("5.0.0"))).expect_err("NU1102");
        assert_eq!(
            newer.render("App.csproj"),
            "App.csproj(9): error NU1102: Lamella builds no System.Device.Gpio at or above 5.0.0. It builds 4.2.0."
        );
        let pinned = select(&reference("System.Device.Gpio", Some("[4.1.0]"))).expect_err("NU1102");
        assert_eq!(
            pinned.text,
            "the project asks for System.Device.Gpio 4.1.0 exactly, and Lamella builds only 4.2.0."
        );
        assert_eq!(select(&reference("System.Device.Gpio", None)).expect_err("NU1015").code, "NU1015");
        assert_eq!(select(&reference("Newtonsoft.Json", Some("13.0.3"))).expect_err("NU1101").code, "NU1101");
        assert_eq!(select(&reference("System.Device.Gpio", Some("banana"))).expect_err("NU1105").code, "NU1105");
        let open = select(&reference("System.Device.Gpio", Some("(4.0.0,)"))).expect("selected");
        assert_eq!(open.warning.map(|warning| warning.code), Some("NU1604"));
        let floated = select(&reference("System.Device.Gpio", Some("4.*"))).expect("selected");
        assert_eq!(floated.warning, None, "a float never warns that it moved");
    }

    #[test]
    fn the_shared_hardware_library_is_on_its_pre_release_line() {
        let hardware = select(&reference("Lamella.Hardware", Some("1.0.0-alpha.1"))).expect("selected");
        assert_eq!(hardware.version.to_string(), "1.0.0-alpha.1");
        let stable = select(&reference("Lamella.Hardware", Some("1.0.0"))).expect_err("NU1102");
        assert_eq!(stable.code, "NU1102");
        let older = select(&reference("Lamella.Hardware", Some("0.9.0"))).expect_err("NU1103");
        assert_eq!(older.code, "NU1103");
        let floated = select(&reference("Lamella.Hardware", Some("1.*"))).expect_err("NU1102");
        assert_eq!(floated.code, "NU1102", "1.0.0-alpha.1 sorts below 1.0.0, the float's floor");
        let own = select(&reference("Lamella.Net.WiFi", Some(env!("CARGO_PKG_VERSION")))).expect("selected");
        assert_eq!(own.warning, None);
    }

    #[test]
    fn a_directive_names_its_package_as_dotnet_does() {
        assert_eq!(
            PackageReference::from_directive("System.Device.Gpio@4.2.0", 1),
            PackageReference { id: "System.Device.Gpio".into(), version: Some("4.2.0".into()), line: 1 }
        );
        assert_eq!(PackageReference::from_directive("System.Device.Gpio", 2).version, None);
    }
}
