//! Which of .NET's two builds a program is compiled as: Debug or Release.

/// Which of .NET's two builds a program is compiled as, the choice a project makes with its
/// `Optimize` property: a Debug build (`Optimize` false, what `dotnet build` produces by default) or
/// a Release build (`Optimize` true, what `dotnet build -c Release` produces).
///
/// Every entry point that builds a program image takes one, and none supplies a default: what a
/// debugger can show depends on it, so a build that assumed one would be right only when the
/// assumption happened to match the project.
///
/// The two produce different code, as a .NET Debug build's JIT does:
///
/// * [`BuildKind::Debug`] keeps every value of every method the program declares in its own stack
///   slot for the whole method, an address-taken local included, so each parameter and named local
///   can be read at every point a debugger can stop at. It gives every statement an instruction of
///   its own, so every sequence point the compiler wrote has an address. `Main(string[] args)` is
///   given the empty array .NET gives a program run with no arguments, whether or not the body
///   reads it.
/// * [`BuildKind::Release`] is the optimized build: values live in registers where they fit, and a
///   statement that computes nothing produces no code. A `Main` whose body never reads `args` is
///   given null, which nothing in the program can observe, and its debug information gives `args`
///   no location, so a debugger reports it as unavailable rather than showing a null the program
///   never had.
///
/// Libraries, the class library among them, are built the same way in either case, as a .NET
/// program's references are optimized whichever way the program itself is built. So are the
/// linker-free images of `build::build`, which carry no debug information: the kind changes only
/// what an unread `args` is given there.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BuildKind {
    /// The unoptimized build a debugger is used with.
    Debug,
    /// The optimized build.
    Release,
}

impl BuildKind {
    /// The kind a project's `Optimize` property selects: `true` is [`BuildKind::Release`], `false`
    /// is [`BuildKind::Debug`].
    #[must_use]
    pub fn from_optimize(optimize: bool) -> Self {
        if optimize {
            BuildKind::Release
        } else {
            BuildKind::Debug
        }
    }

    /// The kind named `name`, spelled as .NET spells its two standard configurations: `"Debug"` or
    /// `"Release"`. Any other text is `None`, including the empty string, so a caller that passed
    /// nothing is refused rather than given one of the two.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Debug" => Some(BuildKind::Debug),
            "Release" => Some(BuildKind::Release),
            _ => None,
        }
    }

    /// The name [`BuildKind::from_name`] reads back.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            BuildKind::Debug => "Debug",
            BuildKind::Release => "Release",
        }
    }

    /// Whether this is [`BuildKind::Debug`].
    #[must_use]
    pub fn is_debug(self) -> bool {
        self == BuildKind::Debug
    }
}

#[cfg(test)]
mod tests {
    use super::BuildKind;

    #[test]
    fn a_name_reads_back_and_nothing_else_is_a_kind() {
        for kind in [BuildKind::Debug, BuildKind::Release] {
            assert_eq!(BuildKind::from_name(kind.name()), Some(kind));
        }
        for other in ["", "debug", "release", "Checked", "Debug "] {
            assert_eq!(BuildKind::from_name(other), None, "{other:?}");
        }
    }

    #[test]
    fn optimize_selects_release() {
        assert_eq!(BuildKind::from_optimize(true), BuildKind::Release);
        assert_eq!(BuildKind::from_optimize(false), BuildKind::Debug);
    }
}
