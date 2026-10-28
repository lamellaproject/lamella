//! Why a baked image would stop on a board, each place named by the method it is in and the member
//! it could not reach.
//!
//! A bake writes an image whether or not every call in it resolves, and the runtime that boots it
//! stops at a call that resolves to nothing. Every tool that bakes checks the image first and refuses
//! with these lines, rather than sending something that stops on the board.

#[cfg(feature = "baked")]
use lamella_cil_runtime::module::ProfileViolation;
#[cfg(feature = "baked")]
use lamella_metadata::Assembly;
#[cfg(feature = "baked")]
use lamella_metadata::signature::SigType;
#[cfg(feature = "baked")]
use lamella_metadata::tables::table;

/// The name ECMA-335 gives the kernel library, which the compiler scopes its well-known types to.
/// A reference by this name is a reference to the corlib, whatever the corlib calls itself.
const KERNEL_LIBRARY: &str = "mscorlib";

/// Whether an assembly reference by `name` means the corlib: the kernel library's name, or the
/// corlib's own.
///
/// **ONE ANSWER FOR BOTH TIERS THAT CARRY LIBRARIES**: the bake's set and the class-library link's
/// are cut from the same reference set by it.
pub fn names_the_corlib(name: &str, corlib: &lamella_metadata::Assembly) -> bool {
    name == KERNEL_LIBRARY || corlib.assembly_name() == Some(name)
}

/// The name of the assembly a type reference resolves in, following a nested type out to the type
/// that encloses it and an instantiation to its generic definition. `None` for a type this
/// assembly defines itself.
///
/// **A LIBRARY IS NAMED BY THE REFERENCE, NOT GUESSED FROM THE MEMBER.** Every reference to a type
/// in another assembly carries that assembly as its scope, so which library a member lives in is a
/// fact of the referencing assembly's own metadata.
#[cfg(feature = "baked")]
fn scope_assembly(assembly: &Assembly, token: lamella_token::Token) -> Option<String> {
    let mut current = token;
    for _ in 0..16 {
        match current.table() {
            table::TYPE_REF => {
                let scope = assembly.type_ref(current.row())?.resolution_scope();
                if scope.table() == table::ASSEMBLY_REF {
                    return assembly
                        .assembly_ref(scope.row())
                        .and_then(|reference| reference.name())
                        .map(String::from);
                }
                current = scope;
            }
            table::TYPE_SPEC => current = generic_definition(assembly, current)?,
            _ => return None,
        }
    }
    None
}

/// The generic definition an instantiation's `TypeSpec` names.
#[cfg(feature = "baked")]
fn generic_definition(assembly: &Assembly, token: lamella_token::Token) -> Option<lamella_token::Token> {
    match assembly.type_spec_signature(token)? {
        SigType::GenericInst { definition, .. } => match *definition {
            SigType::Class(definition) | SigType::ValueType(definition) => Some(definition),
            _ => None,
        },
        _ => None,
    }
}

/// A type's name as a reader writes it: namespace, enclosing types and name, and for an
/// instantiation its generic definition's.
#[cfg(feature = "baked")]
fn type_name(assembly: &Assembly, token: lamella_token::Token) -> Option<String> {
    let token = if token.table() == table::TYPE_SPEC {
        generic_definition(assembly, token)?
    } else {
        token
    };
    let (namespace, name) = assembly.type_token_full_name(token)?;
    Some(if namespace.is_empty() {
        name
    } else {
        format!("{namespace}.{name}")
    })
}

/// A member reference as `Namespace.Type::Member`, the loader's own spelling for a member it could
/// not bind, so the two refusals a reader can meet name a member the same way.
#[cfg(feature = "baked")]
fn member_ref_name(assembly: &Assembly, member: &lamella_metadata::MemberRef<'_>) -> String {
    let name = member.name().unwrap_or("<unnamed>");
    match type_name(assembly, member.parent()) {
        Some(owner) => format!("{owner}::{name}"),
        None => name.to_owned(),
    }
}

/// The member a call token names, from the metadata of the assembly whose token space it is in.
///
/// `None` for a token that names no row there, so a refusal falls back to the token rather than
/// naming a member the program never called.
#[cfg(feature = "baked")]
fn member_name(assembly: &Assembly, token: lamella_token::Token) -> Option<String> {
    match token.table() {
        table::MEMBER_REF => Some(member_ref_name(assembly, &assembly.member_ref(token.row())?)),
        table::METHOD_DEF => assembly.type_defs().find_map(|type_def| {
            let method = type_def.methods().find(|method| method.token() == token)?;
            let owner = type_name(assembly, type_def.token())?;
            Some(format!("{owner}::{}", method.name()?))
        }),
        table::METHOD_SPEC => member_name(assembly, assembly.method_spec_method(token)?),
        _ => None,
    }
}

/// The method a violation is in, as `Namespace.Type::Method`, from the definition it was loaded
/// from; its recorded debug name where no definition answers, as for a lowered instantiation.
///
/// **THE DEFINITION FIRST, SO ONE LINE NAMES CALLER AND MEMBER IN ONE SPELLING.** A debug name is
/// written `Type.Method`, and a line reading `Program.Main calls System.Threading.Thread::Sleep`
/// spells one thing two ways.
#[cfg(feature = "baked")]
fn method_name(
    module: &lamella_cil_runtime::Module,
    method: lamella_cil_runtime::MethodId,
    recorded: Option<&str>,
    assembly: &Assembly,
    asm: u8,
) -> String {
    assembly
        .type_defs()
        .find_map(|type_def| {
            let defined = type_def
                .methods()
                .find(|candidate| module.resolve(asm, candidate.token()) == Some(method))?;
            let owner = type_name(assembly, type_def.token())?;
            Some(format!("{owner}::{}", defined.name()?))
        })
        .or_else(|| recorded.map(String::from))
        .unwrap_or_else(|| format!("method {method}"))
}

/// The assembly a violation's member is in, when the image does not carry it: a library the program
/// reaches through another library and was not itself compiled against.
///
/// `None` for any other violation, and for a member of an assembly the image carries, which is
/// missing for some other reason. `corlib` is the corlib the image carries, `None` for one that
/// carries none.
#[cfg(feature = "baked")]
fn uncarried_assembly(
    violation: &ProfileViolation,
    module: &lamella_cil_runtime::Module,
    assemblies: &[&Assembly],
    corlib: Option<&Assembly>,
) -> Option<String> {
    let ProfileViolation::UnresolvedCall { method, token, .. } = violation else {
        return None;
    };
    let assembly = assemblies.get(usize::from(module.method_asm(*method)))?;
    let token = lamella_token::Token(*token);
    if token.table() != table::MEMBER_REF {
        return None;
    }
    let library = scope_assembly(assembly, assembly.member_ref(token.row())?.parent())?;
    let carried = corlib.is_some_and(|corlib| names_the_corlib(&library, corlib))
        || assemblies.iter().any(|carried| carried.assembly_name() == Some(library.as_str()));
    (!carried).then_some(library)
}

/// One violation as a sentence naming the method and the member, never a bare token.
///
/// `assemblies` are the image's, indexed by assembly id.
#[cfg(feature = "baked")]
fn describe(
    violation: &ProfileViolation,
    module: &lamella_cil_runtime::Module,
    assemblies: &[&Assembly],
    corlib: Option<&Assembly>,
) -> String {
    let (method, recorded) = match violation {
        ProfileViolation::UnsupportedOpcode { method, name, .. }
        | ProfileViolation::UnresolvedCall { method, name, .. }
        | ProfileViolation::UnloweredGeneric { method, name, .. } => (*method, name.as_deref()),
    };
    let asm = module.method_asm(method);
    let assembly = assemblies.get(usize::from(asm)).copied();
    let caller = match assembly {
        Some(assembly) => method_name(module, method, recorded, assembly, asm),
        None => recorded.map_or_else(|| format!("method {method}"), String::from),
    };
    let member = |token: u32| {
        assembly
            .and_then(|assembly| member_name(assembly, lamella_token::Token(token)))
            .unwrap_or_else(|| format!("the member at token 0x{token:08X}"))
    };
    match violation {
        ProfileViolation::UnsupportedOpcode { opcode, .. } => {
            format!("{caller} uses `{opcode}`, which this build's runtime does not support")
        }
        ProfileViolation::UnresolvedCall { token, .. } => {
            match uncarried_assembly(violation, module, assemblies, corlib) {
                Some(library) => format!(
                    "{caller} calls {}, which is in {library}, an assembly the image does not carry",
                    member(*token)
                ),
                None => format!(
                    "{caller} calls {}, which nothing in the image provides",
                    member(*token)
                ),
            }
        }
        ProfileViolation::UnloweredGeneric { token, .. } => format!(
            "{caller} reaches {} through a generic instantiation the bake did not lower",
            member(*token)
        ),
    }
}

/// How many places a listing names before it says how many more there are. A program that misses
/// the tier badly misses it in hundreds of places, and a wall of them buries the first one -- which
/// is the one worth reading.
#[cfg(feature = "baked")]
const SHOWN: usize = 20;

/// Each place a baked image would stop, as a line naming the method it is in and the member it could
/// not reach: the first twenty, then how many more. Each line begins with a newline and four spaces,
/// to follow a sentence that ends in a colon.
///
/// `assemblies` are the image's, by assembly id. `corlib` is the corlib the image carries, `None` for
/// an image that carries none.
#[must_use]
#[cfg(feature = "baked")]
pub fn listed(
    violations: &[ProfileViolation],
    module: &lamella_cil_runtime::Module,
    assemblies: &[&Assembly],
    corlib: Option<&Assembly>,
) -> String {
    let mut text = String::new();
    for violation in violations.iter().take(SHOWN) {
        text.push_str(&format!("\n    {}", describe(violation, module, assemblies, corlib)));
    }
    if violations.len() > SHOWN {
        text.push_str(&format!("\n    ... and {} more", violations.len() - SHOWN));
    }
    text
}

/// The assemblies holding a member the program calls that the image does not carry, each once, in
/// the order the places name them. `assemblies` and `corlib` are as [`listed`] takes them.
#[must_use]
#[cfg(feature = "baked")]
pub fn uncarried_assemblies(
    violations: &[ProfileViolation],
    module: &lamella_cil_runtime::Module,
    assemblies: &[&Assembly],
    corlib: Option<&Assembly>,
) -> Vec<String> {
    let mut named = Vec::new();
    for violation in violations {
        if let Some(library) = uncarried_assembly(violation, module, assemblies, corlib)
            && !named.contains(&library)
        {
            named.push(library);
        }
    }
    named
}

/// Refuse a program that an image of it ALONE cannot run.
///
/// Such an image carries neither the corlib's managed code nor any library: the MCP server's device
/// tools and a baked link make one. It runs a call into the corlib the board's runtime provides, and
/// stops at any other, so a program calling into `System.Device.Gpio`, or into a corlib method with
/// a managed body, is refused here. The refusal names each place, the assemblies it is missing, and
/// the route that bakes the program with what it was compiled against.
///
/// # Errors
/// A program reaching a call such an image cannot resolve, or one that does not load.
#[cfg(feature = "baked")]
pub fn refuse_what_alone_cannot_run(program: &Assembly<'static>) -> Result<(), String> {
    let loaded =
        lamella_load::load_unfrozen(program).map_err(|error| format!("program load: {error}"))?;
    let (methods, _, _) = loaded.module.reachable_set(Some(loaded.entry));
    let violations = loaded.module.validate_profile(Some(&methods));
    if violations.is_empty() {
        return Ok(());
    }
    let assemblies = [program];
    let places = if violations.len() == 1 {
        String::from("one place")
    } else {
        format!("{} places", violations.len())
    };
    let missing = uncarried_assemblies(&violations, &loaded.module, &assemblies, None);
    let head = if missing.is_empty() {
        format!("no image written: the board would stop in {places}:")
    } else {
        format!(
            "no image written: the program calls into {}, which an image of the program alone does \
             not\ncarry, so the board would stop in {places}:",
            missing.join(", ")
        )
    };
    Err(format!(
        "{head}{}\n\n\
         This tool bakes the program alone. `lamella run <file> --device <device>` builds its image \
         with\nthe corlib and the libraries the program was compiled against, for the board that \
         answers, and runs\nit there.",
        listed(&violations, &loaded.module, &assemblies, None)
    ))
}

/// Bake `program` ALONE, into an image of its own code with nothing else beside it, after refusing
/// what such an image cannot run ([`refuse_what_alone_cannot_run`]). The one bake behind the MCP
/// server's device tools and a baked link.
///
/// # Errors
/// A program that does not parse or load, one such an image cannot run, or one that cannot be laid
/// out for flash.
#[cfg(feature = "baked")]
pub fn bake_alone(program: &'static [u8]) -> Result<Vec<u8>, String> {
    let assembly = Assembly::read(program)
        .map_err(|error| format!("the program does not parse: {error:?}"))?;
    refuse_what_alone_cannot_run(&assembly)?;
    let loaded = lamella_load::load(&assembly).map_err(|error| format!("program load: {error}"))?;
    let mut module = loaded.module;
    module.write_baked(Some(loaded.entry)).map_err(|error| format!("bake: {error:?}"))
}
