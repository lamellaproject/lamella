//! A minimal static linker over `lamella-elf` objects.

#![no_std]

extern crate alloc;

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::String;
use alloc::vec::Vec;

use lamella_elf::{Archive, Binding, Machine, Object, ParsedRelocation, SymbolType, arm, riscv};

/// A reason linking failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkError {
    /// A relocation references a symbol no input object defines.
    UndefinedSymbol(String),
    /// The named entry symbol is not defined by any input object.
    MissingEntry(String),
    /// Two input objects define the same global symbol.
    DuplicateSymbol(String),
    /// A relocation type the linker does not handle yet.
    UnsupportedRelocation(u32),
    /// A relocation's resolved offset does not fit its instruction encoding (an out-of-range call).
    RelocationOutOfRange(u32),
    /// No input objects were given, so the target machine is unknown.
    NoObjects,
    /// The input objects target different machines (an ARM object cannot link with a RISC-V one).
    MixedMachines,
    /// An absolute relocation (`R_ARM_ABS32`, `R_RISCV_32`) was found, but the link is base-agnostic
    /// (use [`link_at_base`], which knows the load address an absolute reference needs).
    AbsoluteNeedsBase,
    /// The managed statics regions (plus the EH word) need more RAM than the window allows --
    /// `needed` bytes against a `cap`-byte window. The default windows are deliberately small
    /// (they sit below the runtime-support worker-stack ladders); pass a bigger explicit window
    /// via [`link_with_archives_ram`] if the target's RAM plan has one.
    StaticsOverflow {
        /// The bytes the regions + EH word need together.
        needed: u32,
        /// The window's capacity in bytes.
        cap: u32,
    },
    /// THE IMAGE CARRIES MORE GENERIC INSTANTIATIONS THAN THE CODE MODEL BUDGETS FOR.
    ///
    /// The code model is *monomorphize value types, cap 7*, and **the cap means REFUSE, not
    /// degrade** -- past it there is no fallback, because the alternative (sharing, which boxes) is
    /// exactly what the RAM measurement disqualified. See [`INSTANTIATION_CAP`].
    InstantiationCapExceeded {
        /// How many distinct instantiations the image carries.
        count: usize,
        /// The budget.
        cap: usize,
        /// Their handles, ascending -- the identity a `__lamella_typedesc_<handle>` symbol carries.
        /// Handles rather than spellings because the linker reads symbols, not metadata; the AOT's
        /// descriptor dump maps one to the other.
        handles: Vec<u32>,
    },
}

/// Which producer would have defined a symbol the link could not resolve.
///
/// The backend emits three families of external name and defines none of them itself, so an
/// unresolved one is a statement about a MISSING INPUT rather than about the program. The families
/// are told apart by shape alone, which is all the linker has: it reads symbols, not metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UndefinedProvider {
    /// A runtime-support seam (`lamella_gc_alloc`, the soft-float builtins, the thread and string
    /// helpers): supplied by the runtime-support archive built for the target ISA.
    RuntimeSupport,
    /// A managed method reached across an assembly boundary, spelled
    /// `Namespace.Type.Method.<params>.<return>`: supplied by the assembly that declares the type,
    /// which has to be built as a library object and included in the link.
    ManagedAssembly,
    /// A name the linker itself defines in a later pass (the statics windows, the type descriptors,
    /// the EH tag). Undefined here means an internal defect rather than a missing input.
    LinkerDefined,
    /// A name in none of the shapes above.
    Unknown,
}

/// Classifies an unresolved name by the shape the backend emitted it under.
///
/// Deliberately shape-based and total: every name reaches an arm, and the one arm that admits it
/// cannot be wrong about the others. The internal library form (`L<hash>.f<rid>`, the duplicate-name
/// demotion) contains a dot and would otherwise read as a managed method, so it is separated before
/// the dot is consulted.
fn undefined_provider(name: &str) -> UndefinedProvider {
    if name.starts_with("__lamella_") {
        return UndefinedProvider::LinkerDefined;
    }
    if name.starts_with("lamella_") {
        return UndefinedProvider::RuntimeSupport;
    }
    let demoted = name.strip_prefix('L').is_some_and(|rest| {
        rest.split_once(".f")
            .is_some_and(|(hash, rid)| {
                !hash.is_empty()
                    && hash.bytes().all(|b| b.is_ascii_hexdigit())
                    && !rid.is_empty()
                    && rid.bytes().all(|b| b.is_ascii_digit())
            })
    });
    if demoted {
        return UndefinedProvider::Unknown;
    }
    if name.contains('.') {
        return UndefinedProvider::ManagedAssembly;
    }
    UndefinedProvider::Unknown
}

impl core::fmt::Display for LinkError {
    /// Renders the failure as a sentence naming a cause, because the alternative a caller reaches
    /// for is the derived `Debug`, which prints a mangled symbol and nothing a reader can act on.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            LinkError::UndefinedSymbol(name) => {
                write!(f, "nothing in this link defines `{name}`")?;
                match undefined_provider(name) {
                    UndefinedProvider::RuntimeSupport => write!(
                        f,
                        " -- a runtime-support seam. The compiler emits calls to it and the \
                         runtime-support archive for this target defines it, so a link given no \
                         such archive cannot resolve it. Include the archive built for this \
                         target's ISA. (`lamella_gc_alloc` is reached by any allocation at all: a \
                         `new`, a boxed value, an array, a string join.)"
                    ),
                    UndefinedProvider::ManagedAssembly => write!(
                        f,
                        " -- a managed method in another assembly. Referencing that assembly at \
                         COMPILE time lets the call bind, but the link needs its CODE too: build \
                         the assembly as a library object and include it here."
                    ),
                    UndefinedProvider::LinkerDefined => write!(
                        f,
                        " -- a name the linker defines itself in a later pass, so this is an \
                         internal defect rather than a missing input."
                    ),
                    UndefinedProvider::Unknown => Ok(()),
                }
            }
            LinkError::MissingEntry(name) => {
                write!(f, "no input object defines the entry symbol `{name}`")
            }
            LinkError::DuplicateSymbol(name) => {
                write!(f, "two input objects define `{name}`")
            }
            LinkError::UnsupportedRelocation(kind) => {
                write!(f, "relocation type {kind} is not handled by this linker")
            }
            LinkError::RelocationOutOfRange(kind) => write!(
                f,
                "a relocation of type {kind} resolved too far to fit its instruction encoding"
            ),
            LinkError::NoObjects => {
                write!(f, "no input objects were given, so the target machine is unknown")
            }
            LinkError::MixedMachines => {
                write!(f, "the input objects target different machines")
            }
            LinkError::AbsoluteNeedsBase => write!(
                f,
                "an absolute relocation needs the address the image will be placed at, but this \
                 link is base-agnostic"
            ),
            LinkError::StaticsOverflow { needed, cap } => write!(
                f,
                "the managed statics need {needed} bytes against a {cap}-byte window"
            ),
            LinkError::InstantiationCapExceeded { count, cap, handles } => write!(
                f,
                "the image carries {count} generic instantiations against a cap of {cap} \
                 (handles: {handles:?})"
            ),
        }
    }
}

/// **THE INSTANTIATION BUDGET, AND IT IS WHOLE-IMAGE.** Not per definition: the flash and RAM the
/// spike measured are properties of the image, so eight instantiations of one definition and one
/// each of eight definitions cost the same and are budgeted the same.
///
/// 7, from the spike (`(M-S)(N) = 1,625.9*N - 4,983` over the value-type segment): monomorphizing
/// is a net saving below N = 3.06 and first costs 5% of a 128 KB slot at N = 7.10.
///
/// It is a constant in this crate rather than a capability-profile knob: the profile does not reach
/// the linker, and a number carried somewhere nothing enforces it is worth less than one in the
/// wrong file.
pub const INSTANTIATION_CAP: usize = 7;

/// On ARM, a Thumb function symbol carries the Thumb state in its value's low bit (`answer` =
/// `offset | 1`). The linker normalizes to the even byte offset for layout + reach math (BL keeps a
/// halfword-even target); the Thumb bit is re-applied only to a Thumb executable's `e_entry`. On
/// other machines, and for non-ARM, the value passes through. (Mixed ARM/Thumb interworking, which
/// would need the bit to choose BL vs BLX, is out of scope -- the Lamella backend and `-mthumb` C
/// both produce Thumb.)
fn normalized_value(machine: Machine, value: u32) -> u32 {
    match machine {
        Machine::Arm => value & !1,
        Machine::RiscV => value,
    }
}

/// A linked image: the laid-out, relocated code plus where execution starts.
#[derive(Debug, Clone)]
pub struct LinkedImage {
    /// The combined, relocated `.text` (position-independent).
    pub text: Vec<u8>,
    /// The byte offset of the entry symbol within [`LinkedImage::text`].
    pub entry_offset: u32,
    /// Every defined symbol, as `(name, offset within text)`.
    pub symbols: Vec<(String, u32)>,
    /// The combined, relocated NON-ALLOCATED sections, as `(name, bytes)` in first-seen order --
    /// today the DWARF `.debug_*` family. Each input object's same-named contributions are
    /// concatenated (at their required alignment) and relocated in place, so a debugger reading the
    /// result sees one coherent set of debug sections for the whole image.
    ///
    /// Empty unless some input object carried debug info, so an ordinary image is untouched. These
    /// bytes are NOT part of [`Self::text`]: they occupy no target memory and are never loaded --
    /// they belong in the debug artifact a host-side debugger opens, alongside the flashed image.
    pub debug_sections: Vec<(String, Vec<u8>)>,
}

/// Links `objects` into one image, with `entry` naming the start symbol. Each object's `.text` is
/// laid out in order (4-byte aligned), every relocation's symbol is resolved to its definition, and
/// the code is patched in place.
pub fn link(objects: &[Object], entry: &str) -> Result<LinkedImage, LinkError> {
    link_with_base(objects, entry, None, &[])
}

/// Like [`link`], but with the virtual address `text_base` at which the linked `.text` will be
/// placed -- so ABSOLUTE relocations (`R_ARM_ABS32`, `R_RISCV_32`, e.g. a function pointer) resolve
/// to real addresses, not image offsets. PC-relative relocations are unaffected (they ignore the
/// base), so the result is correct only at `text_base`. For a hosted ELF from
/// [`lamella_elf::write_executable`], `text_base` is the load base plus the header size
/// ([`lamella_elf::EXEC_TEXT_OFFSET`]). Base-agnostic [`link`] rejects an absolute relocation.
pub fn link_at_base(
    objects: &[Object],
    entry: &str,
    text_base: u32,
) -> Result<LinkedImage, LinkError> {
    link_with_base(objects, entry, Some(text_base), &[])
}

/// Like [`link_at_base`], but also resolving `residents` -- `(name, absolute address)` of functions
/// already present on the target (the on-board runtime) -- so the injected object's calls and data refs
/// to those names land on their real addresses. This is the RAM-injection delivery (REPL-against-AOT):
/// the host links a snippet to the RAM buffer it will write, with
/// the resident runtime's seams (`lamella_gc_alloc`, ...) resolved to the addresses it read from the
/// board. The result runs correctly only when loaded at `text_base`. A resident in flash, beyond a Thumb
/// `BL`'s +/-16 MB reach from a RAM buffer, is bridged automatically by an appended long-branch veneer.
pub fn link_at_base_with_residents(
    objects: &[Object],
    entry: &str,
    text_base: u32,
    residents: &[(&str, u32)],
) -> Result<LinkedImage, LinkError> {
    link_with_base(objects, entry, Some(text_base), residents)
}

/// As [`link_at_base`], but first runs function-level [`garbage_collect`] from `entry` -- dropping unused
/// code so its undefined externs fall out and the image shrinks (e.g. linking a program against a whole
/// corlib pulls only the reached methods).
pub fn link_at_base_gc(
    objects: &[Object],
    entry: &str,
    text_base: u32,
) -> Result<LinkedImage, LinkError> {
    link_gc_inner(objects, entry, false, Some(text_base))
}

/// Re-exported from [`lamella_elf`] so the backend that NAMES descriptor symbols and the linker that
/// COLLECTS them here share ONE prefix. A descriptor unreachable from the entry is dropped (so its vtable
/// relocations cannot pin an otherwise-trimmed method); other data is retained wholesale.
pub use lamella_elf::TYPE_DESC_PREFIX;

/// Re-exported stack-map names (see [`lamella_elf`]): the backend NAMES record symbols with the
/// prefixes; this linker applies the record keep-rule by them and defines the start/end symbols
/// around the gathered pointer table.
pub use lamella_elf::{
    STACKMAP_END_SYMBOL, STACKMAP_RECORD_PREFIX, STACKMAP_START_SYMBOL, STACKMAP_STATICS_PREFIX,
};

/// Re-exported return-address stack-map names (see [`lamella_elf`]): the backend contributes
/// per-function fragments in [`STACKMAP_GCMAP_SECTION`]; this linker synthesizes
/// [`STACKMAP_BLOB_SYMBOL`] from the fragments whose function survived, and defines
/// [`TEXT_BASE_SYMBOL`] at the image's `.text` start so a collector can turn a runtime return
/// address into a lookup key.
pub use lamella_elf::{STACKMAP_BLOB_SYMBOL, STACKMAP_GCMAP_SECTION, TEXT_BASE_SYMBOL};

/// Re-exported statics-layout names (see [`lamella_elf`]): the backend references regions and the
/// EH word by them; this linker lays the regions out in a RAM window, defines every symbol, and
/// brackets the span for a boot stub's zero loop.
pub use lamella_elf::{
    EH_TAG_SYMBOL, STATICS_BASE_PREFIX, STATICS_END_SYMBOL, STATICS_START_SYMBOL,
};

/// Every generic instantiation the given objects CARRY, by handle, ascending.
///
/// **THE COUNT IS OF WHAT THE IMAGE CONTAINS, WHICH IS WHY IT IS TAKEN HERE AND NOT IN THE AOT.**
/// A compile-time count -- the collector's closed set -- is an UPPER BOUND, and measured on a
/// program that monomorphizes it overstates by 4x: eight instantiations declared, two reached, six
/// dead-stripped to nothing. Budgeting against the compile-time number would refuse programs that
/// pay for two, and an ordinary program writing `new List<T>()` eight times closes over 104
/// instantiations while linking a handful.
///
/// Asking it of the objects has the property that makes it correct in both directions: a caller who
/// dead-stripped first is counted on the survivors, and one who did not is counted on everything --
/// which is right, because that caller's image really does carry everything. There is no phase to
/// get wrong, because the question is about bytes rather than about intent.
///
/// An instantiation is SELF-IDENTIFYING in a symbol table: its descriptor is
/// `__lamella_typedesc_<handle>` and its handle's table byte is
/// [`lamella_ir::INSTANTIATION_HANDLE_TABLE`], which no ordinary type's handle carries. So this
/// needs no manifest from the build and cannot drift from one.
///
/// **SCOPE, STATED BECAUSE A SILENT BOUND READS AS COVERAGE:** an instantiation with no
/// descriptor is not counted. That is every INTERFACE instantiation -- around half the closed set
/// of a real program -- which costs a tag and an itable entry and NO body. Whether the budget the
/// spike calibrated (1,694 B per instantiation, measured on body-bearing value instantiations)
/// should charge for those has not been measured, so they are excluded rather than guessed at.
#[must_use]
pub fn image_instantiations(objects: &[Object]) -> Vec<u32> {
    let mut handles: BTreeSet<u32> = BTreeSet::new();
    for obj in objects {
        for s in &obj.symbols {
            if !s.defined {
                continue;
            }
            let Some(digits) = s.name.strip_prefix(lamella_elf::TYPE_DESC_PREFIX) else {
                continue;
            };
            let Ok(handle) = digits.parse::<u32>() else {
                continue;
            };
            if handle >> 24 == lamella_ir::INSTANTIATION_HANDLE_TABLE {
                handles.insert(handle);
            }
        }
    }
    handles.into_iter().collect()
}

/// Refuses an image carrying more than `cap` generic instantiations. See [`image_instantiations`]
/// for what is counted and [`INSTANTIATION_CAP`] for the budget.
///
/// **REFUSE, NOT DEGRADE.** Past the cap there is no fallback to switch to: the spike disqualified
/// sharing on RAM (shared boxing crosses the 8 KB arena between N = 4 and N = 6), so a program over
/// budget has no second code model waiting for it. A build that cannot fit its instantiations has to
/// say so.
pub fn check_instantiation_cap(objects: &[Object], cap: usize) -> Result<(), LinkError> {
    let handles = image_instantiations(objects);
    if handles.len() > cap {
        return Err(LinkError::InstantiationCapExceeded {
            count: handles.len(),
            cap,
            handles,
        });
    }
    Ok(())
}

/// Function-level `--gc-sections`: builds the cross-object reference graph from `entry` -- following each
/// reached symbol's relocations, a function's calls AND a data symbol's references (e.g. a type
/// descriptor's vtable entries and base pointer) -- keeps the reachable functions, reachable descriptors,
/// and all other data, then rebuilds each object re-laid-out with its symbols and relocations remapped, so
/// unused functions/descriptors and the undefined externs only they referenced drop out.
pub fn garbage_collect(objects: &[Object], entry: &str) -> Vec<Object> {
    trim_all(objects, &reachable_from(objects, entry))
}

/// Rebuilds every object against `keep`, EXCEPT the ones [`resolves_against_its_own_layout`] refuses
/// to move, which are passed through whole.
fn trim_all(objects: &[Object], keep: &BTreeSet<String>) -> Vec<Object> {
    objects
        .iter()
        .map(|obj| match resolves_against_its_own_layout(obj) {
            true => obj.clone(),
            false => trim_object(obj, keep),
        })
        .collect()
}

/// Whether any of `obj`'s code relocations names a target the trim CANNOT PUT BACK: one that is
/// defined here and either has no name at all (a section symbol) or has no `st_size`.
///
/// **SUCH AN OBJECT IS KEPT WHOLE, BECAUSE A SYMBOL-GRANULARITY TRIM CANNOT RE-ADDRESS IT.**
/// [`trim_object`] rebuilds an object by copying each kept symbol's `[st_value, st_value + st_size)`
/// and re-pointing relocations AT THE SYMBOL BY NAME, so a target survives the move when it has a
/// name to be found by and a size to be copied. Neither of these has both:
///
///   * a SECTION symbol carries no name, and its relocation's offset into the section rides in the
///     ADDEND -- which on ARM is IMPLICIT, i.e. the instruction's own immediate field, so there is
///     not even an addend to rewrite. The addend also addresses the whole section rather than the
///     target's own bytes, so no per-symbol remap could be right.
///   * a size-0 definition is dropped by the trim whatever reaches it (see [`trim_object`]).
///
/// A real linker has the same limit and answers it the same way: `--gc-sections` works at SECTION
/// granularity and never splits a section it cannot re-address.
///
/// **A NAMED, SIZED LOCAL IS NOT HERE, AND THE OMISSION IS DELIBERATE.** It moves correctly: the
/// walk roots it (a relocation target's name is pushed whatever its binding), the trim keeps it, and
/// the relocation is re-pointed at its new position with the addend still measured from the symbol's
/// own base. Including it would keep whole every archive member carrying an internal label, for no
/// gain.
///
/// The objects this selects are compiled ones that address their own literal pools this way; a
/// symbol table the backend writes itself names and sizes every relocation target, so a program
/// object and a corlib object are both unaffected.
fn resolves_against_its_own_layout(obj: &Object) -> bool {
    obj.relocations.iter().any(|r| {
        obj.symbols
            .get(r.symbol as usize)
            .is_some_and(|s| s.defined && (s.name.is_empty() || s.size == 0))
    })
}

/// The names [`garbage_collect`] keeps: everything the cross-object reference graph reaches from
/// `entry`, plus everything [`kept_regardless`] keeps without being reached.
///
/// **EXTRACTED SO THE FOLD PATH ASKS THE SAME QUESTION RATHER THAN ANSWERING IT AGAIN.**
/// [`link_gc_inner`] needs the reachable set MINUS the functions ICF folds away, which it cannot get
/// from [`garbage_collect`]'s objects-in/objects-out shape; before this split it walked a graph of
/// its own that followed a function's calls and NOTHING a data symbol references, so every
/// descriptor, string blob and statics record fell out of the image and the link died on the first
/// reference to one.
fn reachable_from(objects: &[Object], entry: &str) -> BTreeSet<String> {
    let mut defs: BTreeMap<&str, Vec<(usize, usize)>> = BTreeMap::new();
    for (oi, obj) in objects.iter().enumerate() {
        for (si, s) in obj.symbols.iter().enumerate() {
            if s.defined && !s.name.is_empty() {
                defs.entry(s.name.as_str()).or_default().push((oi, si));
            }
        }
    }
    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    stack.push(String::from(entry));
    for obj in objects {
        for s in &obj.symbols {
            if s.defined && s.size > 0 && !s.name.is_empty() && kept_regardless(s) {
                stack.push(s.name.clone());
            }
        }
    }
    for obj in objects.iter().filter(|o| resolves_against_its_own_layout(o)) {
        for s in &obj.symbols {
            if s.defined && !s.name.is_empty() {
                stack.push(s.name.clone());
            }
        }
    }
    while let Some(name) = stack.pop() {
        if !reachable.insert(name.clone()) {
            continue;
        }
        let Some(sites) = defs.get(name.as_str()) else {
            continue;
        };
        for &(oi, si) in sites {
            let obj = &objects[oi];
            let sym = &obj.symbols[si];
            let start = sym.value & !1;
            let end = start + sym.size;
            for r in &obj.relocations {
                if r.offset >= start && r.offset < end {
                    if let Some(target) = obj.symbols.get(r.symbol as usize) {
                        stack.push(target.name.clone());
                    }
                }
            }
        }
    }
    reachable
}

/// Whether [`trim_object`] keeps this symbol WITHOUT asking whether anything reaches it.
///
/// **ANYTHING KEPT REGARDLESS MUST ALSO BE A REACHABILITY ROOT, AND THAT IS WHY THIS IS ONE
/// FUNCTION RATHER THAN A CONDITION WRITTEN TWICE.** A symbol the trim keeps carries its
/// relocations with it; if the walk never followed them, their targets can be dropped and the kept
/// symbol is left pointing at nothing -- an undefined-symbol link error whose cause is two passes
/// disagreeing about the same rule.
///
/// **This bug class has now appeared three times in this one function**: once for type descriptors
/// (fixed by excluding them here), once for stack-map records (fixed by tying them to their
/// function), and once for a string literal blob, whose relocation to `System.String`'s descriptor
/// was never followed because the blob was KEPT without ever being REACHED. The first two were fixed
/// by narrowing what is kept. The general statement is the other way round: **keep and root are the
/// same set**, so [`garbage_collect`] seeds the walk from this predicate instead of restating it.
fn kept_regardless(s: &lamella_elf::ParsedSymbol) -> bool {
    if s.name.starts_with(STACKMAP_RECORD_PREFIX) {
        return false;
    }
    s.kind != SymbolType::Func && !s.name.starts_with(TYPE_DESC_PREFIX)
}

/// Rebuilds `obj` keeping only reachable functions and reachable descriptors (plus all other data),
/// re-laid-out. A referenced symbol not among the kept ones stays an undefined extern (the linker resolves
/// it, or errors if genuinely missing).
fn trim_object(obj: &Object, reachable: &BTreeSet<String>) -> Object {
    let mut kept: Vec<usize> = (0..obj.symbols.len())
        .filter(|&i| {
            let s = &obj.symbols[i];
            if !s.defined || s.size == 0 || s.name.is_empty() {
                return false;
            }
            if let Some(func) = s.name.strip_prefix(STACKMAP_RECORD_PREFIX) {
                return reachable.contains(func);
            }
            reachable.contains(&s.name) || kept_regardless(s)
        })
        .collect();
    kept.sort_by_key(|&i| obj.symbols[i].value & !1);

    let mut text: Vec<u8> = Vec::new();
    let mut ranges: Vec<(u32, u32, u32)> = Vec::new();
    let mut symbols: Vec<lamella_elf::ParsedSymbol> = Vec::new();
    symbols.push(lamella_elf::ParsedSymbol {
        name: String::new(),
        value: 0,
        size: 0,
        binding: Binding::Local,
        kind: SymbolType::NoType,
        defined: false,
        section: None,
    });
    let mut index_of: BTreeMap<String, u32> = BTreeMap::new();
    let mut debug_index_of: BTreeMap<(String, u32), u32> = BTreeMap::new();
    for &i in &kept {
        let s = &obj.symbols[i];
        let start = s.value & !1;
        let end = start + s.size;
        while text.len() % 4 != 0 {
            text.push(0);
        }
        let new_start = text.len() as u32;
        if let Some(slice) = obj.text.get(start as usize..end as usize) {
            text.extend_from_slice(slice);
        }
        ranges.push((start, end, new_start));
        index_of.insert(s.name.clone(), symbols.len() as u32);
        symbols.push(lamella_elf::ParsedSymbol {
            name: s.name.clone(),
            value: new_start | (s.value & 1),
            size: s.size,
            binding: s.binding,
            kind: s.kind,
            defined: true,
            section: None,
        });
    }

    let mut relocations: Vec<ParsedRelocation> = Vec::new();
    for r in &obj.relocations {
        let Some(&(old_start, _, new_start)) = ranges
            .iter()
            .filter(|(a, b, _)| r.offset >= *a && r.offset < *b)
            .min_by_key(|(a, b, _)| b - a)
        else {
            continue;
        };
        let Some(target) = obj.symbols.get(r.symbol as usize) else {
            continue;
        };
        let idx = match index_of.get(target.name.as_str()) {
            Some(&i) => i,
            None => {
                let i = symbols.len() as u32;
                symbols.push(lamella_elf::ParsedSymbol {
                    name: target.name.clone(),
                    value: 0,
                    size: target.size,
                    binding: Binding::Global,
                    kind: target.kind,
                    defined: false,
                    section: None,
                });
                index_of.insert(target.name.clone(), i);
                i
            }
        };
        relocations.push(ParsedRelocation {
            offset: new_start + (r.offset - old_start),
            symbol: idx,
            kind: r.kind,
            addend: r.addend,
            implicit_addend: r.implicit_addend,
        });
    }

    let mut sections = obj.sections.clone();
    for sec in &mut sections {
        let mut kept_relocs: Vec<ParsedRelocation> = Vec::new();
        for r in &sec.relocations {
            let Some(target) = obj.symbols.get(r.symbol as usize) else {
                continue;
            };
            let remapped = match target.section {
                Some(_) => Some((target.value, target.section)),
                None if target.defined => remap_text_offset(&ranges, target.value & !1)
                    .map(|v| (v | (target.value & 1), None)),
                None => Some((target.value, None)),
            };
            let Some((value, section)) = remapped else {
                if let Some(slot) = sec
                    .data
                    .get_mut(r.offset as usize..r.offset as usize + 4)
                {
                    slot.fill(0);
                }
                continue;
            };
            let key = (target.name.clone(), r.symbol);
            let idx = match debug_index_of.get(&key) {
                Some(&i) => i,
                None => {
                    let i = symbols.len() as u32;
                    symbols.push(lamella_elf::ParsedSymbol {
                        name: target.name.clone(),
                        value,
                        size: target.size,
                        binding: match target.defined {
                            true => Binding::Local,
                            false => Binding::Global,
                        },
                        kind: target.kind,
                        defined: target.defined,
                        section,
                    });
                    debug_index_of.insert(key, i);
                    i
                }
            };
            kept_relocs.push(ParsedRelocation {
                offset: r.offset,
                symbol: idx,
                kind: r.kind,
                addend: r.addend,
                implicit_addend: r.implicit_addend,
            });
        }
        sec.relocations = kept_relocs;
    }

    Object {
        machine: obj.machine,
        text,
        text_align: obj.text_align.max(4),
        symbols,
        relocations,
        sections,
    }
}

/// Maps a pre-trim `.text` offset to its post-trim one, or `None` if the byte was dropped. `ranges`
/// holds `(old_start, old_end, new_start)` per kept symbol; the SMALLEST covering range wins, for
/// the same reason it does when relocations are remapped -- an enclosing symbol must not capture an
/// offset that belongs to an inner one.
fn remap_text_offset(ranges: &[(u32, u32, u32)], old: u32) -> Option<u32> {
    ranges
        .iter()
        .filter(|(a, b, _)| old >= *a && old < *b)
        .min_by_key(|(a, b, _)| b - a)
        .map(|(a, _, new_start)| new_start + (old - a))
}

fn link_with_base(
    objects: &[Object],
    entry: &str,
    text_base: Option<u32>,
    residents: &[(&str, u32)],
) -> Result<LinkedImage, LinkError> {
    link_with_base_ram(objects, entry, text_base, residents, None)
}

fn link_with_base_ram(
    objects: &[Object],
    entry: &str,
    text_base: Option<u32>,
    residents: &[(&str, u32)],
    ram: Option<(u32, u32)>,
) -> Result<LinkedImage, LinkError> {
    check_instantiation_cap(objects, INSTANTIATION_CAP)?;
    let synthetic: Vec<Object> = stackmap_table_object(objects)
        .into_iter()
        .chain(gcmap_blob_object(objects))
        .collect();
    if !synthetic.is_empty() {
        let mut with_synthetic: Vec<Object> = objects.to_vec();
        with_synthetic.extend(synthetic);
        return link_with_base_inner(&with_synthetic, entry, text_base, residents, ram);
    }
    link_with_base_inner(objects, entry, text_base, residents, ram)
}

/// One function's contribution to the return-address stack map, as the backend's `.lamella_gcmap`
/// fragment carries it: the owning function's symbol name, and each safepoint as its return address
/// RELATIVE TO that function plus the entry's opaque tail.
///
/// The tail is never interpreted here. `lamella-elf`'s section note says why: one encoder writes an
/// entry's shape and one collector reads it, and a linker that parsed it would be a third party to
/// drift from.
#[derive(Debug, Clone)]
struct GcMapFragment {
    function: String,
    entries: Vec<(u32, Vec<u8>)>,
}

/// Reads a `u32` at `at`, or `None` past the end.
fn rd32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// Every `.lamella_gcmap` fragment in `objects`, in object order. A malformed or truncated fragment
/// ENDS that section's parse rather than being skipped: the format is self-delimiting, so a bad
/// length means every following offset is meaningless, and silently resyncing would emit a map with
/// plausible entries at wrong addresses.
fn gcmap_fragments(objects: &[Object]) -> Vec<GcMapFragment> {
    let mut out = Vec::new();
    for obj in objects {
        for sec in obj
            .sections
            .iter()
            .filter(|s| s.name == STACKMAP_GCMAP_SECTION)
        {
            let data = &sec.data;
            let mut at = 0usize;
            while at < data.len() {
                let Some(name_len) = rd32(data, at) else { break };
                let name_at = at + 4;
                let Some(name) = data
                    .get(name_at..name_at + name_len as usize)
                    .and_then(|b| core::str::from_utf8(b).ok())
                else {
                    break;
                };
                at = (name_at + name_len as usize).next_multiple_of(4);
                let Some(count) = rd32(data, at) else { break };
                at += 4;
                let mut entries = Vec::with_capacity(count as usize);
                let mut truncated = false;
                for _ in 0..count {
                    let (Some(rel_pc), Some(tail_len)) = (rd32(data, at), rd32(data, at + 4)) else {
                        truncated = true;
                        break;
                    };
                    let tail_at = at + 8;
                    let Some(tail) = data.get(tail_at..tail_at + tail_len as usize) else {
                        truncated = true;
                        break;
                    };
                    entries.push((rel_pc, tail.to_vec()));
                    at = (tail_at + tail_len as usize).next_multiple_of(4);
                }
                out.push(GcMapFragment {
                    function: String::from(name),
                    entries,
                });
                if truncated {
                    break;
                }
            }
        }
    }
    out
}

/// The set of function names a fragment can still resolve against -- every non-local symbol DEFINED
/// somewhere in `objects`. A dead-stripped function leaves no definition, which is exactly how a
/// fragment for it is dropped: no keep-rule, no name convention, just the absence of a symbol.
fn defined_names(objects: &[Object]) -> BTreeSet<String> {
    objects
        .iter()
        .flat_map(|o| &o.symbols)
        .filter(|s| s.defined && !s.name.is_empty() && s.binding != Binding::Local)
        .map(|s| s.name.clone())
        .collect()
}

/// The synthetic object RESERVING the return-address stack map, or `None` when no object carries
/// fragments. The bytes are zero here and written by `fill_gcmap_blob` once layout has fixed every
/// address; only the SIZE has to be right this early, and it is knowable because which functions
/// survive is a question about symbol existence rather than about addresses.
fn gcmap_blob_object(objects: &[Object]) -> Option<Object> {
    let machine = objects.first()?.machine;
    let fragments = gcmap_fragments(objects);
    if fragments.is_empty() {
        return None;
    }
    let live = defined_names(objects);
    let size: usize = 4 + fragments
        .iter()
        .filter(|f| live.contains(&f.function))
        .flat_map(|f| &f.entries)
        .map(|(_, tail)| 4 + tail.len())
        .sum::<usize>();
    Some(Object {
        machine,
        text: alloc::vec![0u8; size],
        text_align: 4,
        symbols: alloc::vec![
            lamella_elf::ParsedSymbol {
                name: String::new(),
                value: 0,
                size: 0,
                binding: Binding::Local,
                kind: SymbolType::NoType,
                defined: false,
                section: None,
            },
            lamella_elf::ParsedSymbol {
                name: String::from(STACKMAP_BLOB_SYMBOL),
                value: 0,
                size: size as u32,
                binding: Binding::Global,
                kind: SymbolType::NoType,
                defined: true,
                section: None,
            },
        ],
        relocations: Vec::new(),
        sections: Vec::new(),
    })
}

/// Writes the return-address stack map into the space [`gcmap_blob_object`] reserved: each surviving
/// safepoint as `u32 key` (the function's linked image offset plus the fragment's relative pc, which
/// is exactly `runtime_return_addr - __lamella_text_base`) followed by its tail verbatim, sorted by
/// key for the collector's binary search, behind a `u32 count`.
///
/// The result is byte-for-byte the format the GC ABI defines and
/// `arm32::StackMaps::encode` still writes on the flat path. What moved is WHERE it is built, and
/// therefore whether the addresses in it are true after a dead-strip.
fn fill_gcmap_blob(text: &mut [u8], objects: &[Object], defined: &[Defined]) {
    let Some((blob_at, _)) = resolve_sym(defined, STACKMAP_BLOB_SYMBOL) else {
        return;
    };
    let live = defined_names(objects);
    let mut rows: Vec<(u32, Vec<u8>)> = Vec::new();
    for fragment in gcmap_fragments(objects) {
        if !live.contains(&fragment.function) {
            continue;
        }
        let Some((func_at, _)) = resolve_sym(defined, &fragment.function) else {
            continue;
        };
        for (rel_pc, tail) in fragment.entries {
            rows.push((func_at + rel_pc, tail));
        }
    }
    rows.sort_by_key(|(key, _)| *key);
    let mut out = Vec::with_capacity(4 + rows.iter().map(|(_, t)| 4 + t.len()).sum::<usize>());
    out.extend_from_slice(&(rows.len() as u32).to_le_bytes());
    for (key, tail) in &rows {
        out.extend_from_slice(&key.to_le_bytes());
        out.extend_from_slice(tail);
    }
    let at = blob_at as usize;
    if let Some(slot) = text.get_mut(at..at + out.len()) {
        slot.copy_from_slice(&out);
    }
}

/// The stack-map pointer-table object over every record symbol in `objects` (see
/// [`link_with_base`]), or `None` when no object carries one. Weak duplicate definitions of a
/// record name resolve to one address at the link, so each name is tabled once.
fn stackmap_table_object(objects: &[Object]) -> Option<Object> {
    let machine = objects.first()?.machine;
    let mut names: Vec<String> = objects
        .iter()
        .flat_map(|o| &o.symbols)
        .filter(|s| {
            s.defined
                && (s.name.starts_with(STACKMAP_RECORD_PREFIX)
                    || s.name.starts_with(STACKMAP_STATICS_PREFIX))
        })
        .map(|s| s.name.clone())
        .collect();
    if names.is_empty() {
        return None;
    }
    names.sort();
    names.dedup();
    let mut text = Vec::with_capacity(4 + 4 * names.len());
    text.extend_from_slice(&(names.len() as u32).to_le_bytes());
    text.resize(4 + 4 * names.len(), 0);
    let abs32 = match machine {
        Machine::Arm => arm::R_ARM_ABS32,
        Machine::RiscV => riscv::R_RISCV_32,
    };
    let mut symbols: Vec<lamella_elf::ParsedSymbol> = Vec::new();
    symbols.push(lamella_elf::ParsedSymbol {
        name: String::new(),
        value: 0,
        size: 0,
        binding: Binding::Local,
        kind: SymbolType::NoType,
        defined: false,
        section: None,
    });
    symbols.push(lamella_elf::ParsedSymbol {
        name: String::from(STACKMAP_START_SYMBOL),
        value: 0,
        size: text.len() as u32,
        binding: Binding::Global,
        kind: SymbolType::NoType,
        defined: true,
        section: None,
    });
    symbols.push(lamella_elf::ParsedSymbol {
        name: String::from(STACKMAP_END_SYMBOL),
        value: text.len() as u32,
        size: 0,
        binding: Binding::Global,
        kind: SymbolType::NoType,
        defined: true,
        section: None,
    });
    let mut relocations: Vec<ParsedRelocation> = Vec::with_capacity(names.len());
    for (i, name) in names.into_iter().enumerate() {
        relocations.push(ParsedRelocation {
            offset: 4 + 4 * i as u32,
            symbol: symbols.len() as u32,
            kind: abs32,
            addend: 0,
            implicit_addend: false,
        });
        symbols.push(lamella_elf::ParsedSymbol {
            name,
            value: 0,
            size: 0,
            binding: Binding::Global,
            kind: SymbolType::NoType,
            defined: false,
            section: None,
        });
    }
    Some(Object {
        machine,
        text,
        text_align: 4,
        symbols,
        relocations,
        sections: Vec::new(),
    })
}

fn link_with_base_inner(
    objects: &[Object],
    entry: &str,
    text_base: Option<u32>,
    residents: &[(&str, u32)],
    ram: Option<(u32, u32)>,
) -> Result<LinkedImage, LinkError> {
    let machine = link_machine(objects)?;

    let mut text: Vec<u8> = Vec::new();
    let mut bases: Vec<u32> = Vec::with_capacity(objects.len());
    for obj in objects {
        align_to(&mut text, obj.text_align);
        bases.push(text.len() as u32);
        text.extend_from_slice(&obj.text);
    }

    let mut defined: Vec<Defined> = Vec::new();
    let mut strong: BTreeSet<String> = BTreeSet::new();
    for (oi, obj) in objects.iter().enumerate() {
        for sym in &obj.symbols {
            if !sym.defined || sym.name.is_empty() || sym.binding == Binding::Local {
                continue;
            }
            let (value, thumb) = symbol_target(machine, sym);
            let addr = bases[oi] + value;
            match defined.iter().position(|(n, _, _)| *n == sym.name) {
                Some(pos) => {
                    if sym.binding == Binding::Global {
                        if strong.contains(&sym.name) {
                            return Err(LinkError::DuplicateSymbol(sym.name.clone()));
                        }
                        defined[pos] = (sym.name.clone(), addr, thumb);
                        strong.insert(sym.name.clone());
                    }
                }
                None => {
                    defined.push((sym.name.clone(), addr, thumb));
                    if sym.binding == Binding::Global {
                        strong.insert(sym.name.clone());
                    }
                }
            }
        }
    }

    if objects
        .iter()
        .any(|o| o.sections.iter().any(|s| s.name == STACKMAP_GCMAP_SECTION))
        && !defined.iter().any(|(n, _, _)| n == TEXT_BASE_SYMBOL)
    {
        defined.push((String::from(TEXT_BASE_SYMBOL), 0, false));
    }

    if let Some(base) = text_base {
        for &(name, addr) in residents {
            if defined.iter().any(|(n, _, _)| n == name) {
                return Err(LinkError::DuplicateSymbol(String::from(name)));
            }
            defined.push((
                String::from(name),
                normalized_value(machine, addr).wrapping_sub(base),
                is_thumb_func(machine, addr),
            ));
        }
    }

    let mut regions: Vec<(String, u32)> = Vec::new();
    let mut eh_referenced = false;
    let mut brackets_referenced = false;
    for obj in objects {
        for s in &obj.symbols {
            if s.defined || s.name.is_empty() {
                continue;
            }
            if statics_region_suffix(&s.name).is_some() {
                match regions.iter_mut().find(|(n, _)| *n == s.name) {
                    Some(r) => r.1 = r.1.max(s.size),
                    None => regions.push((s.name.clone(), s.size)),
                }
            } else if s.name == EH_TAG_SYMBOL {
                eh_referenced = true;
            } else if s.name == STATICS_START_SYMBOL || s.name == STATICS_END_SYMBOL {
                brackets_referenced = true;
            }
        }
    }
    regions.retain(|(n, _)| resolve(&defined, n).is_none());
    if !regions.is_empty() || eh_referenced || brackets_referenced {
        if let Some(entry_region) = objects
            .iter()
            .find(|o| o.symbols.iter().any(|s| s.defined && s.name == entry))
            .and_then(|o| {
                o.symbols
                    .iter()
                    .find(|s| !s.defined && statics_region_suffix(&s.name).is_some())
                    .map(|s| s.name.clone())
            })
        {
            if let Some(pos) = regions.iter().position(|(n, _)| *n == entry_region) {
                let lead = regions.remove(pos);
                regions.insert(0, lead);
            }
        }
        let base = text_base.ok_or(LinkError::AbsoluteNeedsBase)?;
        let (ram_base, ram_cap) = ram.unwrap_or(match machine {
            Machine::Arm => (0x2000_1000, 0x1000),
            Machine::RiscV => (0x8030_0000, 0x1000),
        });
        let mut cursor = ram_base;
        for (name, size) in &regions {
            defined.push((name.clone(), cursor.wrapping_sub(base), false));
            cursor += (*size).max(4).next_multiple_of(4);
        }
        let eh_addr = match regions.is_empty() {
            false => ram_base,
            true => {
                cursor += 4;
                ram_base
            }
        };
        defined.push((String::from(EH_TAG_SYMBOL), eh_addr.wrapping_sub(base), false));
        defined.push((
            String::from(STATICS_START_SYMBOL),
            ram_base.wrapping_sub(base),
            false,
        ));
        defined.push((
            String::from(STATICS_END_SYMBOL),
            cursor.wrapping_sub(base),
            false,
        ));
        let needed = cursor - ram_base;
        if needed > ram_cap {
            return Err(LinkError::StaticsOverflow {
                needed,
                cap: ram_cap,
            });
        }
    }

    let mut veneers: BTreeMap<u32, u32> = BTreeMap::new();
    if machine == Machine::Arm {
        for (oi, obj) in objects.iter().enumerate() {
            for r in &obj.relocations {
                if r.kind != arm::R_ARM_THM_CALL && r.kind != arm::R_ARM_THM_JUMP24 {
                    continue;
                }
                let name = &obj.symbols[r.symbol as usize].name;
                let Some((target, _)) = resolve_sym(&defined, name) else {
                    continue;
                };
                let site = bases[oi] + r.offset;
                let addend = relocation_addend(&text, machine, site, r);
                let delta = i64::from(target) + addend - i64::from(site);
                if !thm_call_in_range(delta) && !veneers.contains_key(&target) {
                    let base = text_base.ok_or(LinkError::AbsoluteNeedsBase)?;
                    let voff = emit_thumb_veneer(&mut text, base.wrapping_add(target) | 1);
                    veneers.insert(target, voff);
                }
            }
        }
    }

    for (oi, obj) in objects.iter().enumerate() {
        for r in &obj.relocations {
            let site = bases[oi] + r.offset;
            apply_relocation(
                &mut text,
                machine,
                site,
                text_base,
                bases[oi],
                &defined,
                &obj.symbols,
                r,
                &veneers,
            )?;
        }
    }

    fill_gcmap_blob(&mut text, objects, &defined);

    let debug_sections = link_carried_sections(objects, &bases, machine, text_base, &defined)?;

    let entry_offset =
        resolve(&defined, entry).ok_or_else(|| LinkError::MissingEntry(String::from(entry)))?;
    Ok(LinkedImage {
        text,
        entry_offset,
        symbols: defined.into_iter().map(|(n, a, _)| (n, a)).collect(),
        debug_sections,
    })
}

/// Concatenates every object's carried (non-allocated) sections by NAME and relocates within the
/// result -- the DWARF passthrough. Returns `(name, bytes)` in first-seen order; an empty vec when
/// no input carries debug info, which is the ordinary case and costs one `is_empty` check.
///
/// The whole subtlety here is that TWO address spaces are in play, exactly as DWARF 5 s7.3.1
/// describes. A reference from `.debug_info` to `.debug_abbrev` must resolve to "that contribution
/// to the combined `.debug_abbrev` section" -- a section-relative offset, with no load address in it,
/// because a debug section is never loaded. A reference to a function (`DW_AT_low_pc`, a
/// `DW_OP_addr`) must resolve to a location in the program's VIRTUAL address space -- so it, and
/// only it, takes `text_base`. Getting these two confused yields debug info that looks plausible and
/// points at nothing, so the alloc/non-alloc distinction is carried explicitly below rather than
/// inferred.
fn link_carried_sections(
    objects: &[Object],
    bases: &[u32],
    machine: Machine,
    text_base: Option<u32>,
    defined: &[Defined],
) -> Result<Vec<(String, Vec<u8>)>, LinkError> {
    if objects.iter().all(|o| o.sections.is_empty()) {
        return Ok(Vec::new());
    }

    let mut out: Vec<(String, Vec<u8>)> = Vec::new();
    let mut placed: BTreeMap<(usize, usize), (usize, u32)> = BTreeMap::new();
    for (oi, obj) in objects.iter().enumerate() {
        for (si, sec) in obj.sections.iter().enumerate() {
            if sec.name == STACKMAP_GCMAP_SECTION {
                continue;
            }
            let out_i = match out.iter().position(|(n, _)| *n == sec.name) {
                Some(i) => i,
                None => {
                    out.push((sec.name.clone(), Vec::new()));
                    out.len() - 1
                }
            };
            let data = &mut out[out_i].1;
            align_to(data, sec.addralign);
            placed.insert((oi, si), (out_i, data.len() as u32));
            data.extend_from_slice(&sec.data);
        }
    }

    for (oi, obj) in objects.iter().enumerate() {
        for (si, sec) in obj.sections.iter().enumerate() {
            if sec.name == STACKMAP_GCMAP_SECTION {
                continue;
            }
            let (out_i, contrib) = placed[&(oi, si)];
            for r in &sec.relocations {
                let abs32 = match machine {
                    Machine::Arm => arm::R_ARM_ABS32,
                    Machine::RiscV => riscv::R_RISCV_32,
                };
                if r.kind != abs32 {
                    return Err(LinkError::UnsupportedRelocation(r.kind));
                }
                let sym = &obj.symbols[r.symbol as usize];
                let (target, allocated) = match sym.section {
                    Some(csi) => {
                        let (_, base) = placed[&(oi, csi as usize)];
                        (base + sym.value, false)
                    }
                    None if sym.defined && (sym.name.is_empty() || sym.binding == Binding::Local) => {
                        (bases[oi] + symbol_target(machine, sym).0, true)
                    }
                    None => {
                        let (addr, _) = resolve_sym(defined, &sym.name)
                            .ok_or_else(|| LinkError::UndefinedSymbol(sym.name.clone()))?;
                        (addr, true)
                    }
                };
                let site = contrib + r.offset;
                let data = &mut out[out_i].1;
                let addend = relocation_addend(data, machine, site, r);
                let base = match allocated {
                    true => i64::from(text_base.ok_or(LinkError::AbsoluteNeedsBase)?),
                    false => 0,
                };
                encode_abs32_at(data, site, base + i64::from(target) + addend)?;
            }
        }
    }
    Ok(out)
}

/// Writes a 32-bit little-endian word into a carried section at `site`. Unlike the code path's
/// [`apply_abs32`] this takes the fully-resolved value (the caller has already decided whether a
/// load address belongs in it) and never applies a Thumb bit: a DWARF address names a location, not
/// a branch target, so the interworking bit would corrupt every odd-valued offset it touched.
fn encode_abs32_at(data: &mut [u8], site: u32, value: i64) -> Result<(), LinkError> {
    let slot = data
        .get_mut(site as usize..site as usize + 4)
        .ok_or(LinkError::RelocationOutOfRange(site))?;
    slot.copy_from_slice(&(value as u32).to_le_bytes());
    Ok(())
}

/// On ARM, a Thumb function carries the Thumb bit in its symbol value's low bit (the Lamella backend
/// and `gcc -mthumb` both set it for `STT_FUNC`); that bit is the `T` an `R_ARM_ABS32` reapplies. Data
/// symbols, and any symbol on a non-ARM target, are not Thumb.
///
/// Takes a value the caller has ALREADY established is a function address -- a resident supplied by
/// the host under that name, or a symbol filtered on `SymbolType::Func`. For a symbol of unknown
/// type the question is [`symbol_target`]'s, because parity does not answer it.
fn is_thumb_func(machine: Machine, value: u32) -> bool {
    machine == Machine::Arm && value & 1 == 1
}

/// A symbol's image-relative value and whether an absolute reference to it carries the Thumb bit.
///
/// The Thumb bit lives in the low bit of an `STT_FUNC` symbol's value and ONLY there. For every
/// other symbol type the low bit is part of the ADDRESS: `.rodata.str1.1` holds byte-aligned
/// mergeable strings, so a string constant sits at an odd address as a matter of course, and
/// `.rodata.cst8` is the same shape for constant pools.
///
/// Normalizing that bit away and ORing it back reproduces the address only when the addend is EVEN.
/// With an ODD addend the reference lands one byte low, and nothing announces it -- the value is a
/// plausible address one byte before the intended one, which for a `core::fmt` template is a
/// leading NUL and an empty formatted result.
///
/// So the SYMBOL TYPE is the key and the parity is not: an odd value is Thumb state on a function
/// and a byte offset on everything else.
fn symbol_target(machine: Machine, sym: &lamella_elf::ParsedSymbol) -> (u32, bool) {
    match sym.kind {
        SymbolType::Func => (
            normalized_value(machine, sym.value),
            is_thumb_func(machine, sym.value),
        ),
        SymbolType::NoType | SymbolType::Section => (sym.value, false),
    }
}

/// The assembly-hash suffix of a statics-REGION symbol name, or `None` for any other name. A
/// region suffix is EXACTLY eight lowercase hex digits (the backend's fnv1a32 of the assembly's
/// CIL bytes) -- which is what keeps `__lamella_statics_start`/`__lamella_statics_end` (the
/// zero-span brackets, same prefix) from being mistaken for regions.
fn statics_region_suffix(name: &str) -> Option<&str> {
    let suffix = name.strip_prefix(STATICS_BASE_PREFIX)?;
    (suffix.len() == 8
        && suffix
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(suffix)
}

/// Links `objects` plus, ON DEMAND, only the `archives` members needed to resolve them -- the classic
/// `.a` semantics. Every explicit object is always included; an archive member is pulled only if it
/// defines a symbol still undefined across the current set, iterated to a fixpoint (a pulled member
/// can reference further members). The result is linked exactly as [`link_with_base`] (so it composes
/// with `--gc-sections` -- pull only needed members, then trim unreached functions). `text_base` is
/// `Some` for an absolute-resolving link (see [`link_at_base`]), `None` otherwise.
pub fn link_with_archives(
    objects: &[Object],
    archives: &[Archive],
    entry: &str,
    text_base: Option<u32>,
) -> Result<LinkedImage, LinkError> {
    let included = include_on_demand(objects, archives);
    link_with_base(&included, entry, text_base, &[])
}

/// As [`link_with_archives`], but with an EXPLICIT statics RAM window `(base, size in bytes)` for
/// the managed regions + EH word (see the layout pass in `link_with_base_inner`) instead of the
/// per-machine default. This is the per-target RAM-plan input: a board whose statics cannot live
/// at the default window (ARM 0x2000_1000 + 4 KiB, RISC-V 0x8030_0000 + 4 KiB) passes its own.
/// The defined region symbols appear in [`LinkedImage::symbols`] resident-style: their recorded
/// offset plus `text_base` is the absolute RAM address.
pub fn link_with_archives_ram(
    objects: &[Object],
    archives: &[Archive],
    entry: &str,
    text_base: Option<u32>,
    ram: (u32, u32),
) -> Result<LinkedImage, LinkError> {
    let included = include_on_demand(objects, archives);
    link_with_base_ram(&included, entry, text_base, &[], Some(ram))
}

/// The explicit objects plus the archive members pulled on demand (see [`link_with_archives`]).
fn include_on_demand(objects: &[Object], archives: &[Archive]) -> Vec<Object> {
    let mut included: Vec<Object> = objects.to_vec();
    let mut pulled: BTreeSet<(usize, usize)> = BTreeSet::new();
    loop {
        let undefined = undefined_symbols(&included);
        if undefined.is_empty() {
            break;
        }
        let mut progress = false;
        for (ai, archive) in archives.iter().enumerate() {
            for (mi, member) in archive.members.iter().enumerate() {
                if pulled.contains(&(ai, mi)) || !defines_any(&member.object, &undefined) {
                    continue;
                }
                pulled.insert((ai, mi));
                included.push(member.object.clone());
                progress = true;
            }
        }
        if !progress {
            break;
        }
    }
    included
}

/// The global symbols referenced but not defined anywhere in `objects`.
fn undefined_symbols(objects: &[Object]) -> BTreeSet<String> {
    let mut defined: BTreeSet<&str> = BTreeSet::new();
    let mut referenced: BTreeSet<String> = BTreeSet::new();
    for o in objects {
        for s in &o.symbols {
            if s.name.is_empty() || s.binding == Binding::Local {
                continue;
            }
            if s.defined {
                defined.insert(s.name.as_str());
            } else {
                referenced.insert(s.name.clone());
            }
        }
    }
    referenced
        .into_iter()
        .filter(|n| !defined.contains(n.as_str()))
        .collect()
}

/// Whether `obj` defines any of the `undefined` global symbols (so it should be pulled from its
/// archive).
fn defines_any(obj: &Object, undefined: &BTreeSet<String>) -> bool {
    obj.symbols
        .iter()
        .any(|s| s.defined && s.binding != Binding::Local && undefined.contains(&s.name))
}

/// The single machine all `objects` target; the relocation set is selected from it. Errors if there
/// are no objects, or if they disagree (ARM and RISC-V cannot be laid out together).
fn link_machine(objects: &[Object]) -> Result<Machine, LinkError> {
    let machine = objects.first().ok_or(LinkError::NoObjects)?.machine;
    if objects.iter().any(|o| o.machine != machine) {
        return Err(LinkError::MixedMachines);
    }
    Ok(machine)
}

/// Resolves relocation `r` (at image offset `site`) and patches `text` in place, dispatching on the
/// target `machine` then the relocation type. `defined` maps a name to its image offset; `obj_syms`
/// is the relocation's own object's symbol table (to name `r.symbol`).
#[allow(clippy::too_many_arguments)]
fn apply_relocation(
    text: &mut [u8],
    machine: Machine,
    site: u32,
    text_base: Option<u32>,
    obj_base: u32,
    defined: &[Defined],
    obj_syms: &[lamella_elf::ParsedSymbol],
    r: &ParsedRelocation,
    veneers: &BTreeMap<u32, u32>,
) -> Result<(), LinkError> {
    if machine == Machine::RiscV && r.kind == riscv::R_RISCV_RELAX {
        return Ok(());
    }
    let sym = &obj_syms[r.symbol as usize];
    let (target, target_is_thumb) = if sym.defined
        && (sym.name.is_empty() || sym.binding == Binding::Local)
    {
        let (value, thumb) = symbol_target(machine, sym);
        (obj_base + value, thumb)
    } else {
        resolve_sym(defined, &sym.name).ok_or_else(|| LinkError::UndefinedSymbol(sym.name.clone()))?
    };
    let target = target as i64;
    let site_i = site as i64;
    let addend = relocation_addend(text, machine, site, r);
    match machine {
        Machine::RiscV => match r.kind {
            riscv::R_RISCV_CALL_PLT => {
                let delta = target + addend - site_i;
                let lo12 = (delta & 0xfff) as u32;
                let hi20 = (((delta + 0x800) >> 12) & 0xfffff) as u32;
                patch_or(text, site as usize, hi20 << 12);
                patch_or(text, site as usize + 4, (lo12 & 0xfff) << 20);
                Ok(())
            }
            riscv::R_RISCV_32 => apply_abs32(text, site, text_base, target + addend, false),
            riscv::R_RISCV_HI20 => {
                let base = text_base.ok_or(LinkError::AbsoluteNeedsBase)?;
                let addr = i64::from(base) + target + addend;
                let hi20 = (((addr + 0x800) >> 12) & 0xfffff) as u32;
                patch_or(text, site as usize, hi20 << 12);
                Ok(())
            }
            riscv::R_RISCV_LO12_I => {
                let base = text_base.ok_or(LinkError::AbsoluteNeedsBase)?;
                let addr = i64::from(base) + target + addend;
                let lo12 = (addr & 0xfff) as u32;
                patch_or(text, site as usize, lo12 << 20);
                Ok(())
            }
            riscv::R_RISCV_LO12_S => {
                let base = text_base.ok_or(LinkError::AbsoluteNeedsBase)?;
                let addr = i64::from(base) + target + addend;
                let lo12 = (addr & 0xfff) as u32;
                patch_or(text, site as usize, ((lo12 >> 5) << 25) | ((lo12 & 0x1f) << 7));
                Ok(())
            }
            riscv::R_LAMELLA_REL_DESC => encode_rel32(text, site, target + addend - site_i),
            other => Err(LinkError::UnsupportedRelocation(other)),
        },
        Machine::Arm => match r.kind {
            arm::R_ARM_THM_CALL => {
                let direct = target + addend - site_i;
                if thm_call_in_range(direct) {
                    encode_thm_call(text, site, direct)
                } else {
                    let voff = veneers
                        .get(&(target as u32))
                        .copied()
                        .ok_or(LinkError::RelocationOutOfRange(site))?;
                    encode_thm_call(text, site, i64::from(voff) + addend - site_i)
                }
            }
            arm::R_ARM_CALL => encode_arm_call(text, site, target + addend - site_i),
            arm::R_ARM_ABS32 => {
                apply_abs32(text, site, text_base, target + addend, target_is_thumb)
            }
            arm::R_LAMELLA_REL_DESC => encode_rel32(text, site, target + addend - site_i),
            arm::R_ARM_THM_JUMP24 => {
                let direct = target + addend - site_i;
                if thm_call_in_range(direct) {
                    encode_thm_call(text, site, direct)
                } else {
                    let voff = veneers
                        .get(&(target as u32))
                        .copied()
                        .ok_or(LinkError::RelocationOutOfRange(site))?;
                    encode_thm_call(text, site, i64::from(voff) + addend - site_i)
                }
            }
            arm::R_ARM_THM_MOVW_ABS_NC => {
                let base = text_base.ok_or(LinkError::AbsoluteNeedsBase)?;
                let full = (i64::from(base) + target + addend) | i64::from(target_is_thumb);
                encode_thm_mov(text, site, (full & 0xFFFF) as u16)
            }
            arm::R_ARM_THM_MOVT_ABS => {
                let base = text_base.ok_or(LinkError::AbsoluteNeedsBase)?;
                let full = (i64::from(base) + target + addend) | i64::from(target_is_thumb);
                encode_thm_mov(text, site, ((full >> 16) & 0xFFFF) as u16)
            }
            other => Err(LinkError::UnsupportedRelocation(other)),
        },
    }
}

/// The relocation's addend `A`. RISC-V, and the ARM objects the Lamella backend emits, use explicit
/// `RELA` addends; a `SHT_REL` ARM object (the `-mthumb` C toolchain's convention) stores the addend
/// implicitly in the instruction field, so the linker extracts it from the call's current encoding.
fn relocation_addend(text: &[u8], machine: Machine, site: u32, r: &ParsedRelocation) -> i64 {
    if r.implicit_addend {
        match (machine, r.kind) {
            (Machine::Arm, arm::R_ARM_THM_CALL) => extract_thm_call(text, site),
            (Machine::Arm, arm::R_ARM_THM_JUMP24) => extract_thm_call(text, site),
            (Machine::Arm, arm::R_ARM_CALL) => extract_arm_call(text, site),
            (Machine::Arm, arm::R_ARM_THM_MOVW_ABS_NC) => extract_thm_mov(text, site),
            (Machine::Arm, arm::R_ARM_THM_MOVT_ABS) => extract_thm_mov(text, site) << 16,
            (Machine::Arm, arm::R_ARM_ABS32) | (Machine::RiscV, riscv::R_RISCV_32) => {
                text.get(site as usize..site as usize + 4).map_or(0, |b| {
                    u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as i32 as i64
                })
            }
            _ => 0,
        }
    } else {
        r.addend as i64
    }
}

/// Like [`link`], but with `--gc-sections` dead-stripping first: [`garbage_collect`]'s reachability
/// walk from `entry` decides what survives -- a function's calls AND a data symbol's references, so
/// a reached descriptor keeps the methods its vtable points at and an unreached one takes its
/// vtable's methods with it -- and the survivors are linked exactly as [`link`] links them. This is
/// [`link_at_base_gc`] without a base; the two are one path.
pub fn link_gc(objects: &[Object], entry: &str) -> Result<LinkedImage, LinkError> {
    link_gc_inner(objects, entry, false, None)
}

/// Like [`link_gc`], but ALSO folds identical functions (ICF) after dead-stripping: byte-identical
/// reachable functions (the same code + the same relocations by target) merge to one copy, and
/// references to the folded-away ones redirect to the survivor. Conservative -- it does not chase
/// transitively-identical callees -- and SAFE: an ADDRESS-TAKEN function (the target of an
/// `R_ARM_ABS32`/`R_RISCV_32`, and the entry) keeps its own identity and is never folded away, so a
/// function-pointer comparison still behaves. `text_base` is needed iff the program has an absolute
/// relocation (see [`link_at_base`]).
pub fn link_icf(
    objects: &[Object],
    entry: &str,
    text_base: Option<u32>,
) -> Result<LinkedImage, LinkError> {
    link_gc_inner(objects, entry, true, text_base)
}

/// As [`link_gc`], but pulling archive members on demand first, exactly as [`link_with_archives`]
/// does -- so a program that reaches the runtime-support archive can be measured against its
/// [`link_icf_with_archives`] twin on the SAME input.
///
/// **THE ARCHIVE IS DEAD-STRIPPED HERE AND IS NOT IN THE PRODUCTION LINK, WHICH IS THE WHOLE
/// DIFFERENCE THE SIZE LEDGER MEASURES.** `link_with_archives` pulls a member WHOLE to resolve one
/// undefined symbol and never revisits it, and the AOT driver runs [`garbage_collect`] on the
/// program objects BEFORE the archives are added -- so a seam the program's corlib names but never
/// reaches is charged to every image. Pulling and THEN dead-stripping is what this path does
/// differently, so `text_prod - text_gc` is that charge.
pub fn link_gc_with_archives(
    objects: &[Object],
    archives: &[Archive],
    entry: &str,
    text_base: Option<u32>,
) -> Result<LinkedImage, LinkError> {
    link_gc_inner(&include_on_demand(objects, archives), entry, false, text_base)
}

/// As [`link_icf`], but pulling archive members on demand first (see [`link_gc_with_archives`]).
/// The pair exists because the only honest measure of what ICF recovers is the same object set
/// linked both ways: folding measured on a program object alone omits every archive body, and the
/// archive is where a duplicated helper is most likely to appear twice.
pub fn link_icf_with_archives(
    objects: &[Object],
    archives: &[Archive],
    entry: &str,
    text_base: Option<u32>,
) -> Result<LinkedImage, LinkError> {
    link_gc_inner(&include_on_demand(objects, archives), entry, true, text_base)
}

/// Dead-strips from `entry` and then links through the ORDINARY layout, optionally folding
/// identical functions on the way (see [`link_gc`] / [`link_icf`]).
///
/// **THE WHOLE POINT OF THIS SHAPE IS THAT IT ANSWERS NO QUESTION TWICE.** Reachability is
/// [`reachable_from`]'s -- the same walk [`garbage_collect`] uses, which follows a data symbol's
/// references as well as a function's calls. Trimming is [`trim_object`]'s. Layout, symbol
/// resolution, the statics window, the stack-map tables and the veneers are
/// [`link_with_base`]'s. What is left here is ICF's own decision and nothing else.
///
/// **ICF IS EXPRESSED AS AN OBJECT REWRITE RATHER THAN A LAYOUT OF ITS OWN**: a folded-away
/// function is dropped from the keep set (which takes its stack-map record with it, by
/// `trim_object`'s existing record rule) and its name is re-defined as an ALIAS of the
/// representative's address. That is the whole difference between `link_gc` and `link_icf`, which
/// is what makes `text_icf` against `text_gc` a measurement of folding rather than of two linkers.
fn link_gc_inner(
    objects: &[Object],
    entry: &str,
    fold: bool,
    text_base: Option<u32>,
) -> Result<LinkedImage, LinkError> {
    let machine = link_machine(objects)?;
    let mut keep = reachable_from(objects, entry);
    let folds = match fold {
        true => plan_folds(objects, machine, &keep, entry),
        false => Vec::new(),
    };
    for (away, _) in &folds {
        keep.remove(away);
    }
    let mut trimmed: Vec<Object> = trim_all(objects, &keep);
    define_fold_aliases(&mut trimmed, &folds);
    link_with_base(&trimmed, entry, text_base, &[])
}

/// The ICF decision as `(folded-away name, representative name)` pairs, over the functions that
/// SURVIVE dead-stripping. Empty when nothing folds.
///
/// A function is a candidate when it is a defined, sized, non-local `STT_FUNC` symbol that `keep`
/// retains and that exactly ONE object defines. **The extent is `st_value .. st_value + st_size`**,
/// the same extent [`garbage_collect`] and [`trim_object`] take. It is deliberately not the span up
/// to the next function symbol, which would swallow any data laid between two functions.
///
/// **A MULTIPLY-DEFINED NAME IS EXCLUDED, AND NOT AS A REFINEMENT.** `compiler_builtins` emits
/// its `__aeabi_*` soft-float helpers WEAK and several pulled members re-define one, so the same
/// name arrives twice with identical bytes -- an identical fingerprint, which would fold the second
/// copy into the first and hand back the pair `(name, name)`. Dropping `name` from the keep set
/// then deletes BOTH definitions and the alias has nothing to point at: a fold that erases the
/// function it was meant to share. Which copy survives is `link_with_base_inner`'s strong-over-weak
/// rule to decide, not this pass's, so the honest move is not to fold such a name at all.
fn plan_folds(
    objects: &[Object],
    machine: Machine,
    keep: &BTreeSet<String>,
    entry: &str,
) -> Vec<(String, String)> {
    let mut definitions: BTreeMap<&str, usize> = BTreeMap::new();
    for obj in objects {
        for s in &obj.symbols {
            if s.defined && !s.name.is_empty() {
                *definitions.entry(s.name.as_str()).or_default() += 1;
            }
        }
    }
    let mut funcs: Vec<Func> = Vec::new();
    for (oi, obj) in objects.iter().enumerate() {
        if resolves_against_its_own_layout(obj) {
            continue;
        }
        for s in &obj.symbols {
            if !s.defined
                || s.size == 0
                || s.name.is_empty()
                || s.binding == Binding::Local
                || s.kind != SymbolType::Func
                || !keep.contains(&s.name)
                || definitions.get(s.name.as_str()).copied().unwrap_or(0) != 1
            {
                continue;
            }
            let start = normalized_value(machine, s.value);
            funcs.push((oi, s.name.clone(), start, start + s.size));
        }
    }
    let Some(entry_fi) = funcs.iter().position(|(_, n, _, _)| n == entry) else {
        return Vec::new();
    };
    compute_folds(&funcs, objects, machine, entry_fi)
        .into_iter()
        .enumerate()
        .filter_map(|(fi, rep)| Some((funcs[fi].1.clone(), funcs[rep?].1.clone())))
        .collect()
}

/// Re-defines each folded-away name as an alias of its representative, in the object that kept the
/// representative.
///
/// The alias carries the representative's `st_value` VERBATIM -- Thumb bit included, since on ARM
/// that bit is what makes an `R_ARM_ABS32` reference re-enter Thumb state -- and its `st_size`,
/// because the alias really does cover those bytes. Nothing is added for a representative that did
/// not survive the trim; [`plan_folds`] elects one from the keep set, so that cannot happen, and a
/// silent skip is still better than a panic in a linker.
fn define_fold_aliases(objects: &mut [Object], folds: &[(String, String)]) {
    for (away, rep) in folds {
        let Some((oi, sym)) = objects.iter().enumerate().find_map(|(oi, o)| {
            o.symbols
                .iter()
                .find(|s| s.defined && s.name == *rep)
                .map(|s| (oi, s.clone()))
        }) else {
            continue;
        };
        objects[oi].symbols.push(lamella_elf::ParsedSymbol {
            name: away.clone(),
            ..sym
        });
    }
}

/// A function for ICF comparison: `(oi, name, start, end)` -- its object + byte range.
type Func = (usize, String, u32, u32);

/// A function's identity for ICF: its code bytes plus its relocations as `(offset-within-function,
/// kind, addend, target name)`, sorted. Two functions with equal fingerprints are interchangeable --
/// the relocation targets (by name) must match, so two functions calling different symbols never
/// fold even with identical placeholder bytes; a `SHT_REL` implicit addend rides in the code bytes.
type Fingerprint = (Vec<u8>, Vec<(u32, u32, i32, String)>);

fn function_fingerprint(func: &Func, objects: &[Object]) -> Fingerprint {
    let (oi, _, start, end) = func;
    let code = objects[*oi].text[*start as usize..*end as usize].to_vec();
    let mut relocs: Vec<(u32, u32, i32, String)> = objects[*oi]
        .relocations
        .iter()
        .filter(|r| r.offset >= *start && r.offset < *end)
        .map(|r| {
            let target = objects[*oi]
                .symbols
                .get(r.symbol as usize)
                .map_or(String::new(), |s| s.name.clone());
            (r.offset - start, r.kind, r.addend, target)
        })
        .collect();
    relocs.sort();
    (code, relocs)
}

/// Decides ICF folding: returns `fold_to`, where `fold_to[fi] = Some(rep)` means function `fi` folds
/// into representative `rep`. `funcs` is the SURVIVING set -- [`plan_folds`] filters it against the
/// dead-strip keep set, so there is no reachability flag to consult here and no way to fold into a
/// representative that is about to be trimmed. Functions are grouped by fingerprint; in each group
/// the non-address-taken duplicates fold into one survivor (an address-taken member preferred as the
/// survivor so its identity is what remains). Address-taken functions and the entry never fold away.
fn compute_folds(
    funcs: &[Func],
    objects: &[Object],
    machine: Machine,
    entry_fi: usize,
) -> Vec<Option<usize>> {
    let mut address_taken: BTreeSet<&str> = BTreeSet::new();
    for obj in objects {
        for r in &obj.relocations {
            let absolute = matches!(
                (machine, r.kind),
                (Machine::Arm, arm::R_ARM_ABS32) | (Machine::RiscV, riscv::R_RISCV_32)
            );
            if absolute {
                if let Some(s) = obj.symbols.get(r.symbol as usize) {
                    address_taken.insert(s.name.as_str());
                }
            }
        }
    }
    let fps: Vec<Fingerprint> = funcs
        .iter()
        .map(|f| function_fingerprint(f, objects))
        .collect();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for fi in 0..funcs.len() {
        match groups.iter_mut().find(|g| fps[g[0]] == fps[fi]) {
            Some(g) => g.push(fi),
            None => groups.push(alloc::vec![fi]),
        }
    }
    let keep_identity = |fi: usize| fi == entry_fi || address_taken.contains(funcs[fi].1.as_str());
    let mut fold_to = alloc::vec![None; funcs.len()];
    for group in &groups {
        if group.len() < 2 {
            continue;
        }
        let rep = group
            .iter()
            .copied()
            .find(|&fi| keep_identity(fi))
            .unwrap_or(group[0]);
        for &fi in group {
            if fi != rep && !keep_identity(fi) {
                fold_to[fi] = Some(rep);
            }
        }
    }
    fold_to
}

/// A defined symbol: its image address (Thumb bit normalized away) and whether it is a Thumb
/// function (the `T` bit an `R_ARM_ABS32` ORs back into an absolute reference, so a `blx` to it
/// re-enters Thumb state).
type Defined = (String, u32, bool);

fn resolve(symbols: &[Defined], name: &str) -> Option<u32> {
    symbols
        .iter()
        .find(|(n, _, _)| n == name)
        .map(|&(_, a, _)| a)
}

/// Like [`resolve`], but also returns whether the symbol is a Thumb function (for `R_ARM_ABS32`'s
/// `T` bit).
fn resolve_sym(symbols: &[Defined], name: &str) -> Option<(u32, bool)> {
    symbols
        .iter()
        .find(|(n, _, _)| n == name)
        .map(|&(_, a, t)| (a, t))
}

/// Pads `text` with zero bytes up to the next multiple of `align` (a power of two; 0/1 = no padding).
fn align_to(text: &mut Vec<u8>, align: u32) {
    let align = align.max(1) as usize;
    while text.len() % align != 0 {
        text.push(0);
    }
}

fn patch_or(text: &mut [u8], off: usize, bits: u32) {
    let w = u32::from_le_bytes([text[off], text[off + 1], text[off + 2], text[off + 3]]) | bits;
    text[off..off + 4].copy_from_slice(&w.to_le_bytes());
}

/// Encodes the signed byte offset `off` into the 32-bit Thumb `BL` (encoding T1) at `site`: the
/// S:J1:J2:imm10:imm11 swizzle, `J{1,2} = NOT(I{1,2} XOR S)` (Armv6-M ARM A6.7.13), fully
/// overwriting the two halfwords (so a `SHT_REL` object's in-place addend bits are cleared). This is
/// the link-time twin of `lamella_asm_arm32`'s `ThumbCall` fixup.
fn encode_thm_call(text: &mut [u8], site: u32, off: i64) -> Result<(), LinkError> {
    if off % 2 != 0 || !(-16_777_216..=16_777_214).contains(&off) {
        return Err(LinkError::RelocationOutOfRange(site));
    }
    let s = ((off >> 24) & 1) as u16;
    let i1 = ((off >> 23) & 1) as u16;
    let i2 = ((off >> 22) & 1) as u16;
    let imm10 = ((off >> 12) & 0x3FF) as u16;
    let imm11 = ((off >> 1) & 0x7FF) as u16;
    let j1 = (i1 ^ s) ^ 1;
    let j2 = (i2 ^ s) ^ 1;
    let site = site as usize;
    let slot = text
        .get_mut(site..site + 4)
        .ok_or(LinkError::RelocationOutOfRange(site as u32))?;
    let hw2_old = u16::from_le_bytes([slot[2], slot[3]]);
    let hw1 = 0xF000 | (s << 10) | imm10;
    let hw2 = 0x9000 | (hw2_old & 0x4000) | (j1 << 13) | (j2 << 11) | imm11;
    slot[0..2].copy_from_slice(&hw1.to_le_bytes());
    slot[2..4].copy_from_slice(&hw2.to_le_bytes());
    Ok(())
}

/// Writes the 16-bit immediate `imm16` into a Thumb-2 `MOVW`/`MOVT` at `site`, splitting it across the
/// four scattered fields (imm4 = hw1[3:0], i = hw1[10], imm3 = hw2[14:12], imm8 = hw2[7:0]) and preserving
/// everything else -- the opcode and `Rd` (hw2[11:8]). The MOVW/MOVT distinction (hw1[7]) and which half
/// of the address is passed are the caller's; this is the shared field-packing.
fn encode_thm_mov(text: &mut [u8], site: u32, imm16: u16) -> Result<(), LinkError> {
    let site = site as usize;
    let slot = text
        .get_mut(site..site + 4)
        .ok_or(LinkError::RelocationOutOfRange(site as u32))?;
    let hw1 = u16::from_le_bytes([slot[0], slot[1]]);
    let hw2 = u16::from_le_bytes([slot[2], slot[3]]);
    let imm4 = (imm16 >> 12) & 0xF;
    let i = (imm16 >> 11) & 1;
    let imm3 = (imm16 >> 8) & 0x7;
    let imm8 = imm16 & 0xFF;
    let hw1 = (hw1 & !((1 << 10) | 0xF)) | (i << 10) | imm4;
    let hw2 = (hw2 & !((0x7 << 12) | 0xFF)) | (imm3 << 12) | imm8;
    slot[0..2].copy_from_slice(&hw1.to_le_bytes());
    slot[2..4].copy_from_slice(&hw2.to_le_bytes());
    Ok(())
}

/// The 16-bit immediate currently encoded in a Thumb-2 `MOVW`/`MOVT` at `site` -- the implicit addend of a
/// `SHT_REL` `R_ARM_THM_MOVW_ABS_NC`/`MOVT_ABS` (the inverse of [`encode_thm_mov`]; a fresh
/// `movw rd, #:lower16:sym` reads back 0). The caller shifts a MOVT's result into the high half.
fn extract_thm_mov(text: &[u8], site: u32) -> i64 {
    let site = site as usize;
    let Some(b) = text.get(site..site + 4) else {
        return 0;
    };
    let hw1 = u16::from_le_bytes([b[0], b[1]]);
    let hw2 = u16::from_le_bytes([b[2], b[3]]);
    let imm4 = hw1 & 0xF;
    let i = (hw1 >> 10) & 1;
    let imm3 = (hw2 >> 12) & 0x7;
    let imm8 = hw2 & 0xFF;
    i64::from((imm4 << 12) | (i << 11) | (imm3 << 8) | imm8)
}

/// Whether a Thumb `BL` can reach `off` -- a halfword-even displacement within +/-16 MB.
fn thm_call_in_range(off: i64) -> bool {
    off % 2 == 0 && (-16_777_216..=16_777_214).contains(&off)
}

/// Appends an ARMv6-M long-branch VENEER to `text` (4-byte aligned) and returns its offset. A `BL` that
/// cannot reach a far target instead reaches this trampoline, which loads the absolute `target` (Thumb
/// bit set) and `bx`es to it. It preserves r0-r3 (the call's arguments -- only r0 is touched, saved and
/// restored) and LR (so the callee returns straight to the original caller, never back here). `mov ip,
/// r0` is used because `ldr ip, [pc]` (a high register) does not encode on ARMv6-M.
fn emit_thumb_veneer(text: &mut Vec<u8>, target: u32) -> u32 {
    align_to(text, 4);
    let offset = text.len() as u32;
    text.extend_from_slice(&[0x01, 0xB4]);
    text.extend_from_slice(&[0x02, 0x48]);
    text.extend_from_slice(&[0x84, 0x46]);
    text.extend_from_slice(&[0x01, 0xBC]);
    text.extend_from_slice(&[0x60, 0x47]);
    text.extend_from_slice(&[0x00, 0xBF]);
    text.extend_from_slice(&target.to_le_bytes());
    offset
}

/// The signed byte offset currently encoded in the Thumb `BL` at `site` -- the implicit addend of a
/// `SHT_REL` `R_ARM_THM_CALL` (the inverse of [`encode_thm_call`]; a freshly assembled `bl symbol`
/// reads back -4, the Thumb pipeline bias).
fn extract_thm_call(text: &[u8], site: u32) -> i64 {
    let site = site as usize;
    let Some(b) = text.get(site..site + 4) else {
        return 0;
    };
    let hw1 = u16::from_le_bytes([b[0], b[1]]) as i64;
    let hw2 = u16::from_le_bytes([b[2], b[3]]) as i64;
    let s = (hw1 >> 10) & 1;
    let imm10 = hw1 & 0x3FF;
    let j1 = (hw2 >> 13) & 1;
    let j2 = (hw2 >> 11) & 1;
    let imm11 = hw2 & 0x7FF;
    let i1 = (j1 ^ s) ^ 1;
    let i2 = (j2 ^ s) ^ 1;
    let off = (s << 24) | (i1 << 23) | (i2 << 22) | (imm10 << 12) | (imm11 << 1);
    off - (s << 25)
}

/// Encodes the signed byte offset `off` into the A32 `BL` (encoding A1) at `site`: the 24-bit
/// word-scaled immediate in bits[23:0], preserving the condition + opcode in bits[31:24].
fn encode_arm_call(text: &mut [u8], site: u32, off: i64) -> Result<(), LinkError> {
    if off % 4 != 0 || !(-33_554_432..=33_554_428).contains(&off) {
        return Err(LinkError::RelocationOutOfRange(site));
    }
    let imm24 = ((off >> 2) & 0xFF_FFFF) as u32;
    let site = site as usize;
    let slot = text
        .get_mut(site..site + 4)
        .ok_or(LinkError::RelocationOutOfRange(site as u32))?;
    let instr = u32::from_le_bytes([slot[0], slot[1], slot[2], slot[3]]);
    let new = (instr & 0xFF00_0000) | imm24;
    slot.copy_from_slice(&new.to_le_bytes());
    Ok(())
}

/// The signed byte offset currently encoded in the A32 `BL` at `site` -- the implicit addend of a
/// `SHT_REL` `R_ARM_CALL` (a freshly assembled A32 `bl symbol` reads back -8, the ARM pipeline bias).
fn extract_arm_call(text: &[u8], site: u32) -> i64 {
    let site = site as usize;
    let Some(b) = text.get(site..site + 4) else {
        return 0;
    };
    let imm24 = (u32::from_le_bytes([b[0], b[1], b[2], b[3]]) & 0xFF_FFFF) as i64;
    let off = imm24 << 2;
    if off & (1 << 25) != 0 {
        off - (1 << 26)
    } else {
        off
    }
}

/// Resolves an absolute 32-bit reference: needs the load base (`value` = `S + A` is an image offset),
/// so it errors when the link is base-agnostic; otherwise writes `text_base + value` (`| T`).
fn apply_abs32(
    text: &mut [u8],
    site: u32,
    text_base: Option<u32>,
    value: i64,
    thumb: bool,
) -> Result<(), LinkError> {
    let base = text_base.ok_or(LinkError::AbsoluteNeedsBase)? as i64;
    encode_abs32(text, site, base + value, thumb)
}

/// Writes the signed 32-bit relative `value` (already `S + A - P`) at `site` as a plain little-endian
/// word -- no Thumb-bit forcing and no `text_base` (the value is position-independent). The
/// `R_LAMELLA_REL_DESC` vtable-slot twin of [`encode_abs32`].
fn encode_rel32(text: &mut [u8], site: u32, value: i64) -> Result<(), LinkError> {
    let word = value as u32;
    let site = site as usize;
    let slot = text
        .get_mut(site..site + 4)
        .ok_or(LinkError::RelocationOutOfRange(site as u32))?;
    slot.copy_from_slice(&word.to_le_bytes());
    Ok(())
}

/// Writes the absolute 32-bit `value` (already `text_base + S + A`) at `site`, ORing in `thumb` as
/// the low bit (the ARM ELF `(S + A) | T`, RISC-V passes `false`). Overwrites the word (so a
/// `SHT_REL` object's in-place addend is cleared after `relocation_addend` read it).
fn encode_abs32(text: &mut [u8], site: u32, value: i64, thumb: bool) -> Result<(), LinkError> {
    let word = value as u32 | thumb as u32;
    let site = site as usize;
    let slot = text
        .get_mut(site..site + 4)
        .ok_or(LinkError::RelocationOutOfRange(site as u32))?;
    slot.copy_from_slice(&word.to_le_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use lamella_elf::{
        ArchiveMember, Binding, Machine, Relocation, Section, Symbol, SymbolSection, SymbolType,
        arm, read_object, write_relocatable_object, write_relocatable_object_with_sections,
    };

    /// Every name family the backend emits, classified. Written from the shapes actually observed
    /// in emitted objects rather than from the classifier, so a family it cannot see is a failing
    /// row here rather than a silent `Unknown` in a message.
    #[test]
    fn an_unresolved_name_is_classified_by_the_shape_the_backend_emitted_it_under() {
        for name in [
            "lamella_gc_alloc",
            "lamella_char_to_string",
            "lamella_fabs",
            "lamella_thread_yield",
        ] {
            assert_eq!(
                undefined_provider(name),
                UndefinedProvider::RuntimeSupport,
                "{name} is a runtime-support seam"
            );
        }
        for name in [
            "Lamella.Hardware.Mmio.Write32.II.v",
            "System.Object.Equals.o.z",
            "System.Object.GetHashCode..i",
            "Lamella.Hardware.Mmio..ctor..v",
        ] {
            assert_eq!(
                undefined_provider(name),
                UndefinedProvider::ManagedAssembly,
                "{name} is a managed method reached across an assembly boundary"
            );
        }
        for name in ["__lamella_statics_7f58c4c2", "__lamella_typedesc_6ecd7930_33554521"] {
            assert_eq!(
                undefined_provider(name),
                UndefinedProvider::LinkerDefined,
                "{name} is defined by a later linker pass"
            );
        }
        for name in ["L22f7906e.f2", "L22f7906e.f5"] {
            assert_eq!(
                undefined_provider(name),
                UndefinedProvider::Unknown,
                "{name} is an internal library symbol, not a managed method"
            );
        }
        assert_eq!(undefined_provider("f0"), UndefinedProvider::Unknown);
    }

    /// The rendering, at the two families a user's composition mistake actually produces. Asserted
    /// on content rather than on the whole string so wording can move without a test pinning it.
    #[test]
    fn an_undefined_symbol_renders_the_input_that_would_have_defined_it() {
        let seam = LinkError::UndefinedSymbol(String::from("lamella_gc_alloc"));
        let rendered = alloc::format!("{seam}");
        assert!(rendered.contains("lamella_gc_alloc"), "{rendered}");
        assert!(rendered.contains("runtime-support archive"), "{rendered}");

        let managed = LinkError::UndefinedSymbol(String::from("Lamella.Hardware.Mmio.Write32.II.v"));
        let rendered = alloc::format!("{managed}");
        assert!(rendered.contains("library object"), "{rendered}");
        assert!(rendered.contains("COMPILE"), "{rendered}");

        assert_ne!(
            alloc::format!("{seam}"),
            alloc::format!("{managed}"),
            "each family names its own missing input"
        );
    }

    /// What a carried-section relocation points at: a function in the loaded image, or another
    /// carried section (named by the nameless `STT_SECTION` symbol a real toolchain emits).
    enum DTarget {
        Code(&'static str),
        Section(&'static str),
    }

    /// Builds an object with carried DWARF sections by EMITTING one and reading it back -- so every
    /// test below exercises the writer, the reader, and the linker together, in the shape a real
    /// toolchain object takes. (It was hand-built before the writer could emit carried sections.)
    ///
    /// Cross-section references are made through NAMELESS `STT_SECTION` symbols, exactly as LLVM
    /// emits them, which is the case a name-keyed linker structurally cannot resolve.
    fn debug_obj(
        machine: Machine,
        text: &[u8],
        funcs: &[(&str, u32, u32)],
        debug: &[(&str, &[u8], &[(u32, DTarget)])],
    ) -> Object {
        let abs32 = match machine {
            Machine::Arm => arm::R_ARM_ABS32,
            Machine::RiscV => riscv::R_RISCV_32,
        };
        let mut symbols: Vec<Symbol> = funcs
            .iter()
            .map(|&(name, value, size)| Symbol {
                name,
                value,
                size,
                binding: Binding::Global,
                kind: SymbolType::Func,
                section: SymbolSection::Text,
            })
            .collect();
        let section_syms = symbols.len() as u32;
        for i in 0..debug.len() {
            symbols.push(Symbol {
                name: "",
                value: 0,
                size: 0,
                binding: Binding::Local,
                kind: SymbolType::Section,
                section: SymbolSection::InSection(i as u32),
            });
        }
        let relocs: Vec<Vec<Relocation>> = debug
            .iter()
            .map(|&(_, _, rs)| {
                rs.iter()
                    .map(|(offset, target)| Relocation {
                        offset: *offset,
                        symbol: match target {
                            DTarget::Code(n) => {
                                funcs.iter().position(|&(f, _, _)| f == *n).unwrap() as u32
                            }
                            DTarget::Section(n) => {
                                section_syms
                                    + debug.iter().position(|&(dn, _, _)| dn == *n).unwrap() as u32
                            }
                        },
                        kind: abs32,
                        addend: 0,
                    })
                    .collect()
            })
            .collect();
        let sections: Vec<Section> = debug
            .iter()
            .enumerate()
            .map(|(i, &(name, data, _))| Section {
                name,
                flags: 0,
                addralign: 1,
                data,
                relocations: &relocs[i],
            })
            .collect();
        read_object(&write_relocatable_object_with_sections(
            machine, text, &symbols, &[], &sections,
        ))
        .expect("an emitted object with carried sections reads back")
    }

    /// Reads the 32-bit little-endian word a carried section holds at `offset`.
    fn debug_word(image: &LinkedImage, name: &str, offset: usize) -> u32 {
        let (_, data) = image
            .debug_sections
            .iter()
            .find(|(n, _)| n == name)
            .unwrap_or_else(|| panic!("no {name} in the linked image"));
        u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ])
    }

    fn obj(text: &[u8], syms: &[Symbol], relocs: &[Relocation]) -> Object {
        read_object(&write_relocatable_object(
            Machine::RiscV,
            text,
            syms,
            relocs,
        ))
        .unwrap()
    }

    fn obj_arm(text: &[u8], syms: &[Symbol], relocs: &[Relocation]) -> Object {
        read_object(&write_relocatable_object(Machine::Arm, text, syms, relocs)).unwrap()
    }

    fn func(name: &'static str, value: u32, size: u32) -> Symbol<'static> {
        Symbol {
            name,
            value,
            size,
            binding: Binding::Global,
            kind: SymbolType::Func,
            section: SymbolSection::Text,
        }
    }

    fn undef(name: &'static str) -> Symbol<'static> {
        Symbol {
            name,
            value: 0,
            size: 0,
            binding: Binding::Global,
            kind: SymbolType::NoType,
            section: SymbolSection::Undefined,
        }
    }

    /// An undefined reference CARRYING a size -- the statics-region shape (`st_size` = the
    /// region's byte size, the linker's RAM-layout input).
    fn undef_sized(name: &'static str, size: u32) -> Symbol<'static> {
        Symbol {
            name,
            value: 0,
            size,
            binding: Binding::Global,
            kind: SymbolType::NoType,
            section: SymbolSection::Undefined,
        }
    }

    fn weak(name: &'static str, value: u32, size: u32) -> Symbol<'static> {
        Symbol {
            name,
            value,
            size,
            binding: Binding::Weak,
            kind: SymbolType::Func,
            section: SymbolSection::Text,
        }
    }

    fn data(name: &'static str, value: u32, size: u32) -> Symbol<'static> {
        Symbol {
            name,
            value,
            size,
            binding: Binding::Global,
            kind: SymbolType::NoType,
            section: SymbolSection::Text,
        }
    }

    /// A weak DATA symbol (a stack-map record's shape).
    fn weak_data(name: &'static str, value: u32, size: u32) -> Symbol<'static> {
        Symbol {
            name,
            value,
            size,
            binding: Binding::Weak,
            kind: SymbolType::NoType,
            section: SymbolSection::Text,
        }
    }

    /// An object carrying a `.lamella_gcmap` section beside its `.text`, which is how the ARM
    /// backend hands the linker its per-function safepoint fragments.
    fn obj_arm_gcmap(
        text: &[u8],
        syms: &[Symbol],
        relocs: &[Relocation],
        gcmap: &[u8],
    ) -> Object {
        read_object(&lamella_elf::write_relocatable_object_with_sections(
            Machine::Arm,
            text,
            syms,
            relocs,
            &[lamella_elf::Section {
                name: STACKMAP_GCMAP_SECTION,
                flags: 0,
                addralign: 4,
                data: gcmap,
                relocations: &[],
            }],
        ))
        .unwrap()
    }

    /// One `.lamella_gcmap` fragment for `name`: each `(rel_pc, nrefs)` becomes an entry whose tail
    /// is `[frame_size=8][saved_bytes=8][nrefs][ref_offsets..][ntagged=0]`. An ODD `nrefs` makes the
    /// tail 10 bytes rather than 8, which is what exercises the fragment's 4-byte entry padding.
    fn gcmap_fragment(name: &str, entries: &[(u32, u16)]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        while out.len() % 4 != 0 {
            out.push(0);
        }
        out.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for &(rel_pc, nrefs) in entries {
            let mut tail = Vec::new();
            tail.extend_from_slice(&8u16.to_le_bytes());
            tail.extend_from_slice(&8u16.to_le_bytes());
            tail.extend_from_slice(&nrefs.to_le_bytes());
            for r in 0..nrefs {
                tail.extend_from_slice(&(r * 4).to_le_bytes());
            }
            tail.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&rel_pc.to_le_bytes());
            out.extend_from_slice(&(tail.len() as u32).to_le_bytes());
            out.extend_from_slice(&tail);
            while out.len() % 4 != 0 {
                out.push(0);
            }
        }
        out
    }

    /// The synthesized map as `(key, tail)` rows, read back out of a linked image.
    fn decode_gcmap(img: &LinkedImage) -> Vec<(u32, Vec<u8>)> {
        let (_, at) = img
            .symbols
            .iter()
            .find(|(n, _)| n == STACKMAP_BLOB_SYMBOL)
            .expect("the linker defines the stack-map blob")
            .clone();
        let blob = &img.text[at as usize..];
        let count = u32::from_le_bytes(blob[0..4].try_into().unwrap()) as usize;
        let mut rows = Vec::new();
        let mut off = 4;
        for _ in 0..count {
            let key = u32::from_le_bytes(blob[off..off + 4].try_into().unwrap());
            let nrefs = u16::from_le_bytes(blob[off + 8..off + 10].try_into().unwrap()) as usize;
            let ntag_at = off + 10 + nrefs * 2;
            let ntagged = u16::from_le_bytes(blob[ntag_at..ntag_at + 2].try_into().unwrap()) as usize;
            let end = ntag_at + 2 + ntagged * 2;
            rows.push((key, blob[off + 4..end].to_vec()));
            off = end;
        }
        rows
    }

    /// THE INVARIANT THE RETURN-ADDRESS MAP NEVER HAD A GUARD FOR, AND THE ONE THAT MATTERS:
    /// every key must be an offset INSIDE some surviving function, because a key is what a collector
    /// matches a return address against. A map whose keys are anything else -- label ids, pre-layout
    /// offsets, offsets into a text that dead-stripping has since repacked -- looks perfectly
    /// well-formed and finds no frame.
    #[test]
    fn every_stack_map_key_lands_inside_a_surviving_function() {
        let text = alloc::vec![0u8; 16];
        let prog = obj_arm_gcmap(
            &text,
            &[func("f0", 1, 8), func("f1", 9, 8)],
            &[],
            &[
                gcmap_fragment("f0", &[(4, 1)]),
                gcmap_fragment("f1", &[(0, 0), (4, 2)]),
            ]
            .concat(),
        );
        let img = link_at_base(&[prog], "f0", 0x1000).unwrap();
        let extents: Vec<(u32, u32)> = alloc::vec![(0, 8), (8, 16)];
        let rows = decode_gcmap(&img);
        assert_eq!(rows.len(), 3, "three safepoints across two functions");
        for (key, _) in &rows {
            assert!(
                extents.iter().any(|&(lo, hi)| *key >= lo && *key < hi),
                "key {key} is not inside any function extent {extents:?}"
            );
        }
        assert!(
            rows.windows(2).all(|w| w[0].0 <= w[1].0),
            "the map is sorted for the collector's binary search"
        );
        assert_eq!(
            rows.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            alloc::vec![4, 8, 12],
            "f0+4, f1+0, f1+4 against f1 laid at 8"
        );
    }

    /// The recovery this move exists for: a function that does NOT survive dead-stripping pays
    /// nothing for the safepoints the compiler saw in it. Under the old emission the map was built
    /// from every DECLARED method before the linker had dropped anything.
    #[test]
    fn a_dead_functions_safepoints_are_not_in_the_map() {
        let text = alloc::vec![0u8; 16];
        let live = obj_arm_gcmap(
            &text,
            &[func("f0", 1, 8), func("dead0", 9, 8)],
            &[],
            &[
                gcmap_fragment("f0", &[(4, 1)]),
                gcmap_fragment("dead0", &[(0, 3), (4, 1)]),
            ]
            .concat(),
        );
        let with_dead = decode_gcmap(&link_at_base(&[live.clone()], "f0", 0x1000).unwrap());
        assert_eq!(with_dead.len(), 3, "nothing stripped yet: all three entries");

        let trimmed = garbage_collect(&[live], "f0");
        assert!(
            !trimmed[0].symbols.iter().any(|s| s.name == "dead0"),
            "dead0 is unreachable from f0 and should be stripped"
        );
        let rows = decode_gcmap(&link_at_base(&trimmed, "f0", 0x1000).unwrap());
        assert_eq!(
            rows.len(),
            1,
            "only the surviving function's safepoint is in the map"
        );
        assert_eq!(rows[0].0, 4, "and it is keyed at f0+4");
    }

    /// The fragments are an INPUT: they are not `SHF_ALLOC`, they are not carried out as a debug
    /// section, and the only thing the image gets from them is the synthesized map.
    #[test]
    fn gcmap_fragments_do_not_reach_the_image() {
        let text = alloc::vec![0u8; 8];
        let prog = obj_arm_gcmap(
            &text,
            &[func("f0", 1, 8)],
            &[],
            &gcmap_fragment("f0", &[(4, 1)]),
        );
        let img = link_at_base(&[prog], "f0", 0x1000).unwrap();
        assert!(
            !img.debug_sections
                .iter()
                .any(|(n, _)| n == STACKMAP_GCMAP_SECTION),
            "fragments are consumed by synthesis, not carried through"
        );
        assert_eq!(img.text.len(), 8 + 4 + 14);
        assert!(
            img.symbols.iter().any(|(n, _)| n == TEXT_BASE_SYMBOL),
            "the linker defines the base a collector subtracts"
        );
        assert_eq!(
            img.symbols
                .iter()
                .find(|(n, _)| n == TEXT_BASE_SYMBOL)
                .unwrap()
                .1,
            0,
            "and it is the image's .text start"
        );
    }

    /// A minimal 16-byte METHOD_SLOTS stack-map record: `[func_addr=0][code_size][mode=1]`
    /// `[frame_words][ret_lr_word][root_count=0]` -- enough for the linker-side rules, which never
    /// parse a record's body (they act on the symbol name and its relocation).
    fn smrec_bytes(code_size: u32, frame_words: u16) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&code_size.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&frame_words.to_le_bytes());
        out.extend_from_slice(&(frame_words - 1).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    #[test]
    fn statics_regions_lay_out_entry_first_with_eh_alias_and_brackets() {
        let lib = obj_arm(
            &[0x70, 0x47, 0x00, 0x00, 0, 0, 0, 0],
            &[
                func("g", 1, 4),
                undef_sized("__lamella_statics_bbbbbbbb", 8),
            ],
            &[Relocation {
                offset: 4,
                symbol: 1,
                kind: arm::R_ARM_ABS32,
                addend: 4,
            }],
        );
        let prog = obj_arm(
            &[0x70, 0x47, 0x00, 0x00, 0, 0, 0, 0, 0, 0, 0, 0],
            &[
                func("f0", 1, 4),
                undef_sized("__lamella_statics_aaaaaaaa", 12),
                undef("__lamella_eh_tag"),
            ],
            &[
                Relocation {
                    offset: 4,
                    symbol: 1,
                    kind: arm::R_ARM_ABS32,
                    addend: 8,
                },
                Relocation {
                    offset: 8,
                    symbol: 2,
                    kind: arm::R_ARM_ABS32,
                    addend: 0,
                },
            ],
        );
        let base = 0x100u32;
        let img = link_at_base(&[lib, prog], "f0", base).unwrap();
        let addr = |name: &str| {
            img.symbols
                .iter()
                .find(|(n, _)| n == name)
                .map(|&(_, off)| off.wrapping_add(base))
                .unwrap_or_else(|| panic!("{name} is defined"))
        };
        assert_eq!(addr("__lamella_statics_aaaaaaaa"), 0x2000_1000, "entry first");
        assert_eq!(addr("__lamella_statics_bbbbbbbb"), 0x2000_100C);
        assert_eq!(addr(EH_TAG_SYMBOL), 0x2000_1000, "EH = entry region word 0");
        assert_eq!(addr(STATICS_START_SYMBOL), 0x2000_1000);
        assert_eq!(addr(STATICS_END_SYMBOL), 0x2000_1014, "12 + 8 bytes spanned");
        let word = |off: usize| {
            u32::from_le_bytes([
                img.text[off],
                img.text[off + 1],
                img.text[off + 2],
                img.text[off + 3],
            ])
        };
        assert_eq!(word(4), 0x2000_100C + 4, "lib slot 1 -> ITS region + 4");
        assert_eq!(word(8 + 4), 0x2000_1000 + 8, "prog slot 2 -> its region + 8");
        assert_eq!(word(8 + 8), 0x2000_1000, "prog throw/catch -> the EH word");
    }

    #[test]
    fn statics_window_overflow_fails_loud() {
        let prog = obj_arm(
            &[0x70, 0x47, 0x00, 0x00, 0, 0, 0, 0],
            &[
                func("f0", 1, 4),
                undef_sized("__lamella_statics_aaaaaaaa", 0x2000),
            ],
            &[Relocation {
                offset: 4,
                symbol: 1,
                kind: arm::R_ARM_ABS32,
                addend: 0,
            }],
        );
        assert!(matches!(
            link_at_base(&[prog], "f0", 0x100),
            Err(LinkError::StaticsOverflow {
                needed: 0x2000,
                cap: 0x1000
            })
        ));
    }

    #[test]
    fn gc_trim_preserves_the_region_reference_size() {
        let prog = obj_arm(
            &[0x70, 0x47, 0x00, 0x00, 0, 0, 0, 0],
            &[
                func("f0", 1, 8),
                undef_sized("__lamella_statics_aaaaaaaa", 24),
            ],
            &[Relocation {
                offset: 4,
                symbol: 1,
                kind: arm::R_ARM_ABS32,
                addend: 0,
            }],
        );
        let trimmed = garbage_collect(&[prog], "f0");
        let region = trimmed[0]
            .symbols
            .iter()
            .find(|s| s.name == "__lamella_statics_aaaaaaaa")
            .expect("the region reference survives the trim");
        assert!(!region.defined);
        assert_eq!(region.size, 24, "st_size survives the trim");
        let img = link_at_base(&trimmed, "f0", 0x100).unwrap();
        assert!(
            img.symbols
                .iter()
                .any(|(n, _)| n == "__lamella_statics_aaaaaaaa")
        );
    }

    #[test]
    fn stackmap_records_live_and_die_with_their_function() {
        let mut text = vec![0x70, 0x47, 0x00, 0x00, 0x70, 0x47, 0x00, 0x00];
        text.extend_from_slice(&smrec_bytes(4, 1));
        text.extend_from_slice(&smrec_bytes(4, 1));
        let o = obj_arm(
            &text,
            &[
                func("f0", 1, 4),
                func("dead0", 5, 4),
                weak_data("__lamella_smrec_f0", 8, 16),
                weak_data("__lamella_smrec_dead0", 24, 16),
            ],
            &[
                Relocation {
                    offset: 8,
                    symbol: 0,
                    kind: arm::R_ARM_ABS32,
                    addend: 0,
                },
                Relocation {
                    offset: 24,
                    symbol: 1,
                    kind: arm::R_ARM_ABS32,
                    addend: 0,
                },
            ],
        );
        let trimmed = garbage_collect(&[o], "f0");
        let names: Vec<&str> = trimmed[0]
            .symbols
            .iter()
            .filter(|s| s.defined)
            .map(|s| s.name.as_str())
            .collect();
        assert!(names.contains(&"__lamella_smrec_f0"), "{names:?}");
        assert!(!names.contains(&"__lamella_smrec_dead0"), "{names:?}");
        assert!(!names.contains(&"dead0"), "{names:?}");
        let img = link_at_base(&trimmed, "f0", 0x100).unwrap();
        assert!(img.symbols.iter().any(|(n, _)| n == "__lamella_smrec_f0"));
    }

    #[test]
    fn stackmap_table_gathers_and_brackets_records() {
        let mut text = vec![0x70, 0x47, 0x00, 0x00];
        text.extend_from_slice(&smrec_bytes(4, 1));
        let o = obj_arm(
            &text,
            &[func("f0", 1, 4), weak_data("__lamella_smrec_f0", 4, 16)],
            &[Relocation {
                offset: 4,
                symbol: 0,
                kind: arm::R_ARM_ABS32,
                addend: 0,
            }],
        );
        let base = 0x100u32;
        let img = link_at_base(&[o], "f0", base).unwrap();
        let addr_of = |name: &str| {
            img.symbols
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, a)| *a)
                .unwrap()
        };
        let start = addr_of(STACKMAP_START_SYMBOL);
        let end = addr_of(STACKMAP_END_SYMBOL);
        let word = |off: u32| {
            let i = off as usize;
            u32::from_le_bytes([img.text[i], img.text[i + 1], img.text[i + 2], img.text[i + 3]])
        };
        assert_eq!(end - start, 8, "count word + one pointer");
        assert_eq!(word(start), 1, "one gathered record");
        let record_addr = word(start + 4);
        assert_eq!(record_addr, base + addr_of("__lamella_smrec_f0"));
        assert_eq!(word(record_addr - base), base | 1);
    }

    #[test]
    fn resolves_an_arm_thumb_call_across_two_objects() {
        let caller = obj_arm(
            &[0x00, 0xF0, 0x00, 0xD0, 0x70, 0x47],
            &[func("caller", 1, 6), undef("answer")],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            }],
        );
        let answer = obj_arm(&[0x2A, 0x20, 0x70, 0x47], &[func("answer", 1, 4)], &[]);
        let img = link(&[caller, answer], "caller").unwrap();
        assert_eq!(img.entry_offset, 0);
        assert_eq!(&img.text[0..4], &[0x00, 0xF0, 0x02, 0xF8]);
        assert!(img.symbols.iter().any(|(n, a)| n == "answer" && *a == 8));
    }

    /// The Thumb bit belongs to FUNCTION symbols. A data, section or untyped symbol whose value
    /// happens to be odd is just an odd address: `.rodata.str1.1` is byte-aligned and mergeable, so
    /// a string constant lands at an odd offset as a matter of course.
    ///
    /// Masking the low bit away and ORing it back is the identity only when the addend is EVEN.
    /// With an odd addend the reference lands ONE BYTE LOW, which for a `core::fmt` template is a
    /// leading NUL -- the formatter reads an empty template and writes nothing.
    ///
    /// Both rows assert the written word is `S + 1` AND that it is EVEN. The evenness is what pins
    /// the fixture: it holds only while `S` is odd, so a layout change that moved the data to an
    /// even address would fail here rather than pass without reproducing anything.
    #[test]
    fn an_odd_addressed_non_function_symbol_does_not_take_the_thumb_bit() {
        let mut text = vec![0x70, 0x47, 0x00, 0x00];
        text.extend_from_slice(&[0, 0, 0, 0]);
        text.extend_from_slice(&[0, 0, 0, 0]);
        text.push(b'.');
        text.extend_from_slice(b"ab\0");
        text.push(b'.');
        text.extend_from_slice(b"cd\0");
        assert_eq!(text.len(), 20);
        let section_sym = Symbol {
            name: "",
            value: 13,
            size: 0,
            binding: Binding::Local,
            kind: SymbolType::Section,
            section: SymbolSection::Text,
        };
        let obj = obj_arm(
            &text,
            &[func("f0", 1, 4), section_sym, data("str_pool", 17, 3)],
            &[
                Relocation { offset: 4, symbol: 1, kind: arm::R_ARM_ABS32, addend: 1 },
                Relocation { offset: 8, symbol: 2, kind: arm::R_ARM_ABS32, addend: 1 },
            ],
        );
        let base = 0x0800_0000u32;
        let img = link_at_base(&[obj], "f0", base).unwrap();
        let word = |off: usize| {
            u32::from_le_bytes([
                img.text[off],
                img.text[off + 1],
                img.text[off + 2],
                img.text[off + 3],
            ])
        };
        for (site, symbol_value, what) in [(4usize, 13u32, "section symbol"), (8, 17, "data symbol")]
        {
            assert_eq!(symbol_value % 2, 1, "the fixture places the {what} at an odd address");
            let want = base + symbol_value + 1;
            assert_eq!(word(site), want, "{what} + addend 1 is S + 1, not S");
            assert_eq!(word(site) % 2, 0, "{what} + addend 1 is EVEN -- no Thumb bit on data");
        }
        assert!(img.symbols.iter().any(|(n, a)| n == "f0" && *a == 0));
    }

    #[test]
    fn gc_sections_drops_unreached_functions() {
        let main = obj_arm(
            &[0x00, 0xF0, 0x00, 0xD0, 0x70, 0x47, 0x70, 0x47],
            &[func("f0", 1, 6), func("unused0", 7, 2), undef("keep")],
            &[Relocation {
                offset: 0,
                symbol: 2,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            }],
        );
        let lib = obj_arm(
            &[0x70, 0x47, 0x70, 0x47],
            &[func("keep", 1, 2), func("unused1", 3, 2)],
            &[],
        );
        let trimmed = garbage_collect(&[main, lib], "f0");
        let defined: Vec<&str> = trimmed
            .iter()
            .flat_map(|o| &o.symbols)
            .filter(|s| s.defined && !s.name.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(defined.contains(&"f0"), "the entry is kept");
        assert!(defined.contains(&"keep"), "a reached function is kept");
        assert!(!defined.contains(&"unused0"), "an unreached function is dropped");
        assert!(!defined.contains(&"unused1"), "an unreached library function is dropped");
    }

    #[test]
    fn gc_sections_follows_a_reached_descriptor_and_drops_unreached_ones() {
        let obj = obj_arm(
            &[0u8; 16],
            &[
                func("f0", 1, 4),
                func("m", 5, 2),
                func("dead", 7, 2),
                data("__lamella_typedesc_1", 8, 4),
                data("__lamella_typedesc_2", 12, 4),
            ],
            &[
                Relocation { offset: 0, symbol: 3, kind: arm::R_ARM_ABS32, addend: 0 },
                Relocation { offset: 8, symbol: 1, kind: arm::R_ARM_ABS32, addend: 0 },
                Relocation { offset: 12, symbol: 2, kind: arm::R_ARM_ABS32, addend: 0 },
            ],
        );
        let trimmed = garbage_collect(&[obj], "f0");
        let defined: Vec<&str> = trimmed
            .iter()
            .flat_map(|o| &o.symbols)
            .filter(|s| s.defined && !s.name.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(defined.contains(&"f0"), "the entry is kept");
        assert!(defined.contains(&"__lamella_typedesc_1"), "a reached descriptor is kept");
        assert!(defined.contains(&"m"), "a method reached only through a descriptor's reloc is kept");
        assert!(!defined.contains(&"__lamella_typedesc_2"), "an unreached descriptor is dropped");
        assert!(!defined.contains(&"dead"), "a method only an unreached descriptor referenced drops out");
    }

    #[test]
    fn gc_sections_follows_every_copy_of_a_multiply_defined_descriptor() {
        let weak_desc = |name: &'static str, value: u32| Symbol {
            name,
            value,
            size: 4,
            binding: Binding::Weak,
            kind: SymbolType::NoType,
            section: SymbolSection::Text,
        };
        let prog = obj_arm(
            &[0u8; 8],
            &[func("f0", 1, 4), data("__lamella_typedesc_9", 4, 4)],
            &[Relocation { offset: 0, symbol: 1, kind: arm::R_ARM_ABS32, addend: 0 }],
        );
        let corlib = obj_arm(
            &[0u8; 8],
            &[
                weak_desc("__lamella_typedesc_9", 0),
                weak_desc("__lamella_typedesc_5", 4),
            ],
            &[Relocation { offset: 0, symbol: 1, kind: arm::R_LAMELLA_REL_DESC, addend: 12 }],
        );
        let trimmed = garbage_collect(&[prog, corlib], "f0");
        let defined: Vec<&str> = trimmed
            .iter()
            .flat_map(|o| &o.symbols)
            .filter(|s| s.defined && !s.name.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(defined.contains(&"f0"), "the entry is kept");
        assert!(defined.contains(&"__lamella_typedesc_9"), "the reached (shared) descriptor is kept");
        assert!(
            defined.contains(&"__lamella_typedesc_5"),
            "the base descriptor reached ONLY through corlib's copy of the shared descriptor is kept"
        );
    }

    #[test]
    fn thm_movw_movt_roundtrip() {
        for addr in [
            0x0000_0000u32,
            0x1234_5678,
            0xFFFF_FFFF,
            0x1000_0100,
            0xDEAD_BEEF,
            0x0000_F800,
            0x8001_0001,
        ] {
            let mut movw = [0x40u8, 0xF2, 0x00, 0x03];
            let mut movt = [0xC0u8, 0xF2, 0x00, 0x03];
            encode_thm_mov(&mut movw, 0, (addr & 0xFFFF) as u16).unwrap();
            encode_thm_mov(&mut movt, 0, ((addr >> 16) & 0xFFFF) as u16).unwrap();
            let lo = extract_thm_mov(&movw, 0) as u32;
            let hi = extract_thm_mov(&movt, 0) as u32;
            assert_eq!((hi << 16) | lo, addr, "movw/movt roundtrip {addr:#010x}");
            assert_eq!(movw[3] & 0x0F, 0x03, "movw Rd preserved");
            assert_eq!(movt[3] & 0x0F, 0x03, "movt Rd preserved");
            assert_eq!(movw[0] & 0x80, 0x00, "movw opcode preserved");
            assert_eq!(movt[0] & 0x80, 0x80, "movt opcode preserved");
        }
    }

    #[test]
    fn thm_branch_preserves_bl_vs_bw_opcode() {
        let off = 0x1234;
        let mut bl = [0x00u8, 0xF0, 0x00, 0xD0];
        let mut bw = [0x00u8, 0xF0, 0x00, 0x90];
        encode_thm_call(&mut bl, 0, off).unwrap();
        encode_thm_call(&mut bw, 0, off).unwrap();
        assert_eq!(extract_thm_call(&bl, 0), off, "BL offset");
        assert_eq!(extract_thm_call(&bw, 0), off, "B.W offset");
        let bl_hw2 = u16::from_le_bytes([bl[2], bl[3]]);
        let bw_hw2 = u16::from_le_bytes([bw[2], bw[3]]);
        assert_eq!(bl_hw2 & 0x4000, 0x4000, "BL opcode (hw2 bit 14) preserved");
        assert_eq!(bw_hw2 & 0x4000, 0x0000, "B.W opcode (hw2 bit 14) preserved");
    }

    #[test]
    fn gc_sections_remaps_a_reloc_into_the_smallest_covering_span() {
        let obj = obj_arm(
            &[0u8; 12],
            &[
                func("f0", 1, 4),
                data("outer", 4, 8),
                data("__lamella_typedesc_1", 8, 4),
            ],
            &[
                Relocation { offset: 0, symbol: 2, kind: arm::R_ARM_ABS32, addend: 0 },
                Relocation { offset: 8, symbol: 0, kind: arm::R_ARM_ABS32, addend: 0 },
            ],
        );
        let trimmed = garbage_collect(&[obj], "f0");
        let desc = trimmed[0]
            .symbols
            .iter()
            .find(|s| s.name == "__lamella_typedesc_1")
            .expect("the reached descriptor is kept");
        assert!(
            trimmed[0].relocations.iter().any(|r| r.offset == (desc.value & !1)),
            "the descriptor's relocation must land in the descriptor symbol's own copy \
             (offset {:#x}), got {:?}",
            desc.value & !1,
            trimmed[0].relocations.iter().map(|r| r.offset).collect::<Vec<_>>()
        );
    }

    #[test]
    fn gc_sections_keeps_non_descriptor_data_wholesale() {
        let obj = obj_arm(&[0u8; 8], &[func("f0", 1, 4), data("rodata_blob", 4, 4)], &[]);
        let trimmed = garbage_collect(&[obj], "f0");
        let defined: Vec<&str> = trimmed
            .iter()
            .flat_map(|o| &o.symbols)
            .filter(|s| s.defined && !s.name.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(defined.contains(&"f0"), "the entry is kept");
        assert!(defined.contains(&"rodata_blob"), "unreached non-descriptor data is kept wholesale");
    }

    /// DATA KEPT WHOLESALE IS ALSO A REACHABILITY ROOT, OR IT SURVIVES POINTING AT NOTHING.
    ///
    /// The test above pins that unreached non-descriptor data is KEPT. This pins the consequence
    /// that rule has for the WALK: such a symbol carries its relocations with it, so anything it
    /// names must be kept too. A string literal blob holds a relocation to `System.String`'s type
    /// DESCRIPTOR -- and descriptors are the one data kind collected by reachability -- so before
    /// the seed, the blob survived and its descriptor did not, which is an undefined symbol at
    /// link time.
    ///
    /// **`f0` deliberately does NOT reference either symbol.** That is what makes this the
    /// kept-but-unreached case rather than an ordinary reachable one; with a reference from `f0`
    /// the descriptor would be kept for the wrong reason and the row would pass either way.
    ///
    /// Third instance of one bug class in `trim_object` -- descriptors, stack-map records, and now
    /// a literal blob -- which is why the fix is a shared predicate (`kept_regardless`) rather than
    /// a third special case.
    #[test]
    fn data_kept_wholesale_roots_the_descriptor_it_points_at() {
        let obj = obj_arm(
            &[0u8; 16],
            &[
                func("f0", 1, 4),
                data("__lamella_str_0", 4, 4),
                data("__lamella_typedesc_9", 8, 8),
            ],
            &[Relocation { offset: 4, symbol: 2, kind: arm::R_ARM_ABS32, addend: 0 }],
        );
        let trimmed = garbage_collect(&[obj], "f0");
        let defined: Vec<&str> = trimmed
            .iter()
            .flat_map(|o| &o.symbols)
            .filter(|s| s.defined && !s.name.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(defined.contains(&"__lamella_str_0"), "the blob is kept wholesale, as before");
        assert!(
            defined.contains(&"__lamella_typedesc_9"),
            "the descriptor the kept blob points at must be kept too -- otherwise the blob \
             survives with a dangling relocation and the link fails on an undefined symbol. \
             kept: {defined:?}"
        );
    }

    /// The control for the row above: a descriptor NOTHING points at is still collected. Without
    /// this, seeding every kept-wholesale symbol could have degenerated into "keep all descriptors"
    /// and the row above would pass for a reason that costs flash in every image.
    #[test]
    fn an_unreferenced_descriptor_is_still_collected() {
        let obj = obj_arm(
            &[0u8; 16],
            &[
                func("f0", 1, 4),
                data("__lamella_str_0", 4, 4),
                data("__lamella_typedesc_9", 8, 8),
            ],
            &[],
        );
        let trimmed = garbage_collect(&[obj], "f0");
        let defined: Vec<&str> = trimmed
            .iter()
            .flat_map(|o| &o.symbols)
            .filter(|s| s.defined && !s.name.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(defined.contains(&"__lamella_str_0"), "the blob is still kept wholesale");
        assert!(
            !defined.contains(&"__lamella_typedesc_9"),
            "a descriptor nothing references is still collectable -- the seed must root what kept \
             data POINTS AT, not every descriptor. kept: {defined:?}"
        );
    }

    #[test]
    fn two_weak_definitions_resolve_without_conflict() {
        let caller = obj_arm(
            &[0x00, 0xF0, 0x00, 0xD0, 0x70, 0x47],
            &[func("caller", 1, 6), undef("helper")],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            }],
        );
        let weak_a = obj_arm(&[0x2A, 0x20, 0x70, 0x47], &[weak("helper", 1, 4)], &[]);
        let weak_b = obj_arm(&[0x2B, 0x20, 0x70, 0x47], &[weak("helper", 1, 4)], &[]);
        let img =
            link(&[caller, weak_a, weak_b], "caller").expect("two weak defs resolve, no conflict");
        assert!(img.symbols.iter().any(|(n, _)| n == "helper"), "weak helper defined");
    }

    #[test]
    fn a_strong_definition_overrides_a_weak_one() {
        let caller = obj_arm(
            &[0x00, 0xF0, 0x00, 0xD0, 0x70, 0x47],
            &[func("caller", 1, 6), undef("helper")],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            }],
        );
        let weak_h = obj_arm(&[0x2A, 0x20, 0x70, 0x47], &[weak("helper", 1, 4)], &[]);
        let strong_h = obj_arm(&[0x2B, 0x20, 0x70, 0x47], &[func("helper", 1, 4)], &[]);
        let img = link(&[caller, weak_h, strong_h], "caller").expect("strong overrides weak");
        let (_, addr) = img
            .symbols
            .iter()
            .find(|(n, _)| n == "helper")
            .expect("helper defined");
        assert_eq!(
            img.text[(*addr & !1) as usize],
            0x2B,
            "the strong (global) definition wins over the weak one"
        );
    }

    #[test]
    fn resolves_an_arm_abs32_data_reference() {
        let holder = obj_arm(
            &[0, 0, 0, 0],
            &[func("holder", 1, 4), undef("answer")],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: arm::R_ARM_ABS32,
                addend: 0,
            }],
        );
        let answer = obj_arm(&[0x2A, 0x20, 0x70, 0x47], &[func("answer", 1, 4)], &[]);
        let img = link_at_base(&[holder, answer], "holder", 0x8000).unwrap();
        let word = u32::from_le_bytes([img.text[0], img.text[1], img.text[2], img.text[3]]);
        assert_eq!(
            word,
            (0x8000 + 4) | 1,
            "R_ARM_ABS32 = answer's vaddr | Thumb bit"
        );
        let rebuilt = || {
            obj_arm(
                &[0, 0, 0, 0],
                &[func("holder", 1, 4), undef("answer")],
                &[Relocation {
                    offset: 0,
                    symbol: 1,
                    kind: arm::R_ARM_ABS32,
                    addend: 0,
                }],
            )
        };
        let leaf = obj_arm(&[0x2A, 0x20, 0x70, 0x47], &[func("answer", 1, 4)], &[]);
        assert_eq!(
            link(&[rebuilt(), leaf], "holder").unwrap_err(),
            LinkError::AbsoluteNeedsBase
        );
    }

    #[test]
    fn resolves_a_resident_call_for_ram_injection() {
        let snippet = || {
            obj_arm(
                &[0x00, 0xF0, 0x00, 0xD0, 0x70, 0x47],
                &[func("snippet", 1, 6), undef("lamella_gc_alloc")],
                &[Relocation {
                    offset: 0,
                    symbol: 1,
                    kind: arm::R_ARM_THM_CALL,
                    addend: -4,
                }],
            )
        };
        let img = link_at_base_with_residents(
            &[snippet()],
            "snippet",
            0x2000_0000,
            &[("lamella_gc_alloc", 0x2000_0009)],
        )
        .unwrap();
        assert_eq!(&img.text[0..4], &[0x00, 0xF0, 0x02, 0xF8]);
        assert_eq!(
            link_at_base(&[snippet()], "snippet", 0x2000_0000).unwrap_err(),
            LinkError::UndefinedSymbol(String::from("lamella_gc_alloc"))
        );
    }

    #[test]
    fn a_far_resident_call_gets_a_veneer() {
        let snippet = obj_arm(
            &[0x00, 0xF0, 0x00, 0xD0, 0x70, 0x47],
            &[func("snippet", 1, 6), undef("lamella_gc_alloc")],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            }],
        );
        let img = link_at_base_with_residents(
            &[snippet],
            "snippet",
            0x2000_0000,
            &[("lamella_gc_alloc", 0x0800_0001)],
        )
        .unwrap();
        assert_eq!(&img.text[0..4], &[0x00, 0xF0, 0x02, 0xF8]);
        assert_eq!(
            &img.text[8..20],
            &[
                0x01, 0xB4, 0x02, 0x48, 0x84, 0x46, 0x01, 0xBC, 0x60, 0x47, 0x00, 0xBF
            ]
        );
        let literal = u32::from_le_bytes([img.text[20], img.text[21], img.text[22], img.text[23]]);
        assert_eq!(literal, 0x0800_0001);
    }

    #[test]
    fn rejects_objects_targeting_different_machines() {
        let rv = obj(&[0x13, 0x05, 0xa0, 0x02], &[func("a", 0, 4)], &[]);
        let arm = obj_arm(&[0x70, 0x47], &[func("b", 1, 2)], &[]);
        assert_eq!(link(&[rv, arm], "a").unwrap_err(), LinkError::MixedMachines);
    }

    #[test]
    fn resolves_an_external_call_across_two_objects() {
        let answer = obj(
            &[0x13, 0x05, 0xa0, 0x02, 0x67, 0x80, 0x00, 0x00],
            &[Symbol {
                name: "answer",
                value: 0,
                size: 8,
                binding: Binding::Global,
                kind: SymbolType::Func,
                section: SymbolSection::Text,
            }],
            &[],
        );
        let caller = obj(
            &[0x97, 0x00, 0x00, 0x00, 0xe7, 0x80, 0x00, 0x00],
            &[
                Symbol {
                    name: "caller",
                    value: 0,
                    size: 8,
                    binding: Binding::Global,
                    kind: SymbolType::Func,
                    section: SymbolSection::Text,
                },
                Symbol {
                    name: "answer",
                    value: 0,
                    size: 0,
                    binding: Binding::Global,
                    kind: SymbolType::NoType,
                    section: SymbolSection::Undefined,
                },
            ],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: riscv::R_RISCV_CALL_PLT,
                addend: 0,
            }],
        );
        let img = link(&[caller, answer], "caller").unwrap();
        assert_eq!(img.entry_offset, 0);
        let auipc = u32::from_le_bytes([img.text[0], img.text[1], img.text[2], img.text[3]]);
        let jalr = u32::from_le_bytes([img.text[4], img.text[5], img.text[6], img.text[7]]);
        assert_eq!(auipc, 0x0000_0097);
        assert_eq!(jalr, 0x0080_80e7);
        assert_eq!(
            &img.text[8..16],
            &[0x13, 0x05, 0xa0, 0x02, 0x67, 0x80, 0x00, 0x00]
        );
    }

    #[test]
    fn resolves_riscv_absolute_hi20_lo12_address_materialization() {
        let loader = obj(
            &[
                0x37, 0x05, 0x00, 0x00,
                0x13, 0x05, 0x05, 0x00,
                0x23, 0x20, 0xa1, 0x00,
                0x67, 0x80, 0x00, 0x00,
            ],
            &[
                Symbol {
                    name: "_start",
                    value: 0,
                    size: 16,
                    binding: Binding::Global,
                    kind: SymbolType::Func,
                    section: SymbolSection::Text,
                },
                Symbol {
                    name: "sym",
                    value: 0,
                    size: 0,
                    binding: Binding::Global,
                    kind: SymbolType::NoType,
                    section: SymbolSection::Undefined,
                },
            ],
            &[
                Relocation { offset: 0, symbol: 1, kind: riscv::R_RISCV_HI20, addend: 0 },
                Relocation { offset: 4, symbol: 1, kind: riscv::R_RISCV_LO12_I, addend: 0 },
                Relocation { offset: 8, symbol: 1, kind: riscv::R_RISCV_LO12_S, addend: 0 },
            ],
        );
        let sym = obj(
            &[0x67, 0x80, 0x00, 0x00],
            &[Symbol {
                name: "sym",
                value: 0,
                size: 4,
                binding: Binding::Global,
                kind: SymbolType::Func,
                section: SymbolSection::Text,
            }],
            &[],
        );
        let img = link_at_base(&[loader, sym], "_start", 0x8000_0000).unwrap();
        let insn = |o: usize| u32::from_le_bytes([img.text[o], img.text[o + 1], img.text[o + 2], img.text[o + 3]]);
        assert_eq!(insn(0), 0x8000_0537, "lui a0, 0x80000");
        assert_eq!(insn(4), 0x0105_0513, "addi a0, a0, 16");
        assert_eq!(insn(8), 0x00a1_2823, "sw a0, 16(sp)");
    }

    #[test]
    fn resolves_a_riscv_rel_desc_data_word() {
        let desc = obj(
            &[0xAA, 0xBB, 0xCC, 0xDD],
            &[Symbol {
                name: "desc",
                value: 0,
                size: 4,
                binding: Binding::Global,
                kind: SymbolType::NoType,
                section: SymbolSection::Text,
            }],
            &[],
        );
        let caller = obj(
            &[0, 0, 0, 0],
            &[
                Symbol {
                    name: "caller",
                    value: 0,
                    size: 4,
                    binding: Binding::Global,
                    kind: SymbolType::Func,
                    section: SymbolSection::Text,
                },
                Symbol {
                    name: "desc",
                    value: 0,
                    size: 0,
                    binding: Binding::Global,
                    kind: SymbolType::NoType,
                    section: SymbolSection::Undefined,
                },
            ],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: riscv::R_LAMELLA_REL_DESC,
                addend: 12,
            }],
        );
        let img = link(&[caller, desc], "caller").unwrap();
        let slot = u32::from_le_bytes([img.text[0], img.text[1], img.text[2], img.text[3]]);
        assert_eq!(slot, 16, "REL_DESC resolves to S + A - P (4 + 12 - 0)");
    }

    #[test]
    fn gc_sections_drops_an_unreferenced_object() {
        let answer = obj(
            &[0x13, 0x05, 0xa0, 0x02, 0x67, 0x80, 0x00, 0x00],
            &[Symbol {
                name: "answer",
                value: 0,
                size: 8,
                binding: Binding::Global,
                kind: SymbolType::Func,
                section: SymbolSection::Text,
            }],
            &[],
        );
        let caller = obj(
            &[0x97, 0x00, 0x00, 0x00, 0xe7, 0x80, 0x00, 0x00],
            &[
                Symbol {
                    name: "caller",
                    value: 0,
                    size: 8,
                    binding: Binding::Global,
                    kind: SymbolType::Func,
                    section: SymbolSection::Text,
                },
                Symbol {
                    name: "answer",
                    value: 0,
                    size: 0,
                    binding: Binding::Global,
                    kind: SymbolType::NoType,
                    section: SymbolSection::Undefined,
                },
            ],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: riscv::R_RISCV_CALL_PLT,
                addend: 0,
            }],
        );
        let unused = obj(
            &[0x13, 0x05, 0x00, 0x00, 0x67, 0x80, 0x00, 0x00],
            &[Symbol {
                name: "unused",
                value: 0,
                size: 8,
                binding: Binding::Global,
                kind: SymbolType::Func,
                section: SymbolSection::Text,
            }],
            &[],
        );
        let full = link(&[caller.clone(), answer.clone(), unused.clone()], "caller").unwrap();
        let gc = link_gc(&[caller, answer, unused], "caller").unwrap();
        assert!(
            gc.text.len() < full.text.len(),
            "gc must drop the unused object's code"
        );
        assert!(gc.symbols.iter().any(|(n, _)| n == "caller"));
        assert!(gc.symbols.iter().any(|(n, _)| n == "answer"));
        assert!(!gc.symbols.iter().any(|(n, _)| n == "unused"));
    }

    #[test]
    fn an_unresolved_call_is_an_error() {
        let caller = obj(
            &[0x97, 0x00, 0x00, 0x00, 0xe7, 0x80, 0x00, 0x00],
            &[
                Symbol {
                    name: "caller",
                    value: 0,
                    size: 8,
                    binding: Binding::Global,
                    kind: SymbolType::Func,
                    section: SymbolSection::Text,
                },
                Symbol {
                    name: "missing",
                    value: 0,
                    size: 0,
                    binding: Binding::Global,
                    kind: SymbolType::NoType,
                    section: SymbolSection::Undefined,
                },
            ],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: riscv::R_RISCV_CALL_PLT,
                addend: 0,
            }],
        );
        assert_eq!(
            link(&[caller], "caller").unwrap_err(),
            LinkError::UndefinedSymbol(String::from("missing"))
        );
    }

    #[test]
    fn gc_drops_an_unreached_functions_intrinsic_demand() {
        let object = obj_arm(
            &[
                0x70, 0x47,
                0x00, 0xF0, 0x00, 0xD0,
                0x70, 0x47,
            ],
            &[
                func("f0", 1, 2),
                func("unreached", 3, 6),
                undef("exotic_intrinsic"),
            ],
            &[Relocation {
                offset: 2,
                symbol: 2,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            }],
        );

        assert_eq!(
            link(core::slice::from_ref(&object), "f0").unwrap_err(),
            LinkError::UndefinedSymbol(String::from("exotic_intrinsic")),
            "a plain link demands even a dead function's intrinsic"
        );

        let trimmed = garbage_collect(core::slice::from_ref(&object), "f0");
        let names: Vec<&str> = trimmed
            .iter()
            .flat_map(|o| &o.symbols)
            .filter(|s| !s.name.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(names.contains(&"f0"), "the entry survives");
        assert!(!names.contains(&"unreached"), "the unreached function is dropped");
        assert!(
            !names.contains(&"exotic_intrinsic"),
            "its intrinsic demand drops with it -- the image demands only the reached set"
        );
        assert!(
            link_gc(core::slice::from_ref(&object), "f0").is_ok(),
            "so the gc link resolves with no intrinsic supplied"
        );
    }

    #[test]
    fn archive_members_are_pulled_on_demand() {
        let main = obj_arm(
            &[0x00, 0xF0, 0x00, 0xD0, 0x70, 0x47],
            &[func("main", 1, 6), undef("answer")],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            }],
        );
        let answer = ArchiveMember {
            name: String::from("answer.o"),
            object: obj_arm(&[0x2A, 0x20, 0x70, 0x47], &[func("answer", 1, 4)], &[]),
        };
        let unused = ArchiveMember {
            name: String::from("unused.o"),
            object: obj_arm(&[0x00, 0x20, 0x70, 0x47], &[func("unused", 1, 4)], &[]),
        };
        let archive = Archive {
            members: Vec::from([answer, unused]),
        };
        let img = link_with_archives(&[main], &[archive], "main", None).unwrap();
        assert!(img.symbols.iter().any(|(n, _)| n == "answer"));
        assert!(
            !img.symbols.iter().any(|(n, _)| n == "unused"),
            "an archive member nothing references must not be pulled"
        );
    }

    #[test]
    fn a_local_section_symbol_resolves_within_its_object() {
        let section = Symbol {
            name: "",
            value: 4,
            size: 0,
            binding: Binding::Local,
            kind: SymbolType::NoType,
            section: SymbolSection::Text,
        };
        let obj = obj_arm(
            &[0, 0, 0, 0, 0x0D, 0xF0, 0xFE, 0xCA],
            &[func("f", 1, 4), section],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: arm::R_ARM_ABS32,
                addend: 0,
            }],
        );
        let img = link_with_archives(&[obj], &[], "f", Some(0x1000)).unwrap();
        assert_eq!(
            u32::from_le_bytes([img.text[0], img.text[1], img.text[2], img.text[3]]),
            0x1000 + 4,
        );
    }

    /// AND THE DEAD-STRIP MUST NOT MOVE THAT OBJECT'S BYTES. The section symbol's value plus the
    /// relocation's addend is an offset into the section as it was LAID OUT; a symbol-granularity
    /// re-layout invalidates it and there is no name to re-resolve through, so `trim_all` passes
    /// such an object through whole. Before that rule, `trim_object` turned the unnamed target into
    /// an undefined extern and 51 corpus programs failed the gc link on `UndefinedSymbol("")` --
    /// the empty string being what an unnamed target becomes when it is looked up by name.
    ///
    /// The `dead` function is the control in the other direction: the object is kept whole, so a
    /// body nothing calls survives HERE, where it would be stripped from any other object. That is
    /// the cost of the rule and it is asserted rather than left as a claim.
    #[test]
    fn an_object_that_resolves_against_its_own_layout_is_kept_whole() {
        let section = Symbol {
            name: "",
            value: 4,
            size: 0,
            binding: Binding::Local,
            kind: SymbolType::NoType,
            section: SymbolSection::Text,
        };
        let obj = obj_arm(
            &[
                0, 0, 0, 0,
                0x0D, 0xF0, 0xFE, 0xCA,
                0x70, 0x47,
            ],
            &[func("f", 1, 8), section, func("dead", 9, 2)],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: arm::R_ARM_ABS32,
                addend: 0,
            }],
        );
        let img = link_at_base_gc(core::slice::from_ref(&obj), "f", 0x1000)
            .expect("an object with a section-relative relocation must still link through the gc path");
        assert_eq!(
            u32::from_le_bytes(img.text[0..4].try_into().unwrap()),
            0x1000 + 4,
            "the section-relative word must still address the constant at blob offset 4"
        );
        assert!(
            img.symbols.iter().any(|(n, _)| n == "dead"),
            "the object is kept WHOLE, so even its unreached body survives -- the cost of the rule"
        );
    }

    #[test]
    fn a_transitively_needed_member_is_pulled() {
        let main = obj_arm(
            &[0x00, 0xF0, 0x00, 0xD0, 0x70, 0x47],
            &[func("main", 1, 6), undef("a")],
            &[Relocation {
                offset: 0,
                symbol: 1,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            }],
        );
        let a = ArchiveMember {
            name: String::from("a.o"),
            object: obj_arm(
                &[0x00, 0xF0, 0x00, 0xD0, 0x70, 0x47],
                &[func("a", 1, 6), undef("b")],
                &[Relocation {
                    offset: 0,
                    symbol: 1,
                    kind: arm::R_ARM_THM_CALL,
                    addend: -4,
                }],
            ),
        };
        let b = ArchiveMember {
            name: String::from("b.o"),
            object: obj_arm(&[0x2A, 0x20, 0x70, 0x47], &[func("b", 1, 4)], &[]),
        };
        let archive = Archive {
            members: Vec::from([a, b]),
        };
        let img = link_with_archives(&[main], &[archive], "main", None).unwrap();
        assert!(img.symbols.iter().any(|(n, _)| n == "a"));
        assert!(img.symbols.iter().any(|(n, _)| n == "b"));
    }

    /// `main`: push; bl f; bl g; pop; then `abs_targets` data words, each an R_ARM_ABS32 to a named
    /// function (taking its address). Returns the object.
    fn icf_main(abs_targets: &[u32]) -> Object {
        let mut text: Vec<u8> = Vec::from([
            0x00, 0xB5, 0x00, 0xF0, 0x00, 0xD0, 0x00, 0xF0, 0x00, 0xD0, 0x00, 0xBD,
        ]);
        let mut relocs = Vec::from([
            Relocation {
                offset: 2,
                symbol: 1,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            },
            Relocation {
                offset: 6,
                symbol: 2,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            },
        ]);
        for &sym in abs_targets {
            let offset = text.len() as u32;
            text.extend_from_slice(&[0, 0, 0, 0]);
            relocs.push(Relocation {
                offset,
                symbol: sym,
                kind: arm::R_ARM_ABS32,
                addend: 0,
            });
        }
        let size = text.len() as u32;
        obj_arm(
            &text,
            &[func("main", 1, size), undef("f"), undef("g")],
            &relocs,
        )
    }

    #[test]
    fn icf_folds_identical_functions() {
        let body = &[0x15, 0x20, 0x70, 0x47];
        let f = obj_arm(body, &[func("f", 1, 4)], &[]);
        let g = obj_arm(body, &[func("g", 1, 4)], &[]);
        let icf = link_icf(&[icf_main(&[]), f.clone(), g.clone()], "main", None).unwrap();
        let gc = link_gc(&[icf_main(&[]), f, g], "main").unwrap();
        let addr = |img: &LinkedImage, n: &str| img.symbols.iter().find(|(s, _)| s == n).unwrap().1;
        assert_eq!(addr(&icf, "f"), addr(&icf, "g"), "g folds into f");
        assert!(
            icf.text.len() < gc.text.len(),
            "ICF must drop the duplicate function copy"
        );
    }

    #[test]
    fn icf_keeps_address_taken_functions_distinct() {
        let body = &[0x15, 0x20, 0x70, 0x47];
        let f = obj_arm(body, &[func("f", 1, 4)], &[]);
        let g = obj_arm(body, &[func("g", 1, 4)], &[]);
        let main = icf_main(&[1, 2]);
        let icf = link_icf(&[main, f, g], "main", Some(0x8000)).unwrap();
        let addr = |n: &str| icf.symbols.iter().find(|(s, _)| s == n).unwrap().1;
        assert_ne!(
            addr("f"),
            addr("g"),
            "address-taken functions must keep distinct identities"
        );
    }

    /// A WEAKLY DUPLICATED FUNCTION MUST NOT FOLD INTO ITSELF. `compiler_builtins` re-defines its
    /// `__aeabi_*` helpers weak in several archive members, so one name can arrive twice with
    /// byte-identical bodies. Grouping those by fingerprint yields the fold pair `(dup, dup)`, and
    /// dropping `dup` from the keep set then deletes BOTH copies -- an "optimization" that erases
    /// the function. The control is the SAME objects under `link_gc`: it links, so this row is
    /// about folding rather than about the fixture.
    #[test]
    fn a_weakly_duplicated_identical_function_is_not_folded_out_of_existence() {
        let body = &[0x15, 0x20, 0x70, 0x47];
        let main = obj_arm(
            &[0x00, 0xB5, 0x00, 0xF0, 0x00, 0xD0, 0x00, 0xBD],
            &[func("main", 1, 8), undef("dup")],
            &[Relocation {
                offset: 2,
                symbol: 1,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            }],
        );
        let one = obj_arm(body, &[weak("dup", 1, 4)], &[]);
        let two = obj_arm(body, &[weak("dup", 1, 4)], &[]);
        let inputs = [main, one, two];
        assert!(
            link_gc(&inputs, "main").is_ok(),
            "the control must link -- otherwise the fixture is what is wrong"
        );
        let icf = link_icf(&inputs, "main", None);
        assert!(
            icf.as_ref().is_ok_and(|i| i.symbols.iter().any(|(n, _)| n == "dup")),
            "the fold path must keep a definition of `dup`; got {icf:?}"
        );
    }

    /// THE DEFECT THAT WAS, NOW STATED AS THE PROPERTY: the `--gc-sections`/ICF path used to be
    /// FUNCTION-ONLY, so it could not link any object referencing a defined DATA symbol -- and
    /// every AOT program does (its type descriptors, its string blobs, its stack-map records, its
    /// statics region). The predecessor of this test ASSERTED that refusal, with a note saying to
    /// replace it with the positive claim once the path learned about data. This is that
    /// replacement.
    ///
    /// The `link_at_base` control is kept for the reason it was written: it makes the claim about
    /// the PATH rather than about the fixture, in both directions.
    #[test]
    fn the_gc_and_icf_path_links_a_reference_to_a_data_symbol() {
        let text = [0x15, 0x20, 0x70, 0x47, 0, 0, 0, 0];
        let main = obj_arm(
            &text,
            &[func("main", 1, 8), data("desc", 8, 4)],
            &[Relocation {
                offset: 4,
                symbol: 1,
                kind: arm::R_ARM_ABS32,
                addend: 0,
            }],
        );
        let plain = link_at_base(core::slice::from_ref(&main), "main", 0x8000);
        assert!(
            plain.is_ok(),
            "the control must link -- otherwise the fixture is what is wrong, not the path"
        );
        for (label, linked) in [
            (
                "gc",
                link_gc_with_archives(core::slice::from_ref(&main), &[], "main", Some(0x8000)),
            ),
            (
                "icf",
                link_icf_with_archives(core::slice::from_ref(&main), &[], "main", Some(0x8000)),
            ),
        ] {
            let image = linked.unwrap_or_else(|e| panic!("the {label} path must link: {e:?}"));
            let desc = image
                .symbols
                .iter()
                .find(|(n, _)| n == "desc")
                .unwrap_or_else(|| panic!("the {label} image must define `desc`"))
                .1;
            let word = u32::from_le_bytes(image.text[4..8].try_into().unwrap());
            assert_eq!(
                word,
                0x8000 + desc,
                "the {label} image's ABS32 word must hold `desc`'s address"
            );
        }
    }

    /// A DEFINED SYMBOL WITH NO `st_size` IS DROPPED BY THE DEAD-STRIP, AND THE LINK THEN NAMES IT
    /// AS UNDEFINED. This is `trim_object`'s copy rule showing through: it copies
    /// `[st_value, st_value + st_size)`, so a size-less symbol contributes no bytes and cannot be
    /// kept over somebody else's code.
    ///
    /// It is pinned because the failure READS LIKE A MISSING DEFINITION and is not one -- fifteen
    /// `global_asm!` shims in the runtime-support archive were exactly this, and the fix is a
    /// `.size` directive in the assembly, not a change here. The `link_at_base` control links the
    /// same objects, so the row is about the dead-strip and not about the fixture.
    #[test]
    fn a_defined_symbol_with_no_size_does_not_survive_the_dead_strip() {
        let main = obj_arm(
            &[0x00, 0xB5, 0x00, 0xF0, 0x00, 0xD0, 0x00, 0xBD],
            &[func("main", 1, 8), undef("shim")],
            &[Relocation {
                offset: 2,
                symbol: 1,
                kind: arm::R_ARM_THM_CALL,
                addend: -4,
            }],
        );
        let unsized_shim = obj_arm(&[0x70, 0x47], &[func("shim", 1, 0)], &[]);
        let sized_shim = obj_arm(&[0x70, 0x47], &[func("shim", 1, 2)], &[]);
        assert!(
            link_at_base(&[main.clone(), unsized_shim.clone()], "main", 0x8000).is_ok(),
            "the control must link the size-less symbol -- the ordinary link never trims"
        );
        let refused = link_gc(&[main.clone(), unsized_shim], "main");
        assert!(
            matches!(&refused, Err(LinkError::UndefinedSymbol(n)) if n == "shim"),
            "a size-less definition must not survive the trim; got {refused:?}"
        );
        assert!(
            link_gc(&[main, sized_shim], "main").is_ok(),
            "and the SIZE is the whole difference -- the same shim with st_size links"
        );
    }

    /// A REACHED **LOCAL** BODY'S OWN CALLS ARE FOLLOWED. `trim_object` keeps a symbol by NAME with
    /// no binding test, so a local function whose name is reached survives; the walk therefore has
    /// to follow its relocations too, or the trim keeps a body whose callee it just dropped.
    ///
    /// Nothing had exercised this because the only objects ever dead-stripped were the backend's
    /// own, where every body is Global. An ARCHIVE member is full of internal-linkage Rust bodies,
    /// and `link_gc_with_archives` is what began trimming those.
    #[test]
    fn a_reached_local_bodys_own_calls_are_rooted() {
        let mut helper = func("helper", 9, 8);
        helper.binding = Binding::Local;
        let caller = obj_arm(
            &[
                0x00, 0xB5, 0x00, 0xF0, 0x00, 0xD0, 0x00, 0xBD,
                0x00, 0xB5, 0x00, 0xF0, 0x00, 0xD0, 0x00, 0xBD,
            ],
            &[func("main", 1, 8), helper, undef("leaf")],
            &[
                Relocation {
                    offset: 2,
                    symbol: 1,
                    kind: arm::R_ARM_THM_CALL,
                    addend: -4,
                },
                Relocation {
                    offset: 10,
                    symbol: 2,
                    kind: arm::R_ARM_THM_CALL,
                    addend: -4,
                },
            ],
        );
        let leaf = obj_arm(&[0x70, 0x47], &[func("leaf", 1, 2)], &[]);
        let image = link_gc(&[caller, leaf], "main").expect("the local body's callee must survive");
        assert!(
            image.symbols.iter().any(|(n, _)| n == "leaf"),
            "`leaf` is reached only through a LOCAL caller and must still be kept"
        );
    }

    /// The archive-pulling twins resolve an undefined symbol from a member exactly as
    /// [`link_with_archives`] does, so a fold measurement and a production measurement can be taken
    /// on the SAME input set.
    #[test]
    fn the_fold_path_pulls_archive_members_on_demand() {
        let body = &[0x15, 0x20, 0x70, 0x47];
        let archive = Archive {
            members: alloc::vec![
                ArchiveMember {
                    name: String::from("f.o"),
                    object: obj_arm(body, &[func("f", 1, 4)], &[]),
                },
                ArchiveMember {
                    name: String::from("g.o"),
                    object: obj_arm(body, &[func("g", 1, 4)], &[]),
                },
            ],
        };
        let main = icf_main(&[]);
        assert!(
            link_gc(core::slice::from_ref(&main), "main").is_err(),
            "without the archive there is no `f` -- the control for the two below"
        );
        let objects = core::slice::from_ref(&main);
        let archives = core::slice::from_ref(&archive);
        for pulled in [
            link_gc_with_archives(objects, archives, "main", None),
            link_icf_with_archives(objects, archives, "main", Some(0x8000)),
        ] {
            let image = pulled.expect("the archive member resolves `f`");
            assert!(image.symbols.iter().any(|(n, _)| n == "f"));
        }
    }


    /// Two objects that each carry `.debug_info` + `.debug_abbrev`, the second's contributions
    /// landing at NON-ZERO offsets -- the case that separates a real concatenation from a naive
    /// "copy the first one and hope" pass. `dead` is unreferenced by any code, so a `--gc-sections`
    /// link strips it while the plain link keeps it.
    fn debug_pair() -> [Object; 2] {
        [
            debug_obj(
                Machine::RiscV,
                &[0u8; 8],
                &[("_start", 0, 4), ("dead", 4, 4)],
                &[
                    (
                        ".debug_info",
                        &[0u8; 12],
                        &[
                            (0, DTarget::Section(".debug_abbrev")),
                            (4, DTarget::Code("_start")),
                            (8, DTarget::Code("dead")),
                        ],
                    ),
                    (".debug_abbrev", &[0xAAu8; 8], &[]),
                ],
            ),
            debug_obj(
                Machine::RiscV,
                &[0u8; 4],
                &[("helper", 0, 4)],
                &[
                    (
                        ".debug_info",
                        &[0u8; 4],
                        &[(0, DTarget::Section(".debug_abbrev"))],
                    ),
                    (".debug_abbrev", &[0xBBu8; 4], &[]),
                ],
            ),
        ]
    }

    #[test]
    fn carried_debug_sections_concatenate_by_name_across_objects() {
        let image = link_at_base(&debug_pair(), "_start", 0x2000).unwrap();
        let named = |n: &str| {
            image
                .debug_sections
                .iter()
                .find(|(s, _)| s == n)
                .map(|(_, d)| d.clone())
                .unwrap()
        };
        assert_eq!(named(".debug_info").len(), 12 + 4);
        let abbrev = named(".debug_abbrev");
        assert_eq!(abbrev.len(), 8 + 4);
        assert_eq!(&abbrev[..8], &[0xAA; 8], "first object's bytes lead");
        assert_eq!(&abbrev[8..], &[0xBB; 4], "second object's follow");
    }

    #[test]
    fn a_debug_reference_to_another_debug_section_is_section_relative() {
        let image = link_at_base(&debug_pair(), "_start", 0x2000).unwrap();
        assert_eq!(debug_word(&image, ".debug_info", 0), 0);
        assert_eq!(debug_word(&image, ".debug_info", 12), 8);
    }

    #[test]
    fn a_debug_reference_to_code_carries_the_load_address() {
        let image = link_at_base(&debug_pair(), "_start", 0x2000).unwrap();
        assert_eq!(debug_word(&image, ".debug_info", 4), 0x2000);
        assert_eq!(debug_word(&image, ".debug_info", 8), 0x2004);
    }

    #[test]
    fn a_debug_address_never_takes_the_thumb_bit() {
        let obj = debug_obj(
            Machine::Arm,
            &[0u8; 4],
            &[("_start", 1, 4)],
            &[(".debug_info", &[0u8; 4], &[(0, DTarget::Code("_start"))])],
        );
        let image = link_at_base(&[obj], "_start", 0x2000).unwrap();
        assert_eq!(debug_word(&image, ".debug_info", 0), 0x2000);
    }

    #[test]
    fn an_object_without_debug_info_produces_no_debug_sections() {
        let objects = [obj(&[0x13, 0x05, 0xa0, 0x02], &[func("_start", 0, 4)], &[])];
        let image = link_at_base(&objects, "_start", 0x2000).unwrap();
        assert!(image.debug_sections.is_empty());
    }

    #[test]
    fn gc_sections_does_not_keep_a_function_alive_through_a_debug_reference() {
        let kept = garbage_collect(&debug_pair(), "_start");
        let names: Vec<&str> = kept
            .iter()
            .flat_map(|o| &o.symbols)
            .filter(|s| s.defined && !s.name.is_empty())
            .map(|s| s.name.as_str())
            .collect();
        assert!(names.contains(&"_start"), "the entry survives");
        assert!(
            !names.contains(&"dead"),
            "a debug reference must not resurrect dead code"
        );
    }

    #[test]
    fn gc_sections_tombstones_a_debug_reference_to_stripped_code() {
        let image = link_at_base_gc(&debug_pair(), "_start", 0x2000).unwrap();
        assert_eq!(debug_word(&image, ".debug_info", 4), 0x2000);
        assert_eq!(debug_word(&image, ".debug_info", 8), 0);
    }

    #[test]
    fn gc_sections_remaps_a_debug_reference_to_code_that_moved() {
        let mut objects = [debug_obj(
            Machine::RiscV,
            &[0u8; 12],
            &[("_start", 0, 4), ("dead", 4, 4), ("keep", 8, 4)],
            &[(
                ".debug_info",
                &[0u8; 8],
                &[(0, DTarget::Code("_start")), (4, DTarget::Code("keep"))],
            )],
        )];
        let keep = objects[0]
            .symbols
            .iter()
            .position(|s| s.name == "keep")
            .expect("the fixture defines `keep`") as u32;
        objects[0].relocations.push(ParsedRelocation {
            offset: 0,
            symbol: keep,
            kind: riscv::R_RISCV_32,
            addend: 0,
            implicit_addend: false,
        });
        let image = link_at_base_gc(&objects, "_start", 0x2000).unwrap();
        assert_eq!(debug_word(&image, ".debug_info", 0), 0x2000, "entry first");
        assert_eq!(debug_word(&image, ".debug_info", 4), 0x2004);
    }
}
