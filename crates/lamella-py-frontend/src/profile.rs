//! Compiling against a target image's capability profile.

use alloc::string::String;

use lamella_py_bytecode as bc;

#[doc(no_inline)]
pub use lamella_py_bytecode::{Capability, Profile};

/// A constant the target image cannot materialize, named where the developer can act on it.
///
/// Shaped like [`crate::parser::ParseError`] -- a line plus a message -- so a consumer that already
/// maps one onto a buffer maps this one the same way, rather than learning a second convention.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapabilityError {
    /// The 1-based source line the constant is loaded on, or `0` when no line is known.
    ///
    /// Zero is a real answer rather than a placeholder, and it happens for two reasons worth
    /// telling apart: the constant belongs to a compiler desugaring that no line of source
    /// produced, or the module's line tables were stripped for size. In both cases naming a line
    /// the developer never wrote would be worse than naming none.
    pub line: u32,
    /// The capability the image does not provide.
    pub capability: Capability,
    /// What needs it, from the constant's side (`a float constant`) rather than the syntax's, so the
    /// text stays true for one the compiler synthesized.
    pub construct: &'static str,
    /// The function the constant lives in, or `<module>` for top-level code. Carries the location
    /// when `line` is `0`, which is exactly when it is most needed.
    pub code_object: String,
}

impl core::fmt::Display for CapabilityError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.line > 0 {
            write!(f, "line {}: ", self.line)?;
        }
        write!(
            f,
            "{} in '{}' needs the '{}' capability, which this image does not provide",
            self.construct, self.code_object, self.capability
        )
    }
}

/// Refuse `module` if any constant it holds needs a capability `profile` does not provide.
///
/// Checks the module body and every function; a bundle checks each of its modules. Reports the
/// FIRST refusal in pool order, as every other error in this front end reports the first.
///
/// # Errors
///
/// [`CapabilityError`] naming the capability, the construct and the source line.
pub fn check_module(module: &bc::Module, profile: Profile) -> Result<(), CapabilityError> {
    check_code(&module.body, profile)?;
    for function in module.functions.iter_bodies() {
        check_code(function, profile)?;
    }
    Ok(())
}

/// Refuse `bundle` if any module in it holds a constant `profile` cannot materialize.
///
/// # Errors
///
/// [`CapabilityError`] from the first module that fails, entry first.
pub fn check_bundle(bundle: &bc::Bundle, profile: Profile) -> Result<(), CapabilityError> {
    check_module(&bundle.entry, profile)?;
    for module in &bundle.modules {
        check_module(module, profile)?;
    }
    Ok(())
}

fn check_code(code: &bc::CodeObject, profile: Profile) -> Result<(), CapabilityError> {
    for (index, konst) in code.consts.iter().enumerate() {
        let Some(capability) = konst.required_capability() else {
            continue;
        };
        if profile.supports(capability) {
            continue;
        }
        return Err(CapabilityError {
            line: line_of_const(code, index),
            capability,
            construct: construct_of(konst),
            code_object: code.name.clone(),
        });
    }
    Ok(())
}

/// The source line of the first op that loads constant `index`, or `0` if none does or the line is
/// unknown.
///
/// [`bc::Op::LoadConst`] is the only op that can name one of the refusable constants -- the other
/// const-indexing ops read argument-tag and keyword-name entries, which every image can hold -- so a
/// constant with no `LoadConst` is one the pool carries and nothing reaches. It is still refused (it
/// is in the encoding), and it honestly reports no line.
fn line_of_const(code: &bc::CodeObject, index: usize) -> u32 {
    let Ok(index) = u32::try_from(index) else {
        return 0;
    };
    code.ops
        .iter()
        .position(|op| matches!(op, bc::Op::LoadConst(i) if *i == index))
        .and_then(|ip| code.line_for(ip))
        .unwrap_or(0)
}

/// How a refused constant is named in a diagnostic.
fn construct_of(konst: &bc::Const) -> &'static str {
    match konst {
        bc::Const::Float(_) => "a float constant",
        bc::Const::Imaginary(_) => "an imaginary constant",
        _ => "a constant",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{compile_str, compile_str_for_profile};
    use alloc::string::ToString;

    #[test]
    fn a_float_literal_is_refused_by_a_no_float_profile_and_names_its_line() {
        let source = "x = 1\ny = 2\nz = 3.5\n";
        let no_float = Profile::FULL.without(Capability::Float);

        let err = compile_str_for_profile("m", source, no_float).expect_err("3.5 cannot be encoded");
        let crate::FrontendError::Capability(err) = err else {
            panic!("a capability refusal, not {err}");
        };
        assert_eq!(err.capability, Capability::Float);
        assert_eq!(err.line, 3, "the line table answers which line, exactly");
        assert_eq!(err.code_object, "<module>");
        let text = err.to_string();
        assert!(text.contains("line 3") && text.contains("'float'"), "{text}");

        assert!(compile_str_for_profile("m", source, Profile::FULL).is_ok());
        assert!(compile_str("m", source).is_ok());
    }

    #[test]
    fn a_float_inside_a_function_names_the_function_and_the_line() {
        let source = "def f():\n    return 2.5\n";
        let err = compile_str_for_profile("m", source, Profile::FULL.without(Capability::Float))
            .expect_err("refused");
        let crate::FrontendError::Capability(err) = err else {
            panic!("a capability refusal, not {err}");
        };
        assert_eq!(err.code_object, "f");
        assert_eq!(err.line, 2);
    }

    #[test]
    fn an_imaginary_literal_needs_complex_even_where_float_is_present() {
        let source = "z = 2j\n";
        let no_complex = Profile::FULL.without(Capability::Complex);
        assert!(no_complex.supports(Capability::Float), "only complex was dropped");

        let err = compile_str_for_profile("m", source, no_complex).expect_err("refused");
        let crate::FrontendError::Capability(err) = err else {
            panic!("a capability refusal, not {err}");
        };
        assert_eq!(err.capability, Capability::Complex);
        assert!(compile_str_for_profile("m", source, Profile::FULL).is_ok());
    }

    #[test]
    fn dropping_float_drops_complex_with_it() {
        let no_float = Profile::FULL.without(Capability::Float);
        assert!(!no_float.supports(Capability::Complex));
        assert!(compile_str_for_profile("m", "z = 2j\n", no_float).is_err());

        let complex_only = Profile::BARE.with(Capability::Complex);
        assert!(complex_only.supports(Capability::Float));
    }

    #[test]
    fn an_integer_program_compiles_on_the_smallest_profile() {
        let source = "\
def fib(n: int) -> int:
    a: int = 0
    b: int = 1
    i: int = 0
    while i < n:
        t: int = a + b
        a = b
        b = t
        i = i + 1
    return a

print(fib(10))
";
        assert!(compile_str_for_profile("m", source, Profile::BARE).is_ok());
    }

    #[test]
    fn a_true_division_is_not_refused_even_though_it_produces_a_float() {
        let no_float = Profile::FULL.without(Capability::Float);
        assert!(compile_str_for_profile("m", "def f(a, b):\n    return a / b\n", no_float).is_ok());
        assert!(compile_str_for_profile("m", "def g(x, n):\n    return x ** n\n", no_float).is_ok());
    }

    #[test]
    fn the_check_reads_the_artifact_so_a_synthesized_constant_cannot_slip_past() {
        let mut module = compile_str("m", "x = 1\n").expect("compiles");
        module.body.consts.push(bc::Const::Float(2.5f64.to_bits()));

        let err = check_module(&module, Profile::FULL.without(Capability::Float))
            .expect_err("the pool is the encoding, whatever put the constant in it");
        assert_eq!(err.capability, Capability::Float);
        assert_eq!(err.line, 0, "no op loads it, so there is no line to name");
        assert!(check_module(&module, Profile::FULL).is_ok());
    }

    #[test]
    fn a_bundle_checks_every_module_it_carries() {
        let resolve = |name: &str| -> Option<String> {
            match name {
                "helper" => Some(String::from("SCALE = 1.5\n")),
                _ => None,
            }
        };
        let no_float = Profile::FULL.without(Capability::Float);
        let err = crate::compile_bundle_for_profile("__main__", "import helper\n", &resolve, no_float)
            .expect_err("the imported module's float is refused too");
        let text = err.to_string();
        assert!(text.contains("helper"), "the failing module is named: {text}");
        assert!(text.contains("'float'"), "{text}");
    }
}
