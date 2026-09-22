//! Reading the part of an SDK-style `.csproj` that a Lamella build needs: which sources compile,
//! which assemblies they bind against, and what the program is called.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// The framework identifier a Lamella project targets.
///
/// **THE EXPANSION OF [`MONIKER`], AND ACCEPTED BESIDE IT.** MSBuild parses this pair today with no
/// help from us -- it reads as `Lamella,Version=v1.0`, and `dotnet build` then stops only at
/// "targeting pack not installed" rather than at an unrecognized name. A project written the long
/// way is therefore meaningful to tooling that is not ours, which is why both spellings are read.
pub const FRAMEWORK_IDENTIFIER: &str = "Lamella";

/// The framework version that identifier is paired with.
///
/// `v1.0` rather than `1.0`: MSBuild's `TargetFrameworkVersion` carries the `v`, and a project that
/// spelled it differently would be read by every other tool as a different framework.
pub const FRAMEWORK_VERSION: &str = "v1.0";

/// The framework moniker, which is the spelling a project will actually carry.
///
/// **THE SHORT FORM IS CANONICAL AND THE PAIR IS ITS EXPANSION**, exactly as .NET carries both
/// `net8.0` and `.NETCoreApp,Version=v8.0` for one framework. A reader that took only the pair
/// would make every Lamella project file spell its framework the long way for no gain.
///
/// **`dotnet build` DOES NOT ACCEPT THIS MONIKER ON ITS OWN.** The .NET SDK maps a moniker it does
/// not ship to [`FRAMEWORK_IDENTIFIER`] and [`FRAMEWORK_VERSION`] only when a targets file hooks its
/// `BeforeTargetFrameworkInferenceTargets` extension point, and Lamella installs no such file, so
/// `dotnet build` stops at `NETSDK1013`. The Lamella tools read the moniker directly and are
/// unaffected. Spelling the pair in the project file instead is not a way around this: it clears
/// `NETSDK1013` and then fails at `MSB3644` for want of reference assemblies, which `dotnet` has no
/// way to supply for a Lamella target either.
pub const MONIKER: &str = "lamella1.0";

/// The one SDK an SDK-style project may name here.
pub const SDK: &str = "Microsoft.NET.Sdk";

/// Whether an element this reader does not recognize stops the build.
///
/// **ONE SITE, BECAUSE THE SEVERITY IS A POLICY AND THE DETECTION IS NOT.** Refusing and warning
/// need exactly the same scan, the same collection and the same sentence; only what the caller does
/// with the result differs. Holding that in one constant means the choice can be revisited without
/// touching the part that finds them.
const UNRECOGNIZED_IS_FATAL: bool = true;

/// What a project says it produces.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OutputType {
    /// A program with an entry point.
    Exe,
    /// A class library, which has none.
    Library,
}

/// An SDK-style project, reduced to what a build here consumes.
#[derive(Debug)]
pub struct Project {
    /// Whether this project is a program or a library.
    pub output_type: OutputType,
    /// The assembly's name; the project file's own stem when it says nothing.
    pub assembly_name: String,
    /// The `LangVersion` the project asks for, where it asks for one.
    pub lang_version: Option<String>,
    /// Whether `AllowUnsafeBlocks` is on.
    pub allow_unsafe: bool,
    /// The assemblies to bind and link against, **in the order the project declared them**.
    ///
    /// **DOCUMENT ORDER IS SEMANTIC.** Precedence across a reference set is first-declarer-wins, so
    /// a reader that sorted or de-duplicated these would change which definition of a name the
    /// program resolves to -- silently, and with a successful build to hide it.
    pub references: Vec<PathBuf>,
    /// The source files to compile, in a stable order.
    pub sources: Vec<PathBuf>,
}

/// One element of a project file, as scanned.
///
/// Attributes and children are kept in document order throughout: this file's whole contract with
/// the reader is that what they wrote first is read first.
#[derive(Debug, Clone)]
pub struct Element {
    /// The tag name, verbatim. MSBuild matches these case-insensitively and so does the reader
    /// below; the original spelling is kept so a message can quote what was typed.
    pub name: String,
    /// `(name, value)` for each attribute, in document order.
    pub attributes: Vec<(String, String)>,
    /// Child elements, in document order.
    pub children: Vec<Element>,
    /// The element's own text, with surrounding white space trimmed.
    pub text: String,
    /// The 1-based line the tag opened on, for a message that sends the reader to it.
    pub line: u32,
}

impl Element {
    /// The value of `name`, matched case-insensitively as MSBuild matches attributes.
    #[must_use]
    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The first child called `name`, matched case-insensitively.
    fn child(&self, name: &str) -> Option<&Element> {
        self.children
            .iter()
            .find(|child| child.name.eq_ignore_ascii_case(name))
    }
}

/// An element the reader does not recognize, with enough to find it again.
#[derive(Debug, Clone)]
struct Unrecognized {
    /// What it was called.
    name: String,
    /// Where it sat -- the containing element's name, so "a property" and "an item" are
    /// distinguishable without the reader knowing this file's vocabulary.
    within: String,
    /// The line it opened on.
    line: u32,
}

/// Scan `text` into its elements.
///
/// This knows XML's shape and nothing about projects: an element it has never heard of scans
/// exactly like one it has, and is reported later by the pass that owns the vocabulary.
///
/// # Errors
/// Text that is not well formed, naming the line.
pub fn read(text: &str) -> Result<Element, String> {
    let bytes: Vec<char> = text.chars().collect();
    let mut scanner = Scanner { chars: &bytes, at: 0, line: 1 };
    scanner.skip_prologue()?;
    let root = scanner.element()?;
    scanner.skip_trivia();
    if scanner.at < scanner.chars.len() {
        return Err(format!(
            "line {}: content after the closing </{}>",
            scanner.line, root.name
        ));
    }
    Ok(root)
}

/// The scanning cursor.
struct Scanner<'a> {
    chars: &'a [char],
    at: usize,
    line: u32,
}

impl Scanner<'_> {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.at).copied()
    }

    fn starts_with(&self, text: &str) -> bool {
        self.chars[self.at..]
            .iter()
            .zip(text.chars())
            .filter(|(a, b)| **a == *b)
            .count()
            == text.chars().count()
            && self.chars.len() - self.at >= text.chars().count()
    }

    fn bump(&mut self) -> Option<char> {
        let character = self.peek()?;
        if character == '\n' {
            self.line += 1;
        }
        self.at += 1;
        Some(character)
    }

    fn advance(&mut self, count: usize) {
        for _ in 0..count {
            self.bump();
        }
    }

    /// White space and comments, which may appear between anything.
    fn skip_trivia(&mut self) {
        loop {
            while self.peek().is_some_and(char::is_whitespace) {
                self.bump();
            }
            if self.starts_with("<!--") {
                self.advance(4);
                while self.at < self.chars.len() && !self.starts_with("-->") {
                    self.bump();
                }
                self.advance(3);
                continue;
            }
            break;
        }
    }

    /// The `<?xml ... ?>` declaration and any leading trivia, all optional.
    fn skip_prologue(&mut self) -> Result<(), String> {
        self.skip_trivia();
        if self.starts_with("<?") {
            while self.at < self.chars.len() && !self.starts_with("?>") {
                self.bump();
            }
            if self.at >= self.chars.len() {
                return Err(format!("line {}: unterminated <? ... ?>", self.line));
            }
            self.advance(2);
            self.skip_trivia();
        }
        Ok(())
    }

    /// A name: a tag or attribute identifier.
    fn name(&mut self) -> Result<String, String> {
        let start = self.at;
        while self
            .peek()
            .is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.' || c == ':')
        {
            self.bump();
        }
        if start == self.at {
            return Err(format!("line {}: expected a name", self.line));
        }
        Ok(self.chars[start..self.at].iter().collect())
    }

    /// One element and everything inside it.
    fn element(&mut self) -> Result<Element, String> {
        self.skip_trivia();
        let line = self.line;
        if self.peek() != Some('<') {
            return Err(format!("line {line}: expected an element"));
        }
        self.bump();
        let name = self.name()?;
        let mut attributes = Vec::new();
        loop {
            self.skip_trivia();
            match self.peek() {
                Some('/') => {
                    self.bump();
                    if self.peek() != Some('>') {
                        return Err(format!("line {}: expected > after /", self.line));
                    }
                    self.bump();
                    return Ok(Element {
                        name,
                        attributes,
                        children: Vec::new(),
                        text: String::new(),
                        line,
                    });
                }
                Some('>') => {
                    self.bump();
                    break;
                }
                Some(_) => {
                    let key = self.name()?;
                    self.skip_trivia();
                    if self.peek() != Some('=') {
                        return Err(format!("line {}: expected = after {key}", self.line));
                    }
                    self.bump();
                    self.skip_trivia();
                    let quote = self
                        .bump()
                        .filter(|c| *c == '"' || *c == '\'')
                        .ok_or_else(|| format!("line {}: expected a quoted value", self.line))?;
                    let start = self.at;
                    while self.peek().is_some_and(|c| c != quote) {
                        self.bump();
                    }
                    if self.peek().is_none() {
                        return Err(format!("line {}: unterminated attribute value", self.line));
                    }
                    let raw: String = self.chars[start..self.at].iter().collect();
                    self.bump();
                    attributes.push((key, unescape(&raw)));
                }
                None => return Err(format!("line {line}: unterminated <{name}")),
            }
        }
        let mut children = Vec::new();
        let mut text = String::new();
        loop {
            let before = self.at;
            self.skip_trivia();
            if self.starts_with("</") {
                self.advance(2);
                let closing = self.name()?;
                self.skip_trivia();
                if self.peek() != Some('>') {
                    return Err(format!("line {}: expected > closing {closing}", self.line));
                }
                self.bump();
                if !closing.eq_ignore_ascii_case(&name) {
                    return Err(format!(
                        "line {}: </{closing}> closes <{name}>, opened on line {line}",
                        self.line
                    ));
                }
                return Ok(Element {
                    name,
                    attributes,
                    children,
                    text: unescape(text.trim()),
                    line,
                });
            }
            if self.peek() == Some('<') {
                children.push(self.element()?);
                continue;
            }
            if self.peek().is_none() {
                return Err(format!("line {line}: <{name}> is never closed"));
            }
            if before != self.at && !text.is_empty() {
                text.push(' ');
            }
            while self.peek().is_some_and(|c| c != '<') {
                text.push(self.bump().unwrap_or_default());
            }
        }
    }
}

/// The five predefined XML entities. Nothing here needs numeric character references, and a project
/// file carrying one is better refused as unrecognized text than silently half-decoded.
fn unescape(text: &str) -> String {
    if !text.contains('&') {
        return text.to_owned();
    }
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

impl Project {
    /// Read the project at `path`.
    ///
    /// # Errors
    /// A file that cannot be read, is not well formed, or asks for something this reader does not
    /// recognize -- each naming what it found.
    pub fn read_file(path: &Path, verb: &str) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|error| {
            format!("lamella {verb}: read the project {}: {error}", path.display())
        })?;
        let document = read(&text)
            .map_err(|error| format!("lamella {verb}: {}: {error}", path.display()))?;
        Self::from_document(&document, path, verb)
    }

    /// Interpret a scanned project file.
    ///
    /// # Errors
    /// As [`Project::read_file`], minus the reading.
    pub fn from_document(document: &Element, path: &Path, verb: &str) -> Result<Self, String> {
        if !document.name.eq_ignore_ascii_case("Project") {
            return Err(format!(
                "lamella {verb}: {} starts with <{}>, and a project file starts with <Project>.",
                path.display(),
                document.name
            ));
        }
        match document.attribute("Sdk") {
            Some(sdk) if sdk.eq_ignore_ascii_case(SDK) => {}
            Some(sdk) => {
                return Err(format!(
                    "lamella {verb}: {} names the SDK {sdk:?}, and this build reads {SDK}.\n\n\
                     Nothing was built.",
                    path.display()
                ));
            }
            None => {
                return Err(format!(
                    "lamella {verb}: {} has no Sdk attribute.\n\n\
                     An SDK-style project opens `<Project Sdk=\"{SDK}\">`. The older \
                     non-SDK\nproject format, which names its own imports and targets, is not read \
                     here.\n\nNothing was built.",
                    path.display()
                ));
            }
        }

        let mut unrecognized = Vec::new();
        let mut identifier = None;
        let mut version = None;
        let mut moniker = None;
        let mut output_type = None;
        let mut assembly_name = None;
        let mut lang_version = None;
        let mut allow_unsafe = false;
        let mut references: Vec<(String, Option<String>, u32)> = Vec::new();
        let mut included = Vec::new();
        let mut removed = Vec::new();

        for group in &document.children {
            if group.name.eq_ignore_ascii_case("PropertyGroup") {
                for property in &group.children {
                    let value = property.text.clone();
                    match property.name.to_ascii_lowercase().as_str() {
                        "targetframeworkidentifier" => identifier = Some(value),
                        "targetframeworkversion" => version = Some(value),
                        "targetframework" => moniker = Some((value, property.line)),
                        "outputtype" => output_type = Some((value, property.line)),
                        "assemblyname" => assembly_name = Some(value),
                        "langversion" => lang_version = Some(value),
                        "allowunsafeblocks" => allow_unsafe = value.eq_ignore_ascii_case("true"),
                        _ => unrecognized.push(Unrecognized {
                            name: property.name.clone(),
                            within: "PropertyGroup".to_owned(),
                            line: property.line,
                        }),
                    }
                }
            } else if group.name.eq_ignore_ascii_case("ItemGroup") {
                for item in &group.children {
                    match item.name.to_ascii_lowercase().as_str() {
                        "reference" => {
                            let include =
                                item.attribute("Include").unwrap_or_default().to_owned();
                            let hint = item.child("HintPath").map(|hint| hint.text.clone());
                            references.push((include, hint, item.line));
                        }
                        "compile" => {
                            if let Some(include) = item.attribute("Include") {
                                included.push(include.to_owned());
                            }
                            if let Some(remove) = item.attribute("Remove") {
                                removed.push(remove.to_owned());
                            }
                        }
                        _ => unrecognized.push(Unrecognized {
                            name: item.name.clone(),
                            within: "ItemGroup".to_owned(),
                            line: item.line,
                        }),
                    }
                }
            } else {
                unrecognized.push(Unrecognized {
                    name: group.name.clone(),
                    within: "Project".to_owned(),
                    line: group.line,
                });
            }
        }

        if UNRECOGNIZED_IS_FATAL && !unrecognized.is_empty() {
            return Err(unrecognized_refusal(verb, path, &unrecognized));
        }

        check_framework(verb, path, moniker.as_ref(), identifier.as_deref(), version.as_deref())?;

        let output_type = match output_type {
            None => OutputType::Exe,
            Some((value, _)) if value.eq_ignore_ascii_case("Exe") => OutputType::Exe,
            Some((value, _)) if value.eq_ignore_ascii_case("Library") => OutputType::Library,
            Some((value, line)) => {
                return Err(format!(
                    "lamella {verb}: {}, line {line}: OutputType is {value:?}.\n\n\
                     This build reads Exe and Library.\n\nNothing was built.",
                    path.display()
                ));
            }
        };

        let directory = project_directory(path);
        let mut sources = if included.is_empty() {
            glob_sources(directory)
        } else {
            included.iter().map(|one| directory.join(one)).collect()
        };
        let excluded: BTreeSet<PathBuf> =
            removed.iter().map(|one| directory.join(one)).collect();
        sources.retain(|source| !excluded.contains(source));
        if sources.is_empty() {
            return Err(format!(
                "lamella {verb}: {} has no source files to compile.\n\n\
                 A project compiles every `.cs` beside it unless it says otherwise. Add one, or \
                 name\nthem with <Compile Include=\"...\" />.\n\nNothing was built.",
                path.display()
            ));
        }

        let references = resolve_references(verb, path, directory, &references)?;

        let assembly_name = assembly_name.filter(|name| !name.is_empty()).unwrap_or_else(|| {
            path.file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("Program")
                .to_owned()
        });

        Ok(Self {
            output_type,
            assembly_name,
            lang_version: lang_version.filter(|value| !value.is_empty()),
            allow_unsafe,
            references,
            sources,
        })
    }
}

/// Where each `<Reference>`'s assembly actually lives, in declaration order.
///
/// **A `HintPath` IS HOW A PROJECT NAMES A `.dll` WITH NO PACKAGE FEED**, and it is the only route
/// this reader offers: resolving a bare `Include` would mean a search ladder and a set of implicit
/// assemblies, and nothing is available to a build here that its project did not name.
fn resolve_references(
    verb: &str,
    path: &Path,
    directory: &Path,
    declared: &[(String, Option<String>, u32)],
) -> Result<Vec<PathBuf>, String> {
    let mut resolved = Vec::new();
    for (include, hint, line) in declared {
        let Some(hint) = hint.as_ref().filter(|hint| !hint.is_empty()) else {
            return Err(format!(
                "lamella {verb}: {}, line {line}: <Reference Include=\"{include}\"> has no \
                 HintPath.\n\n\
                 Nothing is available to a build here that this project does not name, so a \
                 reference\nhas to say where the assembly is:\n\n\
                 \x20   <Reference Include=\"{include}\">\n\
                 \x20     <HintPath>..\\path\\to\\{include}.dll</HintPath>\n\
                 \x20   </Reference>\n\n\
                 Nothing was built.",
                path.display()
            ));
        };
        resolved.push(directory.join(hint.replace('\\', "/")));
    }
    Ok(resolved)
}

/// Every `.cs` beside the project, which is what an SDK-style project compiles by default.
///
/// `bin` and `obj` are skipped, as the SDK skips them: they hold build output, and compiling a
/// previous build's copy of a file is a duplicate-definition error nobody can see the cause of.
fn glob_sources(directory: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    walk(directory, &mut found);
    found.sort();
    found
}

/// The directory a project's relative paths are resolved against.
///
/// **A BARE FILENAME'S PARENT IS AN EMPTY PATH, NOT `None`.** `Path::parent` answers `None` only for
/// a root, so `App.csproj` -- what a user types from inside their own project directory -- names the
/// empty path rather than the working directory.
fn project_directory(path: &Path) -> &Path {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent,
        _ => Path::new("."),
    }
}

/// The recursive half of [`glob_sources`].
fn walk(directory: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let skip = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.eq_ignore_ascii_case("bin") || name.eq_ignore_ascii_case("obj"));
            if !skip {
                walk(&path, found);
            }
        } else if path.extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("cs")) {
            found.push(path);
        }
    }
}

/// Whether the project targets this toolchain, and what to do when it does not.
///
/// **BOTH SPELLINGS ARE READ, AND THEY MUST AGREE WHERE BOTH APPEAR.** `<TargetFramework>` is the
/// moniker and the pair is its expansion, exactly as .NET carries `net8.0` beside
/// `.NETCoreApp,Version=v8.0`. A project giving both and disagreeing has said two things, and
/// picking one would build something it did not ask for.
fn check_framework(
    verb: &str,
    path: &Path,
    moniker: Option<&(String, u32)>,
    identifier: Option<&str>,
    version: Option<&str>,
) -> Result<(), String> {
    let spellings = format!(
        "\x20   <TargetFramework>{MONIKER}</TargetFramework>\n\n\
         or, written out:\n\n\
         \x20   <TargetFrameworkIdentifier>{FRAMEWORK_IDENTIFIER}</TargetFrameworkIdentifier>\n\
         \x20   <TargetFrameworkVersion>{FRAMEWORK_VERSION}</TargetFrameworkVersion>"
    );
    let short = match moniker {
        Some((value, line)) if value == MONIKER => Some(true),
        Some((value, line)) => {
            return Err(format!(
                "lamella {verb}: {}, line {line}: this project targets {value:?}, which \
                 this build does not produce.\n\n\
                 It builds:\n\n\
                 {spellings}\n\n\
                 Nothing was built.",
                path.display()
            ));
        }
        None => None,
    };
    let long = match (identifier, version) {
        (None, None) => None,
        (Some(one), Some(two)) if one == FRAMEWORK_IDENTIFIER && two == FRAMEWORK_VERSION => {
            Some(true)
        }
        (Some(one), Some(two)) => {
            return Err(format!(
                "lamella {verb}: {} targets {one},Version={two}, which this build does \
                 not produce.\n\n\
                 It builds:\n\n\
                 {spellings}\n\n\
                 Nothing was built.",
                path.display()
            ));
        }
        (one, two) => {
            let missing = if one.is_none() {
                "TargetFrameworkIdentifier"
            } else {
                "TargetFrameworkVersion"
            };
            let _ = two;
            return Err(format!(
                "lamella {verb}: {} sets one half of the framework pair and not the \
                 other; <{missing}> is missing.\n\n\
                 It builds:\n\n\
                 {spellings}\n\n\
                 Nothing was built.",
                path.display()
            ));
        }
    };
    if short.is_none() && long.is_none() {
        return Err(format!(
            "lamella {verb}: {} does not say which framework it targets.\n\n\
             A project built here says:\n\n\
             {spellings}\n\n\
             Nothing was built.",
            path.display()
        ));
    }
    Ok(())
}

/// Everything the reader did not recognize, named in one pass.
///
/// **EVERY ONE OF THEM, NOT THE FIRST.** A refusal that surfaces one element per run turns adapting
/// a project into a dozen round trips, each of which looks like a fresh problem; a list turns it
/// into one edit. That matters more than what the refusal is called.
///
/// **AND IT SAYS THIS IS STRICTER THAN MSBuild, BECAUSE IT IS.** MSBuild's model is open -- anything
/// inside a `PropertyGroup` IS a property and anything inside an `ItemGroup` IS an item, so a
/// project carrying invented elements builds there with no warning at all. A reader that refused
/// them while implying `dotnet build` would have done the same would be blaming the file for a
/// choice made here.
fn unrecognized_refusal(verb: &str, path: &Path, found: &[Unrecognized]) -> String {
    let mut listed = String::new();
    for one in found {
        let _ = write!(
            listed,
            "\n    line {}: <{}>, inside <{}>",
            one.line, one.name, one.within
        );
    }
    format!(
        "lamella {verb}: {} uses {} this build does not recognize:\n{listed}\n\n\
         Everything a project asks for here has to be something this build acts on, so an element \
         it\ndoes not know is refused rather than passed over. All of them are listed above; there \
         is no\nsecond one waiting behind the first.\n\n\
         \x20   This is STRICTER THAN MSBuild. `dotnet build` accepts any element inside a\n\
         \x20   PropertyGroup or an ItemGroup -- that is its extension model -- and would build \
         this\n\x20   file without a warning. The difference is deliberate: an element that is \
         inert here\n\x20   may be one you believe is in effect.\n\n\
         Nothing was built.",
        path.display(),
        if found.len() == 1 { "an element" } else { "elements" }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A BARE FILENAME'S PARENT IS AN EMPTY PATH, NOT `None`.** `Path::parent` answers `None`
    /// only for a root, so `App.csproj` -- the spelling a user types from inside their own
    /// project directory -- resolved every relative path against `""`.
    ///
    /// The two consumers disagreed about what that meant, which is why it survived every fixture
    /// here: `Path::join` on an empty base quietly yields a usable relative path, so `<Compile>`
    /// items and `<HintPath>` worked, while `read_dir("")` fails -- so the source glob found
    /// nothing and the project was refused for having no sources to compile.
    #[test]
    fn a_project_named_without_a_directory_reads_the_working_directory() {
        assert_eq!(project_directory(Path::new("App.csproj")), Path::new("."));
        assert_eq!(
            project_directory(Path::new("proj/App.csproj")),
            Path::new("proj")
        );
    }


    const HEAD: &str = "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    \
                        <TargetFrameworkIdentifier>Lamella</TargetFrameworkIdentifier>\n    \
                        <TargetFrameworkVersion>v1.0</TargetFrameworkVersion>\n";

    /// A `<Compile>` item that every fixture carries: a project with no sources is refused
    /// before anything else is looked at -- which is right, and would otherwise be the only
    /// thing these cases ever measured.
    const SOURCES: &str = "  <ItemGroup><Compile Include=\"P.cs\" /></ItemGroup>\n";

    fn parse(body: &str) -> Result<Project, String> {
        let text = format!("{HEAD}  </PropertyGroup>\n{SOURCES}{body}</Project>\n");
        let document = read(&text).map_err(|error| format!("scan: {error}"))?;
        Project::from_document(&document, Path::new("Fixture.csproj"), "build")
    }

    /// **DOCUMENT ORDER IS THE ANSWER, NOT AN ARTIFACT OF IT.** Precedence across a reference set is
    /// first-declarer-wins, so a reader that sorted or de-duplicated these would change which
    /// definition of a name the program binds -- with a successful build to hide it.
    #[test]
    fn references_are_read_in_the_order_the_project_declares_them() {
        let project = parse(
            "  <ItemGroup>\n\
             \x20   <Reference Include=\"Gpio\"><HintPath>a/Gpio.dll</HintPath></Reference>\n\
             \x20   <Reference Include=\"Spi\"><HintPath>b/Spi.dll</HintPath></Reference>\n\
             \x20   <Reference Include=\"Bsp\"><HintPath>c/Bsp.dll</HintPath></Reference>\n\
             \x20 </ItemGroup>\n",
        )
        .expect("a project with three references");
        let names: Vec<String> = project
            .references
            .iter()
            .map(|one| one.file_name().unwrap_or_default().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["Gpio.dll", "Spi.dll", "Bsp.dll"]);
    }

    /// **A REFERENCE WITH NO HintPath HAS NOT SAID WHERE THE ASSEMBLY IS**, and nothing is
    /// available to a build here that the project did not name -- so it is refused with the shape
    /// to write, rather than resolved from a search ladder that does not exist.
    #[test]
    fn a_reference_without_a_hint_path_is_refused_and_shows_the_shape() {
        let error = parse("  <ItemGroup><Reference Include=\"Gpio\" /></ItemGroup>\n")
            .expect_err("there is nowhere to look");
        assert!(error.contains("HintPath"), "names what is missing: {error}");
        assert!(error.contains("Gpio"), "and the reference: {error}");
        assert!(error.contains("Nothing was built"), "and stops: {error}");
    }

    /// **EVERY UNRECOGNIZED ELEMENT IN ONE PASS.** One per run would turn adapting a project into a
    /// round trip per element, each looking like a fresh problem.
    #[test]
    fn every_unrecognized_element_is_named_at_once() {
        let text = format!(
            "{HEAD}    <Invented>1</Invented>\n    <AlsoInvented>2</AlsoInvented>\n  \
             </PropertyGroup>\n  <ItemGroup><MadeUpItem Include=\"x\" /></ItemGroup>\n</Project>\n"
        );
        let document = read(&text).expect("it scans -- the shape is fine, the vocabulary is not");
        let error = Project::from_document(&document, Path::new("F.csproj"), "build")
            .expect_err("three elements this build does not act on");
        for name in ["Invented", "AlsoInvented", "MadeUpItem"] {
            assert!(error.contains(name), "names {name}: {error}");
        }
        assert!(
            error.contains("STRICTER THAN MSBuild"),
            "and says whose choice this is: {error}"
        );
    }

    /// **THE BARE SPELLING IS REFUSED WITH THE PAIR THAT REPLACES IT**, which is the same advice
    /// the .NET SDK gives when it meets a framework it does not know.
    ///
    /// **AND THIS ONE CHECKS THE WHOLE MESSAGE RATHER THAN A FRAGMENT OF IT.** A `contains`
    /// assertion answers "is this word present" and never "is this the sentence we meant to
    /// emit", so everything that makes a refusal readable -- where it wraps, whether the sample
    /// under it lines up -- is invisible to one. A refusal is read by a person, so the thing
    /// under test is the whole of it.
    #[test]
    fn the_bare_spelling_is_refused_with_the_pair_that_replaces_it() {
        let text = "<Project Sdk=\"Microsoft.NET.Sdk\">
  <PropertyGroup>
    <TargetFramework>lamella</TargetFramework>
  </PropertyGroup>
</Project>
";
        let document = read(text).expect("it scans");
        let error = Project::from_document(&document, Path::new("F.csproj"), "build")
            .expect_err("the bare name is not the moniker");
        assert_eq!(
            error,
            "lamella build: F.csproj, line 3: this project targets \"lamella\", which this \
             build does not produce.\n\n\
             It builds:\n\n\
             \x20   <TargetFramework>lamella1.0</TargetFramework>\n\n\
             or, written out:\n\n\
             \x20   <TargetFrameworkIdentifier>Lamella</TargetFrameworkIdentifier>\n\
             \x20   <TargetFrameworkVersion>v1.0</TargetFrameworkVersion>\n\n\
             Nothing was built."
        );
    }

    /// **EVERY FRAMEWORK REFUSAL, RENDERED.** One of them is checked word for word above; this
    /// asks the weaker question of all five, so the next refusal added here is covered without
    /// anybody remembering to write a second exact match for it.
    #[test]
    fn every_framework_refusal_renders_without_stray_columns() {
        for (what, properties) in [
            ("a bare name", "<TargetFramework>lamella</TargetFramework>"),
            ("another moniker", "<TargetFramework>net10.0</TargetFramework>"),
            ("another pair", "<TargetFrameworkIdentifier>.NETCoreApp</TargetFrameworkIdentifier>\n    <TargetFrameworkVersion>v10.0</TargetFrameworkVersion>"),
            (
                "half a pair",
                "<TargetFrameworkIdentifier>Lamella</TargetFrameworkIdentifier>",
            ),
            ("no framework at all", ""),
        ] {
            let text = format!(
                "<Project Sdk={SDK:?}>\n  <PropertyGroup>\n    {properties}\n  </PropertyGroup>\n</Project>\n"
            );
            let document = read(&text).expect("it scans");
            let error = Project::from_document(&document, Path::new("F.csproj"), "build")
                .expect_err(what);
            crate::rendered::assert_renders_cleanly(&error, crate::rendered::four_space_sample);
        }
    }

    /// **THE MONIKER IS THE SPELLING A PROJECT WILL ACTUALLY CARRY, AND IT IS READ**, beside the
    /// pair it expands to -- as .NET carries `net8.0` beside `.NETCoreApp,Version=v8.0`.
    #[test]
    fn the_moniker_alone_targets_this_toolchain() {
        let text = "<Project Sdk=\"Microsoft.NET.Sdk\">
  <PropertyGroup>
                        <TargetFramework>lamella1.0</TargetFramework>
  </PropertyGroup>
                      <ItemGroup><Compile Include=\"P.cs\" /></ItemGroup>
</Project>
";
        let document = read(text).expect("it scans");
        let project = Project::from_document(&document, Path::new("F.csproj"), "build")
            .expect("the moniker is the canonical spelling");
        assert_eq!(project.output_type, OutputType::Exe);
    }

    /// **A MONIKER THAT IS NOT OURS IS REFUSED BY NAME**, with both spellings that would work.
    #[test]
    fn another_moniker_is_refused_and_shows_both_spellings() {
        let text = "<Project Sdk=\"Microsoft.NET.Sdk\">
  <PropertyGroup>
                        <TargetFramework>net10.0</TargetFramework>
  </PropertyGroup>
</Project>
";
        let document = read(text).expect("it scans");
        let error = Project::from_document(&document, Path::new("F.csproj"), "build")
            .expect_err("a different framework");
        assert!(error.contains("net10.0"), "names theirs: {error}");
        assert!(error.contains(MONIKER), "and the moniker: {error}");
        assert!(error.contains(FRAMEWORK_IDENTIFIER), "and the pair: {error}");
    }

    /// **HALF A PAIR IS AN UNFINISHED EDIT, NOT A SPELLING.** MSBuild would infer the missing half
    /// from a moniker it knows; ours is not one, so it is named rather than guessed.
    #[test]
    fn half_the_pair_names_the_missing_half() {
        let text = "<Project Sdk=\"Microsoft.NET.Sdk\">
  <PropertyGroup>
                        <TargetFrameworkIdentifier>Lamella</TargetFrameworkIdentifier>
                      </PropertyGroup>
</Project>
";
        let document = read(text).expect("it scans");
        let error = Project::from_document(&document, Path::new("F.csproj"), "build")
            .expect_err("the version is missing");
        assert!(error.contains("TargetFrameworkVersion"), "{error}");
    }

    /// **ANOTHER FRAMEWORK IS NOT SILENTLY BUILT AS OURS.**
    #[test]
    fn a_project_targeting_something_else_is_refused_by_name() {
        let text = "<Project Sdk=\"Microsoft.NET.Sdk\">\n  <PropertyGroup>\n    \
                    <TargetFrameworkIdentifier>.NETCoreApp</TargetFrameworkIdentifier>\n    \
                    <TargetFrameworkVersion>v10.0</TargetFrameworkVersion>\n  \
                    </PropertyGroup>\n</Project>\n";
        let document = read(text).expect("it scans");
        let error = Project::from_document(&document, Path::new("F.csproj"), "build")
            .expect_err("a different framework");
        assert!(error.contains(".NETCoreApp"), "names theirs: {error}");
        assert!(error.contains(FRAMEWORK_IDENTIFIER), "and ours: {error}");
        assert!(error.contains(MONIKER), "and the short spelling: {error}");
    }

    /// **A NON-SDK PROJECT IS REFUSED BY NAME**, because it carries its own imports and targets and
    /// nothing here reads them.
    #[test]
    fn a_project_with_no_sdk_attribute_is_refused() {
        let document = read("<Project><PropertyGroup /></Project>").expect("it scans");
        let error = Project::from_document(&document, Path::new("F.csproj"), "build")
            .expect_err("no Sdk");
        assert!(error.contains("Sdk"), "{error}");
        assert!(error.contains(SDK), "and names the one it reads: {error}");
    }

    /// The scanner's own contract: comments, declarations, self-closing tags and attribute
    /// escaping are shape, and it handles them without knowing what any of it means.
    #[test]
    fn the_scanner_takes_the_shapes_a_project_file_actually_carries() {
        let document = read(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
             <!-- a comment before the root -->\n\
             <Project Sdk=\"Microsoft.NET.Sdk\">\n\
             \x20 <!-- and one inside -->\n\
             \x20 <ItemGroup>\n\
             \x20   <Compile Remove=\"Gen&amp;Old.cs\" />\n\
             \x20 </ItemGroup>\n\
             </Project>",
        )
        .expect("all of this is ordinary");
        let group = &document.children[0];
        assert_eq!(group.name, "ItemGroup");
        assert_eq!(group.children[0].attribute("Remove"), Some("Gen&Old.cs"));
        assert_eq!(document.attribute("sdk"), Some(SDK), "attributes match case-insensitively");
    }

    /// **A MISMATCHED CLOSING TAG NAMES BOTH LINES.** A project file is hand-edited, so this is the
    /// ordinary mistake rather than the exotic one.
    #[test]
    fn a_mismatched_closing_tag_names_what_it_closed_and_where_it_opened() {
        let error = read("<Project>\n  <PropertyGroup>\n  </ItemGroup>\n</Project>")
            .expect_err("that closes the wrong element");
        assert!(error.contains("ItemGroup"), "{error}");
        assert!(error.contains("PropertyGroup"), "{error}");
        assert!(error.contains("line 2"), "and where it opened: {error}");
    }
}
