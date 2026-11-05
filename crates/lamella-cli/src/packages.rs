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
    /// `text` as a version, or `None` when it is not one, as NuGet reads them: each number fits 32 signed
    /// bits, a label's identifiers are letters, digits and hyphens, and one of digits alone has no leading
    /// zero. Build metadata after one `+` is read for its form and then ignored.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let identifiers_of = |dotted: &str| -> Option<Vec<String>> {
            let identifiers: Vec<String> = dotted.split('.').map(str::to_owned).collect();
            let valid = |one: &String| !one.is_empty() && one.bytes().all(|byte| byte.is_ascii_alphanumeric() || byte == b'-');
            identifiers.iter().all(valid).then_some(identifiers)
        };
        let text = match text.split_once('+') {
            Some((version, metadata)) => {
                identifiers_of(metadata)?;
                version
            }
            None => text,
        };
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
            let part = part.trim();
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            *slot = u64::from(part.parse::<i32>().ok()?.unsigned_abs());
        }
        let label = match label {
            None => Vec::new(),
            Some(label) => {
                let identifiers = identifiers_of(label)?;
                let leading_zero = |one: &String| is_numeric(one) && one.len() > 1 && one.starts_with('0');
                if identifiers.iter().any(leading_zero) {
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

/// Whether a label's identifier is a number, which SemVer compares as one, whatever its length.
fn is_numeric(identifier: &str) -> bool {
    identifier.bytes().all(|byte| byte.is_ascii_digit())
}

/// Two pre-release labels in SemVer 2.0's order.
fn compare_labels(left: &[String], right: &[String]) -> Ordering {
    for (one, two) in left.iter().zip(right) {
        let order = match (is_numeric(one), is_numeric(two)) {
            (true, true) => one.len().cmp(&two.len()).then_with(|| one.cmp(two)),
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => one.to_ascii_lowercase().cmp(&two.to_ascii_lowercase()),
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
    /// The numbers of the floor, whose leading ones a version shares to match.
    numbers: [u64; 4],
    /// How many leading numbers it shares: none for `*`, one for `4.*`, all four for `4.2.0-*`.
    fixed: usize,
    /// For a float over pre-releases, the start of the labels it floats over: `rc.` for `4.2.0-rc.*`, and
    /// empty for `4.2.0-*` and `4.*-*`. `None` for a float over stable versions alone.
    label_prefix: Option<String>,
}

impl Float {
    /// Whether `version` is one this float floats over.
    fn matches(&self, version: &Version) -> bool {
        if version.numbers[..self.fixed] != self.numbers[..self.fixed] {
            return false;
        }
        match &self.label_prefix {
            Some(prefix) => {
                !version.is_prerelease()
                    || version.label.join(".").to_ascii_lowercase().starts_with(&prefix.to_ascii_lowercase())
            }
            None => !version.is_prerelease(),
        }
    }
}

/// `text`, the lower end of a range or a bare version, as its floor and the float it states, if any.
///
/// **THE FORMS AND THE FLOORS ARE NuGet's,** from `FloatRange.TryParse`, and a test holds them against what
/// the .NET SDK's own `NuGet.Versioning` answered. A float's floor is its fixed numbers with zeros after
/// them, and for a float over pre-releases the label it names, a label ending in a dot or none at all
/// taking a `0`, the lowest identifier there is. So `1.0.0-beta.*` is above `1.0.0-alpha.2`, and `1.*-*` is
/// below it.
fn floor_and_float(text: &str) -> Option<(Version, Option<Float>)> {
    let lowest_label = |prefix: &str| {
        if prefix.is_empty() || prefix.ends_with('.') { format!("{prefix}0") } else { prefix.to_owned() }
    };
    let floating = |floor: Version, fixed: usize, label_prefix: Option<&str>| {
        let float = Float { numbers: floor.numbers, fixed, label_prefix: label_prefix.map(str::to_owned) };
        Some((floor, Some(float)))
    };
    if !text.contains('*') {
        return Some((Version::parse(text)?, None));
    }
    if text.contains('+') {
        return None;
    }
    if text == "*" {
        return floating(Version::parse("0.0")?, 0, None);
    }
    if text == "*-*" {
        return floating(Version::parse("0.0.0-0")?, 0, Some(""));
    }
    let (first, last) = (text.find('*')?, text.rfind('*')?);
    if last != text.len() - 1 {
        return None;
    }
    if first != last {
        let dash = text.find('-')?;
        if first + 1 != dash {
            return None;
        }
        let stable = format!("{}0", &text[..first]);
        let prefix = &text[dash + 1..last];
        let floor = Version::parse(&format!("{stable}-{}", lowest_label(prefix)))?;
        return floating(floor, stable.split('.').count() - 1, Some(prefix));
    }
    match text.find('-') {
        Some(dash) => {
            let prefix = &text[dash + 1..last];
            let floor = Version::parse(&format!("{}-{}", &text[..dash], lowest_label(prefix)))?;
            floating(floor, 4, Some(prefix))
        }
        None => {
            let stable = format!("{}0", &text[..last]);
            let floor = Version::parse(&stable)?;
            match stable.split('.').count() {
                1 => Some((floor, None)),
                parts => floating(floor, parts - 1, None),
            }
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
    /// `text` as a range, or `None` when it is not one: what NuGet reads as a range, and nothing else. Its
    /// lower end may float, as in `[1.*,2.0)`, and its upper end may not; its upper end is not below its lower
    /// end, and two equal ends are both inclusive or both exclusive.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        if !text.starts_with('[') && !text.starts_with('(') {
            let (version, float) = floor_and_float(text)?;
            return Some(Self { min: Some(Bound { version, inclusive: true }), max: None, float });
        }
        let low_inclusive = text.starts_with('[');
        let high_inclusive = match text.chars().last() {
            Some(']') => true,
            Some(')') => false,
            _ => return None,
        };
        let inner = &text[1..text.len() - 1];
        let ends: Vec<&str> = inner.split(',').map(str::trim).collect();
        let (low, high) = match ends[..] {
            [only] => {
                if !(low_inclusive && high_inclusive) || only.contains('*') {
                    return None;
                }
                let version = Version::parse(only)?;
                return Some(Self {
                    min: Some(Bound { version: version.clone(), inclusive: true }),
                    max: Some(Bound { version, inclusive: true }),
                    float: None,
                });
            }
            [low, high] => (low, high),
            _ => return None,
        };
        if (low.is_empty() && high.is_empty()) || high.contains('*') {
            return None;
        }
        let (min, float) = if low.is_empty() {
            (None, None)
        } else {
            let (version, float) = floor_and_float(low)?;
            (Some(Bound { version, inclusive: low_inclusive }), float)
        };
        let max = if high.is_empty() { None } else { Some(Bound { version: Version::parse(high)?, inclusive: high_inclusive }) };
        if let (Some(min), Some(max)) = (&min, &max) {
            match min.version.cmp(&max.version) {
                Ordering::Greater => return None,
                Ordering::Equal if min.inclusive != max.inclusive => return None,
                _ => {}
            }
        }
        Some(Self { min, max, float })
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
        [&self.min, &self.max].into_iter().flatten().any(|bound| bound.version.is_prerelease())
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

    /// What the reference asks for, as a message names it: the version where it names one, which an
    /// exact range or a bare version does, and otherwise the range in parentheses.
    fn asked(&self) -> String {
        match (self.exact(), self.floor(), &self.max) {
            (Some(exact), _, _) => exact.to_string(),
            (None, Some(floor), None) => floor.to_string(),
            _ => format!("({self})"),
        }
    }

    /// The lower end as a message names it. **A FLOAT'S FLOOR IS NOT WHAT THE PROJECT WROTE,** and the `0` that
    /// makes it the lowest label is nothing a reader asked for: `4.3.0-*` reads as 4.3.0, and `1.0.0-beta.*` as
    /// 1.0.0-beta, as NuGet's restore reads those two.
    fn shown_min(&self, bound: &Bound) -> Version {
        let Some(prefix) = self.float.as_ref().and_then(|float| float.label_prefix.as_deref()) else {
            return bound.version.clone();
        };
        let prefix = prefix.trim_end_matches('.');
        let label = if prefix.is_empty() { Vec::new() } else { prefix.split('.').map(str::to_owned).collect() };
        Version { numbers: bound.version.numbers, label }
    }

    /// Whether every version the range admits lies above `version`.
    fn lies_above(&self, version: &Version) -> bool {
        self.min.as_ref().is_some_and(|bound| bound.version > *version || (bound.version == *version && !bound.inclusive))
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
            write!(formatter, "{} {}", if min.inclusive { ">=" } else { ">" }, self.shown_min(min))?;
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

/// A package Lamella builds in place of NuGet's: its id as Lamella spells it, and the version it
/// builds, the one version any reference to it can get. [`select`] says which references it serves.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provided {
    /// The id as Lamella spells it.
    pub id: String,
    /// The version Lamella builds, which every reference it serves gets.
    pub version: Version,
}

/// The package Lamella builds `id` as.
///
/// **A PROVIDED PACKAGE IS BUILT HERE, NEVER DOWNLOADED.** `System.Device.Gpio` keeps Microsoft's id so
/// a project file carries between Lamella, Lamella RHU and a Raspberry Pi unchanged; on Lamella it is
/// Lamella's own build of the library, standing in for Microsoft's. Lamella's own libraries go under
/// their own ids, `Lamella.Hardware` on the version line it shares with Lamella RHU, the others at
/// this release's version.
#[must_use]
pub fn provided(id: &str) -> Option<Provided> {
    const NAMED: &[(&str, &str)] = &[("System.Device.Gpio", "4.2.0"), ("Lamella.Hardware", "1.0.0-alpha.2")];
    if let Some((canonical, version)) = NAMED.iter().find(|(named, _)| named.eq_ignore_ascii_case(id)) {
        return Some(Provided { id: (*canonical).to_owned(), version: Version::parse(version)? });
    }
    let lamella = id.len() > "Lamella.".len() && id.as_bytes()[.."Lamella.".len()].eq_ignore_ascii_case(b"Lamella.");
    let version = Version::parse(env!("CARGO_PKG_VERSION"))?;
    lamella.then(|| Provided { id: id.to_owned(), version })
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
/// **A REFERENCE AT OR BELOW THE VERSION BUILT IS SERVED BY THAT BUILD**, as NuGet serves the
/// nearest version above the one asked for, with NU1603 where the two differ. A reference above it is
/// refused with what to do (reference that version or lower, or use a `lamella` that provides the
/// newer one), as the .NET SDK refuses a target framework newer than it supports.
///
/// **NuGet's OWN RULE DECIDES A PRE-RELEASE:** a reference takes one only when it names one or floats over
/// them. So where the version built is a pre-release, `1.0.0-alpha.1` and `1.*-*` are served and `0.9.0`,
/// `*` and `[0.9.0,)` are NU1103, though each lies below it. A float takes the version built when that is
/// the highest it floats over, or above its floor, and a float whose floor lies above is refused as any
/// other reference above it is: `1.0.0-beta.*` above `1.0.0-alpha.2`.
///
/// # Errors
/// An id Lamella does not provide (NU1101), a reference with no version (NU1015), a version that is
/// not one (NU1105), a version above the one built or one no build satisfies (NU1102), and a reference
/// naming no pre-release that only a pre-release satisfies (NU1103).
pub fn select(reference: &PackageReference) -> Result<Selected, Diagnostic> {
    let error = |code: &'static str, text: String| Diagnostic {
        severity: Severity::Error,
        code,
        text,
        line: reference.line,
    };
    let id = &reference.id;
    let Some(Provided { id: canonical, version }) = provided(id) else {
        return Err(error("NU1101", not_provided(id)));
    };
    let Some(text) = &reference.version else {
        return Err(error(
            "NU1015",
            format!("the reference to {canonical} gives no version. Lamella builds {version}: name one, as Version=\"{version}\"."),
        ));
    };
    let Some(range) = Range::parse(text) else {
        return Err(error(
            "NU1105",
            format!("{text:?} is not a version or a version range, so no build of {canonical} can be chosen. Lamella builds {version}."),
        ));
    };
    if range.select(std::slice::from_ref(&version)).is_none() {
        if range.lies_above(&version) {
            let asked = range.asked();
            let or_lower = if range.exact().is_some() { "" } else { " or lower" };
            return Err(error(
                "NU1102",
                format!(
                    "the current lamella does not provide {canonical} {asked}. Either reference {canonical} {version}{or_lower}, or use a version of lamella that provides {canonical} {asked}."
                ),
            ));
        }
        if version.is_prerelease() && range.contains(&version) {
            return Err(error(
                "NU1103",
                format!(
                    "Lamella builds {canonical} ({range}) only as a pre-release, {version}, and a reference takes a pre-release only when it names one, as Version=\"{version}\"."
                ),
            ));
        }
        let text = match range.exact() {
            Some(exact) => format!("the project asks for {canonical} {exact} exactly, and Lamella builds only {version}."),
            None => format!("Lamella builds no {canonical} in {range}. It builds {version}."),
        };
        return Err(error("NU1102", text));
    }
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
                "the project depends on {canonical} ({range}) but {canonical} {floor} was not found. {canonical} {version} was resolved instead."
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

    /// **EVERY FORM, READ AS NuGet READS IT, AGAINST WHAT THE .NET SDK's `NuGet.Versioning` ANSWERED** (11.0.100-rc.2,
    /// `VersionRange.TryParse` with floats allowed): its floor, or that it is no range at all.
    #[test]
    fn the_forms_and_floors_are_what_nugets_own_library_answers() {
        for (form, floor) in [
            ("*", "0.0.0"),
            ("*-*", "0.0.0-0"),
            ("4.*", "4.0.0"),
            ("4.*-*", "4.0.0-0"),
            ("4.2.*", "4.2.0"),
            ("4.2.*-*", "4.2.0-0"),
            ("4.2.0.*", "4.2.0"),
            ("4.2.0-*", "4.2.0-0"),
            ("4.2.0-rc.*", "4.2.0-rc.0"),
            ("1.*-rc.*", "1.0.0-rc.0"),
            ("1.0-*", "1.0.0-0"),
            ("1-*", "1.0.0-0"),
            ("1.0.0-*", "1.0.0-0"),
            ("1.0.0-beta.*", "1.0.0-beta.0"),
            ("1.0.0-alpha.*", "1.0.0-alpha.0"),
            ("1.*-*", "1.0.0-0"),
            ("1.0.*-*", "1.0.0-0"),
            ("1.0.*-rc*", "1.0.0-rc"),
            ("1.0.0.*-*", "1.0.0-0"),
            ("4.2.0-rc*", "4.2.0-rc"),
            ("4.2.0-rc.1*", "4.2.0-rc.1"),
            ("4.2.0-rc-*", "4.2.0-rc-"),
            ("*-rc.*", "0.0.0-rc.0"),
            ("*-rc*", "0.0.0-rc"),
            ("1.2.3.4-*", "1.2.3.4-0"),
            ("1.*-rc.1*", "1.0.0-rc.1"),
            ("4*", "40.0.0"),
            ("1.2*", "1.20.0"),
            ("[1.*,2.0)", "1.0.0"),
            ("[1.*,2.0]", "1.0.0"),
            ("(1.*,2.0)", "1.0.0"),
            ("[1.0.0-*,2.0)", "1.0.0-0"),
            ("[1.0.0-rc.*,2.0)", "1.0.0-rc.0"),
            ("[*,2.0)", "0.0.0"),
            ("[*,]", "0.0.0"),
            ("[1.*,)", "1.0.0"),
            ("[1.0.*,1.0]", "1.0.0"),
            ("(1.0,)", "1.0.0"),
            ("[1.0,)", "1.0.0"),
            ("(1.0,1.0)", "1.0.0"),
            ("[1.0]", "1.0.0"),
            ("[1.0,1.0]", "1.0.0"),
            ("[1.0, 2.0]", "1.0.0"),
            ("[1.0-rc,1.0]", "1.0.0-rc"),
            (" 1.0 ", "1.0.0"),
            ("01.2", "1.2.0"),
            ("1 .2", "1.2.0"),
            ("1.2.3+meta", "1.2.3"),
            ("1.2.3+a.b", "1.2.3"),
            ("1.2.3-a+b", "1.2.3-a"),
            ("1.2.3-0", "1.2.3-0"),
            ("1.2.3-0a", "1.2.3-0a"),
            ("1.2.3--", "1.2.3--"),
            ("1.2.3-a-b", "1.2.3-a-b"),
            ("1.2.3-99999999999999999999", "1.2.3-99999999999999999999"),
            ("2147483647.0", "2147483647.0.0"),
        ] {
            let range = Range::parse(form).unwrap_or_else(|| panic!("{form} is a range"));
            let min = range.min.as_ref().unwrap_or_else(|| panic!("{form} has a floor"));
            assert_eq!(min.version.to_string(), floor, "{form}");
        }
        for form in [
            "", "banana", "1.*.0", "*.1", "**", "1.0.0-*.1", "1.0.0-a*b", "4.2.0-*-*", "1.*-*-*", "1.*.*", "1.2.3.4.*",
            "1.2.3.4.5-*", "1.2.3.4.5.*", "1.0.0+a-*", "1.0.0-*+a", "1.*+a", "1.*-rc.*-*", "[1.*]", "(1.*)", "[*]",
            "[,1.0-*]", "[1.*,2.*]", "[1.*,1.*]", "[1.5.*,1.0]", "[,*]", "[1.0,*]", "[2.0,1.0]", "[2.0,1.0)", "[1.0,1.0)",
            "(1.0,1.0]", "(1.0)", "(,)", "[,)", "(,]", "[,]", "[1.0,2.0,3.0]", "[1.0", "1.0]", "[1.0,", "(1.0,2.0", "[]",
            "()", "[,", ",", "1.0,2.0", "[1.0,1.0-rc]", "1.2.3.4.5", "1.2.3-01", "1.2.3-00", "1.2.3-a.01", "1.2.3-a..b",
            "1.2.3+", "1.2.3+a_b", "1.2.3+a+b", "1.2.3-", "-1", "1.-2", "1.2.3-a_b", "1.2.3-\u{e9}", "v1.0", "1.0.0-alpha.2.",
            ".1", "1.", "1..2", "2147483648.0", "99999999999",
        ] {
            assert!(Range::parse(form).is_none(), "{form:?} is no range to NuGet");
        }
    }

    /// **WHAT A RANGE PRINTS IS WHAT NuGet's RESTORE PRINTED FOR IT,** with no internal floor in it, but for
    /// two forms where NuGet's own print shows one and this does not.
    #[test]
    fn a_range_prints_as_nugets_restore_prints_it() {
        for (form, printed) in [
            ("5.*", ">= 5.0.0"),
            ("4.3.0-*", ">= 4.3.0"),
            ("1.0.0-beta.*", ">= 1.0.0-beta"),
            ("1.0.*-rc*", ">= 1.0.0-rc"),
            ("4.2.0-rc*", ">= 4.2.0-rc"),
            ("1.0.0-rc.1*", ">= 1.0.0-rc.1"),
            ("[1.*,2.0)", ">= 1.0.0 && < 2.0.0"),
            ("[4.3,5.0)", ">= 4.3.0 && < 5.0.0"),
            ("(4.0.0,)", "> 4.0.0"),
            ("[,5.0)", "< 5.0.0"),
            ("[4.1.0]", "= 4.1.0"),
            ("1.*-rc.*", ">= 1.0.0-rc"),
            ("5.*-*", ">= 5.0.0"),
        ] {
            assert_eq!(Range::parse(form).expect("a range").to_string(), printed, "{form}");
        }
    }

    /// **WHICH BUILD A REFERENCE GETS, AGAINST `VersionRange.FindBestMatch`** over the one version Lamella builds:
    /// a float whose floor lies above it gets none, and one whose floor lies below it floats over it.
    #[test]
    fn the_one_build_is_chosen_as_nugets_find_best_match_chooses() {
        for (built, form, chosen) in [
            ("1.0.0-alpha.2", "1.0.0-beta.*", false),
            ("1.0.0-alpha.2", "1.0.0-rc.*", false),
            ("1.0.0-alpha.2", "1.0.0-alpha.*", true),
            ("1.0.0-alpha.2", "1.0.0-*", true),
            ("1.0.0-alpha.2", "1.*-*", true),
            ("1.0.0-alpha.2", "1.0.*-*", true),
            ("1.0.0-alpha.2", "1.*", false),
            ("1.0.0-alpha.2", "*", false),
            ("1.0.0-alpha.2", "*-*", true),
            ("1.0.0-alpha.2", "1.*-rc.*", false),
            ("1.0.0-alpha.2", "1.*-alpha.*", true),
            ("1.0.0-alpha.2", "1.0-*", true),
            ("1.0.0-alpha.2", "1-*", true),
            ("1.0.0-alpha.2", "[1.*,2.0)", false),
            ("1.0.0-alpha.2", "0.9.0", false),
            ("1.0.0-alpha.2", "1.0.0", false),
            ("1.0.0-alpha.2", "0.*", false),
            ("1.0.0-alpha.2", "[0.9.0,)", false),
            ("1.0.0-alpha.2", "[1.0.0-alpha.1]", false),
            ("1.0.0-alpha.2", "1.0.0-alpha.1", true),
            ("1.0.0-alpha.2", "1.0.0-alpha.3", false),
            ("1.0.0-alpha.2", "4.*", false),
            ("4.2.0", "1.0.0-beta.*", true),
            ("4.2.0", "1.*-*", true),
            ("4.2.0", "1.*", true),
            ("4.2.0", "*", true),
            ("4.2.0", "*-*", true),
            ("4.2.0", "1.*-rc.*", true),
            ("4.2.0", "[1.*,2.0)", false),
            ("4.2.0", "0.9.0", true),
            ("4.2.0", "1.0.0-alpha.1", true),
            ("4.2.0", "[1.0.0-alpha.1]", false),
            ("4.2.0", "4.*", true),
            ("4.2.0", "4.1.0", true),
            ("4.2.0", "4.2.*", true),
            ("4.2.0", "4.1.*", true),
            ("4.2.0", "[4.0,5.0)", true),
            ("4.2.0", "[4.3,)", false),
            ("4.2.0", "5.*", false),
            ("4.2.0", "3.*", true),
        ] {
            assert_eq!(pick(form, &[built]).is_some(), chosen, "{form} against {built}");
        }
    }

    /// **M8's TWO ROWS.** A label float above the version built is refused, in NuGet's words for it, and a float
    /// over pre-releases of the numbers built is served; each was the other way about.
    #[test]
    fn a_label_float_above_the_version_built_is_refused_and_one_over_it_is_served() {
        for above in ["1.0.0-beta.*", "1.0.0-rc.*", "1.*-rc.*"] {
            let refused = select(&reference("Lamella.Hardware", Some(above))).expect_err("above the version built");
            assert_eq!(refused.code, "NU1102", "{above}");
            assert!(refused.text.contains("the current lamella does not provide"), "{above}: {}", refused.text);
            assert!(!refused.text.contains(".0)"), "no internal floor: {}", refused.text);
        }
        for over in ["1.*-*", "1.0.*-*", "1.0-*", "1-*", "*-*", "1.*-alpha.*", "1.0.0-alpha.*"] {
            let served = select(&reference("Lamella.Hardware", Some(over))).expect("served");
            assert_eq!((served.version.to_string(), served.warning), ("1.0.0-alpha.2".to_owned(), None), "{over}");
        }
    }

    /// **WHAT NuGet ACCEPTS IS NOT "NOT A VERSION", AND WHAT IT REFUSES IS.** `[2.0,1.0]` names no versions at
    /// all, and it is NuGet's refusal of the text, not a build that is missing.
    #[test]
    fn valid_forms_are_read_and_an_inverted_range_is_not_a_version() {
        for valid in ["1.*-rc.*", "1.0-*", "[1.*,2.0)"] {
            assert_ne!(select(&reference("System.Device.Gpio", Some(valid))).map(|one| one.id).map_err(|error| error.code), Err("NU1105"), "{valid}");
        }
        assert_eq!(select(&reference("System.Device.Gpio", Some("[2.0,1.0]"))).expect_err("invalid").code, "NU1105");
    }

    /// **AN ID IS THE PROJECT'S TEXT,** and one with a character across its eighth byte is an id Lamella does not
    /// provide, and no panic.
    #[test]
    fn a_non_ascii_id_is_not_provided_and_does_not_panic() {
        assert!(provided("Lamella\u{e9}.Foo").is_none());
        assert!(provided("\u{e9}\u{e9}\u{e9}\u{e9}Foo").is_none());
        assert_eq!(select(&reference("Lamella\u{e9}.Foo", Some("1.0"))).expect_err("NU1101").code, "NU1101");
        assert!(provided("LAMELLA.Net.WiFi").is_some());
        assert!(provided("Lamella.").is_none());
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
            "App.csproj(9): warning NU1603: the project depends on System.Device.Gpio (>= 4.1.0) but \
             System.Device.Gpio 4.1.0 was not found. System.Device.Gpio 4.2.0 was resolved instead."
        );
        let exact = select(&reference("system.device.gpio", Some("4.2.0"))).expect("selected");
        assert_eq!((exact.id.as_str(), exact.warning), ("System.Device.Gpio", None));
        let newer = select(&reference("System.Device.Gpio", Some("5.0.0"))).expect_err("NU1102");
        assert_eq!(
            newer.render("App.csproj"),
            "App.csproj(9): error NU1102: the current lamella does not provide System.Device.Gpio 5.0.0. Either \
             reference System.Device.Gpio 4.2.0 or lower, or use a version of lamella that provides \
             System.Device.Gpio 5.0.0."
        );
        let unnamed = select(&reference("System.Device.Gpio", None)).expect_err("NU1015");
        assert_eq!(
            unnamed.text,
            "the reference to System.Device.Gpio gives no version. Lamella builds 4.2.0: name one, as Version=\"4.2.0\"."
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
    fn the_shared_hardware_library_is_served_up_to_the_version_built() {
        let built = select(&reference("Lamella.Hardware", Some("1.0.0-alpha.2"))).expect("selected");
        assert_eq!((built.version.to_string(), built.warning), ("1.0.0-alpha.2".to_owned(), None));
        let older = select(&reference("Lamella.Hardware", Some("1.0.0-alpha.1"))).expect("selected");
        assert_eq!(older.version.to_string(), "1.0.0-alpha.2");
        assert_eq!(
            older.warning.expect("NU1603").render("App.csproj"),
            "App.csproj(9): warning NU1603: the project depends on Lamella.Hardware (>= 1.0.0-alpha.1) but \
             Lamella.Hardware 1.0.0-alpha.1 was not found. Lamella.Hardware 1.0.0-alpha.2 was resolved instead."
        );
        for floated in ["1.0.0-*", "1.0.0-alpha.*"] {
            let hardware = select(&reference("Lamella.Hardware", Some(floated))).expect("selected");
            assert_eq!((hardware.version.to_string(), hardware.warning), ("1.0.0-alpha.2".to_owned(), None), "{floated}");
        }
        let pinned = select(&reference("Lamella.Hardware", Some("[1.0.0-alpha.1]"))).expect_err("NU1102");
        assert_eq!(pinned.text, "the project asks for Lamella.Hardware 1.0.0-alpha.1 exactly, and Lamella builds only 1.0.0-alpha.2.");
        let unnamed = select(&reference("Lamella.Hardware", None)).expect_err("NU1015");
        assert_eq!(
            unnamed.text,
            "the reference to Lamella.Hardware gives no version. Lamella builds 1.0.0-alpha.2: name one, as \
             Version=\"1.0.0-alpha.2\"."
        );
        for (above, asked, or_lower) in [
            ("1.0.0", "1.0.0", " or lower"),
            ("1.*", "(>= 1.0.0)", " or lower"),
            ("1.0.0-beta.*", "(>= 1.0.0-beta)", " or lower"),
            ("[1.0.0-alpha.3]", "1.0.0-alpha.3", ""),
        ] {
            let refused = select(&reference("Lamella.Hardware", Some(above))).expect_err("above the provided version");
            assert_eq!(
                refused.render("App.csproj"),
                format!(
                    "App.csproj(9): error NU1102: the current lamella does not provide Lamella.Hardware {asked}. \
                     Either reference Lamella.Hardware 1.0.0-alpha.2{or_lower}, or use a version of lamella that \
                     provides Lamella.Hardware {asked}."
                ),
                "{above}"
            );
        }
        for below in ["0.9.0", "*", "0.*", "[0.9.0,)"] {
            assert_eq!(select(&reference("Lamella.Hardware", Some(below))).expect_err("NU1103").code, "NU1103", "{below}");
        }
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
