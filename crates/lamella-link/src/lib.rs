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
}

/// On ARM, a Thumb function symbol carries the Thumb state in its value's low bit (`answer` =
/// `offset | 1`). The linker normalizes to the even byte offset for layout + reach math (BL keeps a
/// halfword-even target); the Thumb bit is re-applied only to a Thumb executable's `e_entry`. On
/// other machines, and for non-ARM, the value passes through. (Mixed ARM/Thumb interworking, which
/// would need the bit to choose BL vs BLX, is out of scope -- our backend + `-mthumb` C are Thumb.)
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
/// to those names land on their real addresses. This is the RAM-injection delivery (REPL-against-AOT,
/// delivery-foundations.md scenario 5): the host links a snippet to the RAM buffer it will write, with
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
    let trimmed = garbage_collect(objects, entry);
    link_with_base(&trimmed, entry, Some(text_base), &[])
}

/// Re-exported from [`lamella_elf`] so the backend that NAMES descriptor symbols and the linker that
/// COLLECTS them here share ONE prefix. A descriptor unreachable from the entry is dropped (so its vtable
/// relocations cannot pin an otherwise-trimmed method); other data is retained wholesale.
pub use lamella_elf::TYPE_DESC_PREFIX;

/// Function-level `--gc-sections`: builds the cross-object reference graph from `entry` -- following each
/// reached symbol's relocations, a function's calls AND a data symbol's references (e.g. a type
/// descriptor's vtable entries and base pointer) -- keeps the reachable functions, reachable descriptors,
/// and all other data, then rebuilds each object re-laid-out with its symbols and relocations remapped, so
/// unused functions/descriptors and the undefined externs only they referenced drop out.
pub fn garbage_collect(objects: &[Object], entry: &str) -> Vec<Object> {
    let mut defs: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    for (oi, obj) in objects.iter().enumerate() {
        for (si, s) in obj.symbols.iter().enumerate() {
            if s.defined && s.binding != Binding::Local && !s.name.is_empty() {
                defs.entry(s.name.as_str()).or_insert((oi, si));
            }
        }
    }
    let mut reachable: BTreeSet<String> = BTreeSet::new();
    let mut stack: Vec<String> = Vec::new();
    stack.push(String::from(entry));
    while let Some(name) = stack.pop() {
        if !reachable.insert(name.clone()) {
            continue;
        }
        let Some(&(oi, si)) = defs.get(name.as_str()) else {
            continue;
        };
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
    let mut out = Vec::with_capacity(objects.len());
    for obj in objects {
        out.push(trim_object(obj, &reachable));
    }
    out
}

/// Rebuilds `obj` keeping only reachable functions and reachable descriptors (plus all other data),
/// re-laid-out. A referenced symbol not among the kept ones stays an undefined extern (the linker resolves
/// it, or errors if genuinely missing).
fn trim_object(obj: &Object, reachable: &BTreeSet<String>) -> Object {
    let mut kept: Vec<usize> = (0..obj.symbols.len())
        .filter(|&i| {
            let s = &obj.symbols[i];
            s.defined
                && s.size > 0
                && !s.name.is_empty()
                && (reachable.contains(&s.name)
                    || (s.kind != SymbolType::Func && !s.name.starts_with(TYPE_DESC_PREFIX)))
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
    });
    let mut index_of: BTreeMap<String, u32> = BTreeMap::new();
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
        });
    }

    let mut relocations: Vec<ParsedRelocation> = Vec::new();
    for r in &obj.relocations {
        let Some(&(old_start, _, new_start)) =
            ranges.iter().find(|(a, b, _)| r.offset >= *a && r.offset < *b)
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
                    size: 0,
                    binding: Binding::Global,
                    kind: target.kind,
                    defined: false,
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

    Object {
        machine: obj.machine,
        text,
        text_align: obj.text_align.max(4),
        symbols,
        relocations,
    }
}

fn link_with_base(
    objects: &[Object],
    entry: &str,
    text_base: Option<u32>,
    residents: &[(&str, u32)],
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
            let addr = bases[oi] + normalized_value(machine, sym.value);
            let thumb = is_thumb_func(machine, sym.value);
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

    let mut veneers: BTreeMap<u32, u32> = BTreeMap::new();
    if machine == Machine::Arm {
        for (oi, obj) in objects.iter().enumerate() {
            for r in &obj.relocations {
                if r.kind != arm::R_ARM_THM_CALL {
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
                &defined,
                &obj.symbols,
                r,
                &veneers,
            )?;
        }
    }

    let entry_offset =
        resolve(&defined, entry).ok_or_else(|| LinkError::MissingEntry(String::from(entry)))?;
    Ok(LinkedImage {
        text,
        entry_offset,
        symbols: defined.into_iter().map(|(n, a, _)| (n, a)).collect(),
    })
}

/// On ARM, a Thumb function carries the Thumb bit in its symbol value's low bit (our backend +
/// `gcc -mthumb` both set it for `STT_FUNC`); that bit is the `T` an `R_ARM_ABS32` reapplies. Data
/// symbols, and any symbol on a non-ARM target, are not Thumb.
fn is_thumb_func(machine: Machine, value: u32) -> bool {
    machine == Machine::Arm && value & 1 == 1
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
    defined: &[Defined],
    obj_syms: &[lamella_elf::ParsedSymbol],
    r: &ParsedRelocation,
    veneers: &BTreeMap<u32, u32>,
) -> Result<(), LinkError> {
    if machine == Machine::RiscV && r.kind == riscv::R_RISCV_RELAX {
        return Ok(());
    }
    let name = &obj_syms[r.symbol as usize].name;
    let (target, target_is_thumb) =
        resolve_sym(defined, name).ok_or_else(|| LinkError::UndefinedSymbol(name.clone()))?;
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
            other => Err(LinkError::UnsupportedRelocation(other)),
        },
    }
}

/// The relocation's addend `A`. RISC-V (and our own ARM objects) use explicit `RELA` addends; a
/// `SHT_REL` ARM object (the `-mthumb` C toolchain's convention) stores the addend implicitly in the
/// instruction field, so the linker extracts it from the call's current encoding.
fn relocation_addend(text: &[u8], machine: Machine, site: u32, r: &ParsedRelocation) -> i64 {
    if r.implicit_addend {
        match (machine, r.kind) {
            (Machine::Arm, arm::R_ARM_THM_CALL) => extract_thm_call(text, site),
            (Machine::Arm, arm::R_ARM_CALL) => extract_arm_call(text, site),
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

/// Like [`link`], but with `--gc-sections` dead-stripping at the FUNCTION level: only functions
/// reachable from `entry` (over the call graph the `.rela.text` relocations expose) are kept; unused
/// functions are dropped even from an otherwise-used object. The kept functions are re-laid-out
/// (the entry first) and their relocations re-applied to the new offsets.
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

fn link_gc_inner(
    objects: &[Object],
    entry: &str,
    fold: bool,
    text_base: Option<u32>,
) -> Result<LinkedImage, LinkError> {
    let machine = link_machine(objects)?;
    let mut funcs: Vec<(usize, String, u32, u32)> = Vec::new();
    for (oi, obj) in objects.iter().enumerate() {
        let mut bounds: Vec<(u32, String)> = obj
            .symbols
            .iter()
            .filter(|s| {
                s.defined
                    && s.binding == Binding::Global
                    && s.kind == SymbolType::Func
                    && !s.name.is_empty()
            })
            .map(|s| (normalized_value(machine, s.value), s.name.clone()))
            .collect();
        bounds.sort_by_key(|(v, _)| *v);
        for i in 0..bounds.len() {
            let start = bounds[i].0;
            let end = bounds
                .get(i + 1)
                .map(|(v, _)| *v)
                .unwrap_or(obj.text.len() as u32);
            funcs.push((oi, bounds[i].1.clone(), start, end));
        }
    }
    let index_of = |name: &str| funcs.iter().position(|(_, n, _, _)| n == name);
    let entry_fi = index_of(entry).ok_or_else(|| LinkError::MissingEntry(String::from(entry)))?;

    let mut reachable = alloc::vec![false; funcs.len()];
    let mut stack = alloc::vec![entry_fi];
    reachable[entry_fi] = true;
    while let Some(fi) = stack.pop() {
        let (oi, _, start, end) = funcs[fi].clone();
        for r in &objects[oi].relocations {
            if r.offset < start || r.offset >= end {
                continue;
            }
            if let Some(s) = objects[oi].symbols.get(r.symbol as usize) {
                if let Some(tfi) = index_of(&s.name) {
                    if !reachable[tfi] {
                        reachable[tfi] = true;
                        stack.push(tfi);
                    }
                }
            }
        }
    }

    let fold_to = if fold {
        compute_folds(&funcs, objects, machine, &reachable, entry_fi)
    } else {
        alloc::vec![None; funcs.len()]
    };

    let mut text: Vec<u8> = Vec::new();
    let mut new_offset: Vec<Option<u32>> = alloc::vec![None; funcs.len()];
    let mut defined: Vec<Defined> = Vec::new();
    let order = core::iter::once(entry_fi).chain(
        (0..funcs.len()).filter(|&fi| reachable[fi] && fi != entry_fi && fold_to[fi].is_none()),
    );
    for fi in order {
        let (oi, name, start, end) = &funcs[fi];
        align_to(&mut text, objects[*oi].text_align);
        let off = text.len() as u32;
        new_offset[fi] = Some(off);
        defined.push((name.clone(), off, machine == Machine::Arm));
        text.extend_from_slice(&objects[*oi].text[*start as usize..*end as usize]);
    }
    for fi in 0..funcs.len() {
        if let Some(rep) = fold_to[fi] {
            let rep_off = new_offset[rep].expect("a representative is laid out");
            defined.push((funcs[fi].1.clone(), rep_off, machine == Machine::Arm));
        }
    }

    let no_veneers: BTreeMap<u32, u32> = BTreeMap::new();
    for fi in 0..funcs.len() {
        let Some(fbase) = new_offset[fi] else {
            continue;
        };
        let (oi, _, start, end) = &funcs[fi];
        for r in &objects[*oi].relocations {
            if r.offset < *start || r.offset >= *end {
                continue;
            }
            let site = fbase + (r.offset - start);
            apply_relocation(
                &mut text,
                machine,
                site,
                text_base,
                &defined,
                &objects[*oi].symbols,
                r,
                &no_veneers,
            )?;
        }
    }

    let entry_offset = new_offset[entry_fi].expect("entry laid out");
    Ok(LinkedImage {
        text,
        entry_offset,
        symbols: defined.into_iter().map(|(n, a, _)| (n, a)).collect(),
    })
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
/// into representative `rep`. Reachable functions are grouped by fingerprint; in each group the
/// non-address-taken duplicates fold into one survivor (an address-taken member preferred as the
/// survivor so its identity is what remains). Address-taken functions and the entry never fold away.
fn compute_folds(
    funcs: &[Func],
    objects: &[Object],
    machine: Machine,
    reachable: &[bool],
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
    let fps: Vec<Option<Fingerprint>> = (0..funcs.len())
        .map(|fi| reachable[fi].then(|| function_fingerprint(&funcs[fi], objects)))
        .collect();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for fi in 0..funcs.len() {
        let Some(fp) = &fps[fi] else { continue };
        match groups.iter_mut().find(|g| fps[g[0]].as_ref() == Some(fp)) {
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
    let hw1 = 0xF000 | (s << 10) | imm10;
    let hw2 = 0xD000 | (j1 << 13) | (j2 << 11) | imm11;
    let site = site as usize;
    let slot = text
        .get_mut(site..site + 4)
        .ok_or(LinkError::RelocationOutOfRange(site as u32))?;
    slot[0..2].copy_from_slice(&hw1.to_le_bytes());
    slot[2..4].copy_from_slice(&hw2.to_le_bytes());
    Ok(())
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
    use lamella_elf::{
        ArchiveMember, Binding, Machine, Relocation, Symbol, SymbolSection, SymbolType, arm,
        read_object, write_relocatable_object,
    };

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
}
