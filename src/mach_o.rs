/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Reading of Mach-O files, the executable and library format on iPhone OS.
//!
//! Implemented using the mach_object crate. All usage of that crate should be
//! confined to this module. The goal is to read the Mach-O binary exactly once,
//! storing any information we'll need later.
//!
//! Useful resources:
//! - Apple's [Overview of the Mach-O Executable Format](https://developer.apple.com/library/archive/documentation/Performance/Conceptual/CodeFootprint/Articles/MachOOverview.html) explains what "segments" and "sections" are, and provides short descriptions of the purposes of some common sections.
//! - Apple's old "OS X ABI Mach-O File Format Reference", which is mirrored in [various](https://github.com/aidansteele/osx-abi-macho-file-format-reference) [places](https://www.symbolcrash.com/wp-content/uploads/2019/02/ABI_MachOFormat.pdf) online.
//! - Alex Drummond's [Inside a Hello World executable on OS X](https://adrummond.net/posts/macho) is about macOS circa 2017 rather than iPhone OS circa 2008, so not all of what it says applies, but the sections up to and including "9. The indirect symbol table" are helpful.
//! - The LLVM functions [`RuntimeDyldMachO::populateIndirectSymbolPointersSection`](https://github.com/llvm/llvm-project/blob/2e999b7dd1934a44d38c3a753460f1e5a217e9a5/llvm/lib/ExecutionEngine/RuntimeDyld/RuntimeDyldMachO.cpp#L179-L220) and [`MachOObjectFile::getIndirectSymbolTableEntry`](https://github.com/llvm/llvm-project/blob/3c09ed006ab35dd8faac03311b14f0857b01949c/llvm/lib/Object/MachOObjectFile.cpp#L4803-L4808) are references for how to read the indirect symbol table.
//! - `/usr/include/mach-o/reloc.h` in the macOS SDK was the reference for the
//!   format of relocation entries.
//! - The [source code of the mach_object crate](https://docs.rs/mach_object/latest/src/mach_object/commands.rs.html) has useful comments that don't show up in the generated documentation, e.g. around `DySymTab`.

use crate::abi::GuestFunction;
use crate::fs::{Fs, GuestPath};
use crate::mem::{ConstPtr, GuestUSize, Mem, MutPtr, Ptr};
use mach_object::{
    cpu_subtype_t, vm_prot_t, DyLib, LoadCommand, MachCommand, OFile, ThreadState,
    N_ARM_THUMB_DEF, S_LAZY_SYMBOL_POINTERS, S_MOD_INIT_FUNC_POINTERS,
    S_NON_LAZY_SYMBOL_POINTERS, S_SYMBOL_STUBS,
};
use std::collections::HashMap;
use std::io::{Cursor, Seek, SeekFrom};

const VM_PROT_READ: vm_prot_t = 1;
const VM_PROT_WRITE: vm_prot_t = 2;
#[allow(dead_code)]
const VM_PROT_EXECUTE: vm_prot_t = 4;

/// `CPU_TYPE_ARM64` (`CPU_TYPE_ARM | CPU_ARCH_ABI64` = `0x0100_000c`).
const CPU_TYPE_ARM64: i32 = mach_object::CPU_TYPE_ARM | 0x0100_0000;
/// Default `__TEXT` base of 64-bit iOS executables (`0x1_0000_0000`).
const ARM64_DEFAULT_TEXT_BASE: u64 = 0x1_0000_0000;
/// Rebased `__TEXT` base inside `< 4 GiB` `Mem` (`0x0010_0000`).
const ARM64_REBASED_TEXT_BASE: u32 = 0x0010_0000;

#[inline]
fn rebase_arm64_addr(addr: u64) -> u32 {
    if addr >= ARM64_DEFAULT_TEXT_BASE {
        ((addr - ARM64_DEFAULT_TEXT_BASE) as u32).wrapping_add(ARM64_REBASED_TEXT_BASE)
    } else {
        addr as u32
    }
}

#[derive(Debug)]
pub struct MachO {
    /// Name (for debugging purposes and sorting)
    pub name: String,
    /// Paths of dynamic libraries referenced by the binary.
    pub dynamic_libraries: Vec<String>,
    /// Metadata related to sections.
    pub sections: Vec<Section>,
    /// All defined symbols in the binary, including local symbols. Use
    /// `external_symbols` for dependent-dylib resolution. Thumb function symbols
    /// always have the Thumb bit set.
    pub exported_symbols: HashMap<String, u32>,
    /// Defined symbols marked external in the Mach-O symbol table, used for
    /// dependent-dylib resolution.
    pub external_symbols: HashMap<String, u32>,
    /// List of addresses and names of external relocations for the dynamic
    /// linker to resolve.
    pub external_relocations: Vec<(u32, String)>,
    /// Address/program counter value for the entry point.
    pub entry_point_pc: Option<u32>,
    /// Whether the entry point came from an `LC_MAIN` load command rather
    /// than `LC_UNIXTHREAD`/`LC_THREAD`. With `LC_MAIN`, the entry point is
    /// the C `main` function itself, which dyld calls with
    /// `(argc, argv, envp, apple)` passed in registers per the calling
    /// convention, rather than a `start` routine that reads them from the
    /// stack.
    pub entry_point_is_lc_main: bool,
    /// End address of the highest-addressed segment.
    /// This is used by get_end() to return the first address after the last
    /// segment in the executable.
    pub last_segment_end: u32,
    /// Base address of the __TEXT segment (where the mach_header lives in
    /// memory). Used by `_dyld_get_image_header`.
    pub text_base: u32,
    /// True if this image is a 64-bit `CPU_TYPE_ARM64` binary.
    pub is_arm64: bool,
}

#[derive(Debug)]
pub struct Section {
    /// Section name.
    pub name: String,
    /// Section address in memory.
    pub addr: u32,
    /// Section size in bytes.
    pub size: u32,
    /// What type of section is this?
    pub type_: SectionType,
    /// Information specific to special dynamic linker sections, if this is one.
    pub dyld_indirect_symbol_info: Option<DyldIndirectSymbolInfo>,
}

impl Section {
    /// Returns address of the next section.
    pub fn next_section_addr(&self) -> u32 {
        self.addr + self.size
    }
}

/// Various kinds of sections. We're only interested in the ones used by the
/// dynamic linker for now (see also [DyldIndirectSymbolInfo]).
#[derive(Debug, PartialEq, Eq)]
pub enum SectionType {
    /// Normal section, or some section type we don't care about.
    Normal,
    /// Symbol stub section, usually called `__symbol_stub4` or
    /// `__picsymbolstub4`.
    SymbolStubs,
    /// Lazy symbol pointer section, usually called `__la_symbol_ptr`.
    LazySymbolPointers,
    /// Non-lazy symbol pointer section, usually called `__nl_symbol_ptr`.
    NonLazySymbolPointers,
    /// Initialization function pointer section, usually called
    /// `__mod_init_func`.
    ModInitFuncPointers,
}

/// Information relevant to certain special sections which contain a series of
/// pointers or stub functions for indirectly referencing dynamically-linked
/// symbols (see also [SectionType]).
#[derive(Debug)]
pub struct DyldIndirectSymbolInfo {
    /// The size in bytes of an entry (pointer or stub function) in the section.
    pub entry_size: u32,
    /// A list of symbol names corresponding to the entries.
    pub indirect_undef_symbols: Vec<Option<String>>,
}

/// Helper trait that makes [MachO::get_section] work. Yes, this is overkill. :)
pub trait SectionPredicate {
    fn test(&self, section: &Section) -> bool;
}
impl SectionPredicate for &str {
    fn test(&self, section: &Section) -> bool {
        section.name == *self
    }
}
impl SectionPredicate for SectionType {
    fn test(&self, section: &Section) -> bool {
        section.type_ == *self
    }
}

#[derive(Debug, Clone)]
enum ParsedSym<'a> {
    Debug,
    Undefined {
        name: Option<&'a str>,
    },
    Prebound {
        name: Option<&'a str>,
    },
    Defined {
        name: Option<&'a str>,
        external: bool,
        entry: u64,
        desc: u16,
    },
}

fn read_nlist_sym<'a>(
    bytes: &'a [u8],
    idx: u32,
    (symoff, nsyms, stroff, strsize): (u32, u32, u32, u32),
    is_64bit: bool,
) -> Option<ParsedSym<'a>> {
    // Mask out INDIRECT_SYMBOL_LOCAL (0x80000000) and INDIRECT_SYMBOL_ABS (0x40000000)
    if (idx & 0xc000_0000) != 0 || idx >= nsyms {
        return None;
    }
    let nlist_size: usize = if is_64bit { 16 } else { 12 };
    let entry_off = (symoff as usize).checked_add((idx as usize).checked_mul(nlist_size)?)?;
    let entry_end = entry_off.checked_add(nlist_size)?;
    if entry_end > bytes.len() {
        return None;
    }
    let raw = &bytes[entry_off..entry_end];
    let n_strx = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let n_type = raw[4];
    let _n_sect = raw[5];
    let n_desc = u16::from_le_bytes([raw[6], raw[7]]);
    let n_value: u64 = if is_64bit {
        u64::from_le_bytes([
            raw[8], raw[9], raw[10], raw[11], raw[12], raw[13], raw[14], raw[15],
        ])
    } else {
        u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]) as u64
    };

    // N_STAB = 0xe0
    if (n_type & 0xe0) != 0 {
        return Some(ParsedSym::Debug);
    }

    let name: Option<&'a str> = if n_strx == 0 || n_strx >= strsize {
        None
    } else {
        let str_start = (stroff as usize).checked_add(n_strx as usize)?;
        let str_limit = (stroff as usize).checked_add(strsize as usize)?.min(bytes.len());
        if str_start >= str_limit {
            None
        } else {
            let tail = &bytes[str_start..str_limit];
            let nul_pos = tail.iter().position(|&b| b == 0).unwrap_or(tail.len());
            std::str::from_utf8(&tail[..nul_pos]).ok()
        }
    };

    let external = (n_type & 0x01) != 0;
    match n_type & 0x0e {
        // N_SECT (0x0e) or N_ABS (0x02)
        0x0e | 0x02 => Some(ParsedSym::Defined {
            name,
            external,
            entry: n_value,
            desc: n_desc,
        }),
        // N_PBUD (0x0c)
        0x0c => Some(ParsedSym::Prebound { name }),
        // N_UNDF (0x00) or N_INDR (0x0a)
        _ => Some(ParsedSym::Undefined { name }),
    }
}

fn read_uleb128(slice: &[u8], pos: &mut usize) -> u64 {
    let mut result: u64 = 0;
    let mut shift: u32 = 0;
    while *pos < slice.len() {
        let byte = slice[*pos];
        *pos += 1;
        if shift < 64 {
            result |= ((byte & 0x7f) as u64) << shift;
        }
        if (byte & 0x80) == 0 {
            break;
        }
        shift += 7;
    }
    result
}

fn read_sleb128(slice: &[u8], pos: &mut usize) -> i64 {
    let mut result: i64 = 0;
    let mut shift: u32 = 0;
    let mut byte: u8 = 0;
    while *pos < slice.len() {
        byte = slice[*pos];
        *pos += 1;
        if shift < 64 {
            result |= ((byte & 0x7f) as i64) << shift;
        }
        shift += 7;
        if (byte & 0x80) == 0 {
            break;
        }
    }
    if shift < 64 && (byte & 0x40) != 0 {
        result |= (!0i64) << shift;
    }
    result
}

fn read_cstring<'a>(slice: &'a [u8], pos: &mut usize) -> &'a str {
    let start = *pos;
    while *pos < slice.len() && slice[*pos] != 0 {
        *pos += 1;
    }
    let end = *pos;
    if *pos < slice.len() {
        *pos += 1; // skip NUL
    }
    std::str::from_utf8(&slice[start..end]).unwrap_or("")
}

fn apply_single_rebase(
    seg_idx: usize,
    seg_off: u64,
    is_64bit: bool,
    segment_offsets: &[u32],
    slide: u32,
    into_mem: &mut Mem,
) {
    let Some(&seg_base) = segment_offsets.get(seg_idx) else {
        return;
    };
    let addr = seg_base.wrapping_add(seg_off as u32).wrapping_add(slide);
    if is_64bit {
        let ptr64: MutPtr<u64> = Ptr::from_bits(addr);
        let old64: u64 = into_mem.read(ptr64);
        let low48 = old64 & 0x0000_ffff_ffff_ffff;
        let new64 = if low48 >= ARM64_DEFAULT_TEXT_BASE
            && low48 < ARM64_DEFAULT_TEXT_BASE + 0x4000_0000
        {
            (rebase_arm64_addr(low48).wrapping_add(slide)) as u64
        } else if low48 < ARM64_REBASED_TEXT_BASE as u64 && low48 != 0 {
            (ARM64_REBASED_TEXT_BASE
                .wrapping_add(low48 as u32)
                .wrapping_add(slide)) as u64
        } else {
            old64
        };
        into_mem.write(ptr64, new64);
    } else {
        let original_location: MutPtr<u32> = Ptr::from_bits(addr);
        let old: u32 = into_mem.read(original_location);
        into_mem.write(original_location, old.wrapping_add(slide));
    }
}

fn apply_dyld_rebase(
    slice: &[u8],
    ptr_size: u64,
    is_64bit: bool,
    segment_offsets: &[u32],
    slide: u32,
    into_mem: &mut Mem,
) {
    let mut pos: usize = 0;
    let mut seg_idx: usize = 0;
    let mut seg_off: u64 = 0;
    let mut rebase_type: u8 = 1; // REBASE_TYPE_POINTER

    while pos < slice.len() {
        let byte = slice[pos];
        pos += 1;
        let opcode = byte & 0xf0;
        let imm = byte & 0x0f;
        match opcode {
            0x00 => break, // REBASE_OPCODE_DONE
            0x10 => {
                rebase_type = imm;
            }
            0x20 => {
                seg_idx = imm as usize;
                seg_off = read_uleb128(slice, &mut pos);
            }
            0x30 => {
                seg_off = seg_off.wrapping_add(read_uleb128(slice, &mut pos));
            }
            0x40 => {
                seg_off = seg_off.wrapping_add((imm as u64).wrapping_mul(ptr_size));
            }
            0x50 => {
                for _ in 0..imm {
                    if rebase_type == 1 {
                        apply_single_rebase(seg_idx, seg_off, is_64bit, segment_offsets, slide, into_mem);
                    }
                    seg_off = seg_off.wrapping_add(ptr_size);
                }
            }
            0x60 => {
                let count = read_uleb128(slice, &mut pos);
                for _ in 0..count {
                    if rebase_type == 1 {
                        apply_single_rebase(seg_idx, seg_off, is_64bit, segment_offsets, slide, into_mem);
                    }
                    seg_off = seg_off.wrapping_add(ptr_size);
                }
            }
            0x70 => {
                let add = read_uleb128(slice, &mut pos);
                if rebase_type == 1 {
                    apply_single_rebase(seg_idx, seg_off, is_64bit, segment_offsets, slide, into_mem);
                }
                seg_off = seg_off.wrapping_add(add).wrapping_add(ptr_size);
            }
            0x80 => {
                let count = read_uleb128(slice, &mut pos);
                let skip = read_uleb128(slice, &mut pos);
                for _ in 0..count {
                    if rebase_type == 1 {
                        apply_single_rebase(seg_idx, seg_off, is_64bit, segment_offsets, slide, into_mem);
                    }
                    seg_off = seg_off.wrapping_add(skip).wrapping_add(ptr_size);
                }
            }
            _ => break,
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_dyld_bind(
    slice: &[u8],
    ptr_size: u64,
    is_64bit: bool,
    is_weak: bool,
    is_lazy: bool,
    segment_offsets: &[u32],
    slide: u32,
    into_mem: &mut Mem,
    external_relocations: &mut Vec<(u32, String)>,
) {
    let mut pos: usize = 0;
    let mut seg_idx: usize = 0;
    let mut seg_off: u64 = 0;
    let mut bind_type: u8 = 1; // BIND_TYPE_POINTER
    let mut sym_name = String::new();
    let mut addend: i64 = 0;
    let mut in_threaded_table = false;
    let mut threaded_symbols: Vec<(String, i64)> = Vec::new();

    let mut do_one_bind = |seg_idx: usize,
                           seg_off: u64,
                           bind_type: u8,
                           sym_name: &str,
                           addend: i64,
                           into_mem: &mut Mem,
                           external_relocations: &mut Vec<(u32, String)>| {
        if bind_type != 1 || sym_name.is_empty() {
            return;
        }
        let Some(&seg_base) = segment_offsets.get(seg_idx) else {
            return;
        };
        let addr = seg_base.wrapping_add(seg_off as u32).wrapping_add(slide);
        if is_64bit {
            let ptr64: MutPtr<u64> = Ptr::from_bits(addr);
            if is_weak && into_mem.read(ptr64) != 0 {
                return;
            }
            let addend_u64 = (addend as i32 as u32) as u64;
            into_mem.write(ptr64, addend_u64);
        } else {
            let ptr32: MutPtr<u32> = Ptr::from_bits(addr);
            if is_weak && into_mem.read(ptr32) != 0 {
                return;
            }
            if addend != 0 && into_mem.read(ptr32) == 0 {
                into_mem.write(ptr32, addend as i32 as u32);
            }
        }
        external_relocations.push((addr, sym_name.to_string()));
    };

    while pos < slice.len() {
        let byte = slice[pos];
        pos += 1;
        let opcode = byte & 0xf0;
        let imm = byte & 0x0f;
        match opcode {
            0x00 => {
                // BIND_OPCODE_DONE: in lazy_bind each entry is terminated by DONE,
                // while in regular/weak bind DONE terminates the stream unless we
                // were populating the threaded ordinal table.
                if in_threaded_table {
                    in_threaded_table = false;
                } else if !is_lazy {
                    break;
                }
            }
            0x10 | 0x30 => {
                // BIND_OPCODE_SET_DYLIB_ORDINAL_IMM / SET_DYLIB_SPECIAL_IMM
            }
            0x20 => {
                // BIND_OPCODE_SET_DYLIB_ORDINAL_ULEB
                let _ = read_uleb128(slice, &mut pos);
            }
            0x40 => {
                // BIND_OPCODE_SET_SYMBOL_TRAILING_FLAGS_IMM
                sym_name = read_cstring(slice, &mut pos).to_string();
            }
            0x50 => {
                // BIND_OPCODE_SET_TYPE_IMM
                bind_type = imm;
            }
            0x60 => {
                // BIND_OPCODE_SET_ADDEND_SLEB
                addend = read_sleb128(slice, &mut pos);
            }
            0x70 => {
                // BIND_OPCODE_SET_SEGMENT_AND_OFFSET_ULEB
                seg_idx = imm as usize;
                seg_off = read_uleb128(slice, &mut pos);
            }
            0x80 => {
                // BIND_OPCODE_ADD_ADDR_ULEB
                seg_off = seg_off.wrapping_add(read_uleb128(slice, &mut pos));
            }
            0x90 => {
                // BIND_OPCODE_DO_BIND
                if in_threaded_table {
                    threaded_symbols.push((sym_name.clone(), addend));
                } else {
                    do_one_bind(
                        seg_idx,
                        seg_off,
                        bind_type,
                        &sym_name,
                        addend,
                        into_mem,
                        external_relocations,
                    );
                    seg_off = seg_off.wrapping_add(ptr_size);
                }
            }
            0xA0 => {
                // BIND_OPCODE_DO_BIND_ADD_ADDR_ULEB
                let add = read_uleb128(slice, &mut pos);
                do_one_bind(
                    seg_idx,
                    seg_off,
                    bind_type,
                    &sym_name,
                    addend,
                    into_mem,
                    external_relocations,
                );
                seg_off = seg_off.wrapping_add(add).wrapping_add(ptr_size);
            }
            0xB0 => {
                // BIND_OPCODE_DO_BIND_ADD_ADDR_IMM_SCALED
                do_one_bind(
                    seg_idx,
                    seg_off,
                    bind_type,
                    &sym_name,
                    addend,
                    into_mem,
                    external_relocations,
                );
                seg_off = seg_off.wrapping_add(((imm as u64) + 1).wrapping_mul(ptr_size));
            }
            0xC0 => {
                // BIND_OPCODE_DO_BIND_ULEB_TIMES_SKIPPING_ULEB
                let count = read_uleb128(slice, &mut pos);
                let skip = read_uleb128(slice, &mut pos);
                for _ in 0..count {
                    do_one_bind(
                        seg_idx,
                        seg_off,
                        bind_type,
                        &sym_name,
                        addend,
                        into_mem,
                        external_relocations,
                    );
                    seg_off = seg_off.wrapping_add(skip).wrapping_add(ptr_size);
                }
            }
            0xD0 => {
                // BIND_OPCODE_THREADED
                match imm {
                    0x00 => {
                        // BIND_SUBOPCODE_THREADED_SET_BIND_ORDINAL_TABLE_SIZE_ULEB
                        let cap = read_uleb128(slice, &mut pos) as usize;
                        threaded_symbols.reserve(cap.min(65536));
                        in_threaded_table = true;
                    }
                    0x01 => {
                        // BIND_SUBOPCODE_THREADED_APPLY
                        if let Some(&seg_base) = segment_offsets.get(seg_idx) {
                            let mut cur_off = seg_off;
                            for _ in 0..0x10_0000 {
                                let addr = seg_base
                                    .wrapping_add(cur_off as u32)
                                    .wrapping_add(slide);
                                let raw: u64 = into_mem.read(ConstPtr::from_bits(addr));
                                let delta = ((raw >> 51) & 0x7ff) * 8;
                                let is_bind = (raw & (1u64 << 62)) != 0;
                                if is_bind {
                                    let ordinal = (raw & 0xffff) as usize;
                                    if let Some((t_name, t_addend)) =
                                        threaded_symbols.get(ordinal)
                                    {
                                        let addend_u64 = (*t_addend as i32 as u32) as u64;
                                        into_mem.write(MutPtr::<u64>::from_bits(addr), addend_u64);
                                        external_relocations.push((addr, t_name.clone()));
                                    }
                                } else {
                                    let target = raw & 0x0007_ffff_ffff_ffff;
                                    let rebased = if target >= ARM64_DEFAULT_TEXT_BASE {
                                        rebase_arm64_addr(target).wrapping_add(slide)
                                    } else {
                                        ARM64_REBASED_TEXT_BASE
                                            .wrapping_add(target as u32)
                                            .wrapping_add(slide)
                                    };
                                    into_mem.write(MutPtr::<u64>::from_bits(addr), rebased as u64);
                                }
                                if delta == 0 {
                                    break;
                                }
                                cur_off = cur_off.wrapping_add(delta);
                            }
                        }
                    }
                    _ => break,
                }
            }
            _ => break,
        }
    }
}

fn find_dyld_lazy_bind_range(bytes: &[u8], is_64bit: bool) -> Option<(u64, u64)> {
    let header_size = if is_64bit { 32 } else { 28 };
    if bytes.len() < header_size {
        return None;
    }
    let ncmds = u32::from_le_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]) as usize;
    let mut off = header_size;
    for _ in 0..ncmds {
        if off + 8 > bytes.len() {
            break;
        }
        let cmd = u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
        let cmdsize =
            u32::from_le_bytes([bytes[off + 4], bytes[off + 5], bytes[off + 6], bytes[off + 7]])
                as usize;
        if cmdsize < 8 || off + cmdsize > bytes.len() {
            break;
        }
        // LC_DYLD_INFO (0x22) or LC_DYLD_INFO_ONLY (0x80000022)
        if (cmd == 0x22 || cmd == 0x8000_0022) && cmdsize >= 40 {
            let lazy_off = u32::from_le_bytes([
                bytes[off + 32],
                bytes[off + 33],
                bytes[off + 34],
                bytes[off + 35],
            ]) as u64;
            let lazy_size = u32::from_le_bytes([
                bytes[off + 36],
                bytes[off + 37],
                bytes[off + 38],
                bytes[off + 39],
            ]) as u64;
            if lazy_size > 0 {
                return Some((lazy_off, lazy_size));
            }
        }
        off += cmdsize;
    }
    None
}

/// Parsed relocation entry
#[derive(Debug)]
enum Reloc {
    External {
        addr: u32,
        sym_idx: u32,
        is_pc_relative: bool,
        size: u32,
        type_: u32,
    },
    #[allow(dead_code)]
    Local {
        addr: u32,
        section_idx: u32,
        is_pc_relative: bool,
        size: u32,
        type_: u32,
    },
    #[allow(dead_code)]
    Scattered {
        offset: u32,
        value: u32,
        is_pc_relative: bool,
        size: u32,
        type_: u32,
    },
}
impl Reloc {
    fn parse(is_bigend: bool, entry: [u8; 8]) -> Self {
        assert!(!is_bigend);

        let word1 = u32::from_le_bytes(entry[..4].try_into().unwrap());
        let word2 = u32::from_le_bytes(entry[4..8].try_into().unwrap());

        if (word1 & 0x80000000) != 0 {
            let bitfield = word1;
            let value = word2;

            let offset = bitfield & 0xffffff;
            let type_ = (bitfield >> 24) & 0xf;
            let size = 1 << ((bitfield >> 28) & 3); // log2-encoded pointer size
            let is_pc_relative = ((bitfield >> 30) & 1) == 1;

            Reloc::Scattered {
                offset,
                value,
                is_pc_relative,
                size,
                type_,
            }
        } else {
            let addr = word1;
            let bitfield = word2;

            let sym_or_section_idx = bitfield & 0xffffff;
            let is_pc_relative = ((bitfield >> 24) & 1) == 1;
            let size = 1 << ((bitfield >> 25) & 3); // log2-encoded pointer size
            let is_external = (bitfield >> 27) & 1;
            let type_ = (bitfield >> 28) & 0xf;

            if is_external == 1 {
                Reloc::External {
                    addr,
                    sym_idx: sym_or_section_idx,
                    is_pc_relative,
                    size,
                    type_,
                }
            } else {
                Reloc::Local {
                    addr,
                    section_idx: sym_or_section_idx,
                    is_pc_relative,
                    size,
                    type_,
                }
            }
        }
    }
}

/// Subtypes supported by the 32-bit iPhone OS ARM backend, in selection
/// preference order. Keep generic ARMv7 ahead of specialized ARMv7 variants.
/// ARM_ALL is a generic ARM image, not a wildcard accepting arbitrary ISAs.
fn arm_subtype_priority(subtype: cpu_subtype_t) -> Option<u8> {
    // Mach-O reserves the high byte for capability requirements. No such
    // requirements are supported by this ARM32 backend (in particular LIB64
    // is not valid here). Do not silently strip unknown required capabilities.
    if (subtype as u32) & 0xff00_0000 != 0 {
        return None;
    }
    match subtype {
        mach_object::CPU_SUBTYPE_ARM_V7 => Some(4),
        mach_object::CPU_SUBTYPE_ARM_V7S => Some(3),
        mach_object::CPU_SUBTYPE_ARM_V7F => Some(2),
        mach_object::CPU_SUBTYPE_ARM_V6 => Some(1),
        mach_object::CPU_SUBTYPE_ARM_ALL => Some(0),
        // Excludes older ARM/XScale, watchOS ARMv7k, ARMv8 and unknown values.
        _ => None,
    }
}

fn cpu_subtype_to_str(ty: cpu_subtype_t) -> &'static str {
    match ty {
        mach_object::CPU_SUBTYPE_ARM_ALL => "armv???",
        mach_object::CPU_SUBTYPE_ARM_V4T => "armv4t",
        mach_object::CPU_SUBTYPE_ARM_V5TEJ => "armv5tej",
        mach_object::CPU_SUBTYPE_ARM_V6 => "armv6",
        mach_object::CPU_SUBTYPE_ARM_XSCALE => "armxscale",
        mach_object::CPU_SUBTYPE_ARM_V7 => "armv7",
        mach_object::CPU_SUBTYPE_ARM_V7F => "armv7f",
        mach_object::CPU_SUBTYPE_ARM_V7S => "armv7s",
        mach_object::CPU_SUBTYPE_ARM_V7K => "armv7k",
        mach_object::CPU_SUBTYPE_ARM_V8 => "armv8",
        _ => {
            log!(
                "Warning: cpu_subtype_to_str: unexpected cpu subtype: {:?}; using 'armv???'.",
                ty
            );
            "armv???"
        }
    }
}

fn record_defined_symbol(
    defined_symbols: &mut HashMap<String, u32>,
    external_symbols: &mut HashMap<String, u32>,
    name: &str,
    address: u32,
    external: bool,
) {
    defined_symbols.insert(name.to_string(), address);
    if external {
        external_symbols.insert(name.to_string(), address);
    }
}

/// Only the virtual tail (vmsize - filesize) is zero-filled. Missing bytes
/// inside the declared file range are corruption, not BSS.
fn validate_segment_file_range(
    fileoff: u64,
    filesize: u64,
    vmsize: u64,
    file_len: usize,
) -> Result<(), &'static str> {
    if filesize > vmsize {
        return Err("Mach-O segment file size exceeds its virtual size");
    }
    if filesize == 0 {
        return Ok(());
    }
    let end = fileoff
        .checked_add(filesize)
        .ok_or("Mach-O segment file range overflows")?;
    if end > file_len as u64 {
        return Err("Mach-O segment extends past end of file; executable is truncated or malformed (check the IPA)");
    }
    Ok(())
}

fn validate_segment_ranges(commands: &[MachCommand], file_len: usize) -> Result<(), &'static str> {
    for MachCommand(command, _) in commands {
        if let LoadCommand::Segment {
            segname,
            fileoff,
            filesize,
            vmsize,
            ..
        }
        | LoadCommand::Segment64 {
            segname,
            fileoff,
            filesize,
            vmsize,
            ..
        } = command
        {
            // mach_object exposes these fields as usize. Use checked
            // conversions: usize does not implement Into<u64> in Rust.
            let offset =
                u64::try_from(*fileoff).map_err(|_| "Segment file offset does not fit u64")?;
            let size =
                u64::try_from(*filesize).map_err(|_| "Segment file size does not fit u64")?;
            let virtual_size =
                u64::try_from(*vmsize).map_err(|_| "Segment virtual size does not fit u64")?;
            if let Err(error) = validate_segment_file_range(offset, size, virtual_size, file_len) {
                log!("Rejecting Mach-O: segment {} fileoff={:#x} filesize={:#x} vmsize={:#x}, file length={:#x}: {}",
                    segname, fileoff, filesize, vmsize, file_len, error);
                return Err(error);
            }
        }
    }
    Ok(())
}

impl MachO {
    /// Load the all the sections from a Mach-O binary (provided as `bytes`)
    /// into the guest memory (`into_mem`), and return a struct containing
    /// metadata (e.g. symbols).
    pub fn load_from_bytes(
        bytes: &[u8],
        into_mem: &mut Mem,
        name: String,
        slide_to_address: u32,
    ) -> Result<MachO, &'static str> {
        log_dbg!("Reading {:?}", name);

        let mut cursor = Cursor::new(bytes);

        let file = OFile::parse(&mut cursor).map_err(|_| "Could not parse Mach-O file")?;

        let (header, commands) = match file {
            OFile::MachFile { header, commands } => (header, commands),
            OFile::FatFile { files, .. } => {
                let mut best_subslice: Option<&[u8]> = None;
                let mut best_priority = None;
                let mut had_invalid_slice = false;
                for (arch, _) in &files {
                    if arch.cputype != mach_object::CPU_TYPE_ARM {
                        continue;
                    }
                    let Some(priority) = arm_subtype_priority(arch.cpusubtype) else {
                        log!(
                            "Skipping unsupported ARM FAT subtype {:#x}",
                            arch.cpusubtype
                        );
                        had_invalid_slice = true;
                        continue;
                    };
                    // Per Apple's Mach-O FAT documentation, each `fat_arch`
                    // gives an absolute offset and size into the wrapping FAT
                    // file. Some malformed / partially-downloaded IPAs (e.g.
                    // Bike Race Free) have a header that describes a slice
                    // larger than the bytes we actually have on disk, which
                    // used to crash with `range end index N out of range for
                    // slice of length M`. Bounds-check here and skip the slice
                    // instead of panicking; we'll fall back to whatever other
                    // slice is present, or surface a clean error.
                    let off = arch.offset as usize;
                    let size = arch.size as usize;
                    let end = off.checked_add(size);
                    let in_bounds = matches!(end, Some(e) if e <= bytes.len());
                    if !in_bounds {
                        log!(
                            "Warning: FAT slice (cputype {:#x}, cpusubtype {:#x}) declares \
                             range {:#x}..{:#x}, past end of file ({:#x}); skipping this slice.",
                            arch.cputype,
                            arch.cpusubtype,
                            off,
                            off.saturating_add(size),
                            bytes.len(),
                        );
                        had_invalid_slice = true;
                        continue;
                    }
                    // A FAT range can fit while the Mach-O inside it declares
                    // missing segment bytes. Validate before ranking candidates,
                    // and before any guest memory is reserved for a candidate.
                    let subslice = &bytes[off..off + size];
                    let usable = match OFile::parse(&mut Cursor::new(subslice)) {
                        Ok(OFile::MachFile { header, commands }) => {
                            header.cputype == mach_object::CPU_TYPE_ARM
                                && header.cpusubtype == arch.cpusubtype
                                && arm_subtype_priority(header.cpusubtype).is_some()
                                && !header.is_64bit()
                                && !header.is_bigend()
                                && validate_segment_ranges(&commands, subslice.len()).is_ok()
                        }
                        _ => false,
                    };
                    if !usable {
                        log!(
                            "Skipping invalid ARM FAT slice at {:#x} (subtype {:#x})",
                            off,
                            arch.cpusubtype
                        );
                        had_invalid_slice = true;
                        continue;
                    }
                    if best_priority.map_or(true, |best| priority > best) {
                        best_subslice = Some(subslice);
                        best_priority = Some(priority);
                    }
                }
                // Fallback: if no 32-bit ARM slice is present in the FAT binary,
                // look for a valid 64-bit `CPU_TYPE_ARM64` slice.
                if best_subslice.is_none() {
                    for (arch, _) in &files {
                        if arch.cputype != CPU_TYPE_ARM64 {
                            continue;
                        }
                        let off = arch.offset as usize;
                        let size = arch.size as usize;
                        let Some(end) = off.checked_add(size) else {
                            had_invalid_slice = true;
                            continue;
                        };
                        if end > bytes.len() {
                            had_invalid_slice = true;
                            continue;
                        }
                        let subslice = &bytes[off..end];
                        let usable = match OFile::parse(&mut Cursor::new(subslice)) {
                            Ok(OFile::MachFile { header, commands }) => {
                                header.cputype == CPU_TYPE_ARM64
                                    && header.is_64bit()
                                    && !header.is_bigend()
                                    && validate_segment_ranges(&commands, subslice.len()).is_ok()
                            }
                            _ => false,
                        };
                        if usable {
                            best_subslice = Some(subslice);
                            break;
                        } else {
                            had_invalid_slice = true;
                        }
                    }
                }
                return if let Some(subslice) = best_subslice {
                    MachO::load_from_bytes(subslice, into_mem, name, slide_to_address)
                } else if had_invalid_slice {
                    Err("FAT binary has no usable ARM slice: slices are truncated, malformed or unsupported (check the IPA)")
                } else {
                    Err("No supported architecture in the fat binary")
                };
            }
            OFile::ArFile { .. } | OFile::SymDef { .. } => {
                return Err("Unexpected Mach-O file kind: not an executable");
            }
        };

        let is_arm64 = header.cputype == CPU_TYPE_ARM64;
        if header.cputype != mach_object::CPU_TYPE_ARM && !is_arm64 {
            return Err("Executable is not for an ARM CPU!");
        }

        let is_bigend = header.is_bigend();
        if is_bigend {
            return Err("Executable is not little-endian!");
        }
        let is_64bit = header.is_64bit();
        if is_64bit != is_arm64 {
            return Err("Executable is not 32-bit!");
        }
        if !is_arm64 && arm_subtype_priority(header.cpusubtype).is_none() {
            log!(
                "Rejecting {:?}: unsupported ARM CPU subtype {:#x}",
                name,
                header.cpusubtype
            );
            return Err("Unsupported ARM CPU subtype or capabilities (expected ARM_ALL, ARMv6, ARMv7, ARMv7f or ARMv7s)");
        }

        if is_arm64 {
            log!("Loading arm64 (64-bit ARMv8) slice for {:?}", name);
        } else {
            log!(
                "Loading {} slice for {:?}",
                cpu_subtype_to_str(header.cpusubtype),
                name
            );
        }

        // Do not mutate guest memory until every file-backed segment, including
        // __LINKEDIT, is known to fit the selected thin image.
        validate_segment_ranges(&commands, bytes.len())?;

        let split_segs = (header.flags & mach_object::MH_SPLIT_SEGS) != 0;

        // Info used while parsing file
        let mut first_segment_base: Option<u32> = None;
        let mut first_read_write_segment_base: Option<u32> = None;
        let mut text_segment_base: Option<u32> = None;
        let mut all_sections = Vec::new();
        let mut sym_tab_info: Option<(u32, u32, u32, u32)> = None;
        let mut segment_offsets = Vec::new();
        let mut last_segment_end: u32 = 0;

        // Info used for the result
        let mut dynamic_libraries = Vec::new();
        let mut exported_symbols = HashMap::new();
        let mut external_symbols = HashMap::new();
        let mut indirect_undef_symbols: Vec<Option<String>> = Vec::new();
        let mut external_relocations: Vec<(u32, String)> = Vec::new();
        let mut entry_point_pc: Option<u32> = None;
        let mut entry_point_is_lc_main = false;

        let slide = slide_to_address;
        let mut data_segment_ranges: Vec<(u32, u32)> = Vec::new();
        for MachCommand(cmd, _) in &commands {
            if let LoadCommand::SymTab {
                symoff,
                nsyms,
                stroff,
                strsize,
            } = *cmd
            {
                sym_tab_info = Some((symoff, nsyms, stroff, strsize));
            } else if let LoadCommand::Segment {
                ref segname,
                vmaddr,
                ..
            }
            | LoadCommand::Segment64 {
                ref segname,
                vmaddr,
                ..
            } = *cmd
            {
                if &**segname == "__TEXT" && text_segment_base.is_none() {
                    let v: u32 = if is_64bit {
                        rebase_arm64_addr(vmaddr as u64)
                    } else {
                        vmaddr as u32
                    };
                    text_segment_base = Some(v + slide);
                }
            }
        }

        for MachCommand(command, _size) in commands {
            match command {
                LoadCommand::Segment {
                    segname,
                    vmaddr,
                    vmsize,
                    fileoff,
                    filesize,
                    initprot,
                    sections,
                    ..
                }
                | LoadCommand::Segment64 {
                    segname,
                    vmaddr,
                    vmsize,
                    fileoff,
                    filesize,
                    initprot,
                    sections,
                    ..
                } => {
                    let vmaddr: u32 = if is_64bit {
                        rebase_arm64_addr(vmaddr as u64)
                    } else {
                        vmaddr.try_into().unwrap()
                    };
                    let vmsize: u32 = if is_64bit && &*segname == "__PAGEZERO" {
                        0x4000
                    } else {
                        vmsize.try_into().unwrap()
                    };
                    let filesize: u32 = filesize.try_into().unwrap();

                    if first_segment_base.is_none() {
                        first_segment_base = Some(vmaddr + slide);
                    }
                    if first_read_write_segment_base.is_none()
                        && (initprot & VM_PROT_READ) != 0
                        && (initprot & VM_PROT_WRITE) != 0
                    {
                        first_read_write_segment_base = Some(vmaddr + slide);
                    }

                    let load_me = match &*segname {
                        // Special linker data section, not meant to be loaded.
                        "__LINKEDIT" => false,
                        // Zero page needs to be handled seperately.
                        "__PAGEZERO" => {
                            assert!(vmaddr == 0);
                            assert!(filesize == 0);
                            into_mem.set_null_segment_size(vmsize);
                            false
                        }
                        "__TEXT" => {
                            text_segment_base = Some(vmaddr + slide);
                            true
                        }
                        "__DATA" | "__DATA_CONST" | "__DATA_DIRTY" | "__OBJC_CONST"
                        | "__AUTH" | "__AUTH_CONST" => true,
                        // `__RESTRICT` is a one-section segment Apple's
                        // linker emits to mark a binary as non-injectable
                        // (it disables `DYLD_INSERT_LIBRARIES` and the
                        // task-port debugging path in dyld). It contains a
                        // `__restrict` C-string section; the segment is
                        // documented in Apple's open-source dyld
                        // (`dyld/src/ImageLoaderMachO.cpp`) and xnu's
                        // `bsd/kern/mach_loader.c`. We don't enforce that
                        // policy in HLE, so treat the segment as a regular
                        // read-only data segment without spamming a warning.
                        "__RESTRICT" => true,
                        // `__S3E_DATA` is a data segment used by the Marmalade SDK
                        // (formerly Airplay SDK), which some older iOS games link with.
                        "__S3E_DATA" => true,
                        _ => {
                            log!("Warning: Unexpected segment name: {}", segname);
                            true
                        }
                    };

                    // Apple's xnu kernel (`bsd/kern/mach_loader.c`,
                    // `parse_machfile()` → `load_segment()`) explicitly
                    // skips `LC_SEGMENT` commands whose `vmsize == 0` —
                    // they reserve no address space and have no data.
                    // Some shipping iOS binaries (e.g. games containing a
                    // dummy `__RESTRICT` segment from older linkers) emit
                    // zero-vmsize segments; reserving 0 bytes used to
                    // panic our allocator (`NonZeroU32::new(0).unwrap()`
                    // in `Chunk::new`), so honour the kernel's contract
                    // and silently ignore them here.
                    if load_me && vmsize == 0 {
                        log_dbg!(
                            "Skipping zero-vmsize segment {} at {:#x} (matches xnu mach_loader.c behaviour)",
                            segname,
                            vmaddr + slide
                        );
                        all_sections.extend_from_slice(&sections);
                        segment_offsets.push(vmaddr);
                        last_segment_end = last_segment_end.max(vmaddr + slide);
                        continue;
                    }

                    if load_me {
                        log_dbg!(
                            "reserve {} addr {:#x} size {}",
                            segname,
                            vmaddr + slide,
                            vmsize
                        );
                        into_mem.reserve(vmaddr + slide, vmsize);

                        // If filesize is less than vmsize, the rest of the
                        // segment should be filled with zeroes. We are assuming
                        // the memory is already zeroed!
                        if filesize > 0 {
                            assert!(filesize <= vmsize);
                            let fileoff: usize = fileoff
                                .try_into()
                                .map_err(|_| "Segment file offset does not fit host usize")?;
                            let filesize_usize = filesize as usize;
                            let file_end = fileoff
                                .checked_add(filesize_usize)
                                .ok_or("Segment file range overflows host usize")?;
                            let src = &bytes[fileoff..file_end];
                            let dst =
                                into_mem.bytes_at_mut(Ptr::from_bits(vmaddr + slide), filesize);
                            dst.copy_from_slice(src);
                            if is_64bit && &*segname != "__TEXT" {
                                data_segment_ranges.push((vmaddr + slide, filesize));
                            }
                        }
                    }

                    all_sections.extend_from_slice(&sections);
                    segment_offsets.push(vmaddr);
                    last_segment_end = last_segment_end.max(vmaddr + vmsize + slide);
                }
                LoadCommand::SymTab {
                    symoff,
                    nsyms,
                    stroff,
                    strsize,
                } => {
                    let info = (symoff, nsyms, stroff, strsize);
                    sym_tab_info = Some(info);
                    for idx in 0..nsyms {
                        let Some(sym) = read_nlist_sym(bytes, idx, info, is_64bit) else {
                            continue;
                        };
                        if let ParsedSym::Defined {
                            name: Some(name),
                            external,
                            entry,
                            desc,
                        } = sym
                        {
                            let entry: u32 = if is_64bit {
                                rebase_arm64_addr(entry)
                            } else {
                                entry as u32
                            };
                            let entry = if !is_64bit && (desc & N_ARM_THUMB_DEF != 0) {
                                entry | GuestFunction::THUMB_BIT
                            } else {
                                entry
                            };
                            record_defined_symbol(
                                &mut exported_symbols,
                                &mut external_symbols,
                                name,
                                slide.wrapping_add(entry),
                                external,
                            );
                        }
                    }
                }
                LoadCommand::DySymTab {
                    indirectsymoff,
                    nindirectsyms,
                    extreloff,
                    nextrel,
                    ..
                } => {
                    let indirectsyms = {
                        let start = indirectsymoff as usize;
                        let len = (nindirectsyms as usize).saturating_mul(4);
                        if len == 0 || start >= bytes.len() || start.saturating_add(len) > bytes.len() {
                            &[][..]
                        } else {
                            &bytes[start..start + len]
                        }
                    };
                    if let Some(info) = sym_tab_info {
                        for idx_bytes in indirectsyms.chunks_exact(4) {
                            let idx = u32::from_le_bytes([
                                idx_bytes[0],
                                idx_bytes[1],
                                idx_bytes[2],
                                idx_bytes[3],
                            ]);
                            let sym = read_nlist_sym(bytes, idx, info, is_64bit);
                            indirect_undef_symbols.push(match sym {
                                Some(ParsedSym::Undefined { name }) => name.map(String::from),
                                Some(ParsedSym::Prebound { name }) => name.map(String::from),
                                Some(ParsedSym::Defined { name, .. }) => name.map(String::from),
                                Some(ParsedSym::Debug) | None => None,
                            });
                        }
                    }

                    let extrels = {
                        let start = extreloff as usize;
                        let len = (nextrel as usize).saturating_mul(8);
                        if len == 0 || start >= bytes.len() || start.saturating_add(len) > bytes.len() {
                            &[][..]
                        } else {
                            &bytes[start..start + len]
                        }
                    };
                    for entry in extrels.chunks_exact(8) {
                        let entry_arr: [u8; 8] = [
                            entry[0], entry[1], entry[2], entry[3], entry[4], entry[5], entry[6],
                            entry[7],
                        ];
                        let reloc = Reloc::parse(is_bigend, entry_arr);
                        let Reloc::External {
                            addr,
                            sym_idx,
                            is_pc_relative: false,
                            size,
                            type_: 0,
                        } = reloc
                        else {
                            continue;
                        };
                        if size != 4 && !(is_64bit && size == 8) {
                            continue;
                        }
                        let base_seg = if split_segs {
                            first_read_write_segment_base
                                .or(first_segment_base)
                                .unwrap_or(0)
                        } else {
                            first_segment_base.unwrap_or(0)
                        };
                        let addr = addr.wrapping_add(base_seg);

                        let Some(info) = sym_tab_info else {
                            continue;
                        };
                        match read_nlist_sym(bytes, sym_idx, info, is_64bit) {
                            Some(ParsedSym::Undefined { name: Some(n) }) => {
                                external_relocations.push((addr, String::from(n)));
                            }
                            Some(ParsedSym::Defined { entry, desc, .. }) => {
                                let entry = if is_64bit {
                                    rebase_arm64_addr(entry).wrapping_add(slide)
                                } else {
                                    let e = (entry as u32).wrapping_add(slide);
                                    if desc & N_ARM_THUMB_DEF != 0 {
                                        e | GuestFunction::THUMB_BIT
                                    } else {
                                        e
                                    }
                                };
                                if is_64bit && size == 8 {
                                    let addr_ptr: MutPtr<u64> = Ptr::from_bits(addr);
                                    let addend: u64 = into_mem.read(addr_ptr);
                                    let addend_low = (addend & 0xffff_ffff) as u32;
                                    into_mem.write(addr_ptr, (entry.wrapping_add(addend_low)) as u64);
                                } else {
                                    let addr_ptr: MutPtr<u32> = Ptr::from_bits(addr);
                                    let addend: u32 = into_mem.read(addr_ptr);
                                    into_mem.write(addr_ptr, entry.wrapping_add(addend));
                                }
                            }
                            Some(ParsedSym::Prebound { name: Some(n) }) => {
                                if is_64bit && size == 8 {
                                    let ptr_ptr = Ptr::<u64, true>::from_bits(addr);
                                    into_mem.write(ptr_ptr, 0);
                                } else {
                                    let ptr_ptr = Ptr::<u32, true>::from_bits(addr);
                                    into_mem.write(ptr_ptr, 0);
                                }
                                external_relocations.push((addr, String::from(n)));
                            }
                            _ => {}
                        };
                    }
                }
                LoadCommand::EncryptionInfo { id, .. } => {
                    if id != 0 {
                        return Err(
                            "The executable is encrypted. touchHLE can't run encrypted apps!",
                        );
                    }
                }
                LoadCommand::LoadDyLib(DyLib { name, .. }) => {
                    dynamic_libraries.push(String::from(&*name));
                }
                LoadCommand::UnixThread { state, .. } => {
                    if let ThreadState::Arm { __pc, .. } = state {
                        if entry_point_pc.is_none() {
                            entry_point_pc = Some(__pc);
                        }
                    }
                }
                LoadCommand::EntryPoint {
                    entryoff,
                    stacksize,
                } => {
                    if stacksize != 0 {
                        log!("TODO: stack size of {:#x} bytes requested", stacksize);
                    }
                    let entryoff: u32 = entryoff as u32;
                    let base = text_segment_base
                        .or(first_segment_base)
                        .unwrap_or(if is_64bit { ARM64_REBASED_TEXT_BASE + slide } else { slide });
                    entry_point_pc = Some(base.wrapping_add(entryoff));
                    entry_point_is_lc_main = true;
                }
                LoadCommand::DyldInfo {
                    rebase_off,
                    rebase_size,
                    bind_off,
                    bind_size,
                    weak_bind_off,
                    weak_bind_size,
                    ..
                } => {
                    fn checked_dyld_info_slice<'a>(
                        bytes: &'a [u8],
                        off: u64,
                        size: u64,
                    ) -> &'a [u8] {
                        let Ok(off_usize) = usize::try_from(off) else {
                            return &[];
                        };
                        let Ok(size_usize) = usize::try_from(size) else {
                            return &[];
                        };
                        let Some(end) = off_usize.checked_add(size_usize) else {
                            return &[];
                        };
                        if end > bytes.len() {
                            return &[];
                        }
                        &bytes[off_usize..end]
                    }

                    let ptr_size: u64 = if is_64bit { 8 } else { 4 };

                    if rebase_size > 0 {
                        let slice = checked_dyld_info_slice(
                            bytes,
                            rebase_off as u64,
                            rebase_size as u64,
                        );
                        apply_dyld_rebase(
                            slice,
                            ptr_size,
                            is_64bit,
                            &segment_offsets,
                            slide,
                            into_mem,
                        );
                    }

                    if bind_size > 0 {
                        let slice = checked_dyld_info_slice(
                            bytes,
                            bind_off as u64,
                            bind_size as u64,
                        );
                        apply_dyld_bind(
                            slice,
                            ptr_size,
                            is_64bit,
                            false,
                            false,
                            &segment_offsets,
                            slide,
                            into_mem,
                            &mut external_relocations,
                        );
                    }

                    if weak_bind_size > 0 {
                        let slice = checked_dyld_info_slice(
                            bytes,
                            weak_bind_off as u64,
                            weak_bind_size as u64,
                        );
                        apply_dyld_bind(
                            slice,
                            ptr_size,
                            is_64bit,
                            true,
                            false,
                            &segment_offsets,
                            slide,
                            into_mem,
                            &mut external_relocations,
                        );
                    }

                    if let Some((lazy_off, lazy_size)) = find_dyld_lazy_bind_range(bytes, is_64bit) {
                        let slice = checked_dyld_info_slice(bytes, lazy_off, lazy_size);
                        apply_dyld_bind(
                            slice,
                            ptr_size,
                            is_64bit,
                            false,
                            true,
                            &segment_offsets,
                            slide,
                            into_mem,
                            &mut external_relocations,
                        );
                    }
                }
                _ => (),
            }
        }

        if is_64bit {
            for (seg_addr, seg_len) in data_segment_ranges {
                let dst = into_mem.bytes_at_mut(Ptr::from_bits(seg_addr), seg_len);
                for chunk in dst.chunks_exact_mut(8) {
                    let raw = u64::from_le_bytes([
                        chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6],
                        chunk[7],
                    ]);
                    if raw >= ARM64_DEFAULT_TEXT_BASE
                        && raw < ARM64_DEFAULT_TEXT_BASE + 0x4000_0000
                    {
                        let rebased = (rebase_arm64_addr(raw).wrapping_add(slide)) as u64;
                        chunk.copy_from_slice(&rebased.to_le_bytes());
                    }
                }
            }
        }

        if entry_point_pc.is_none() && is_arm64 {
            if let Some(&main_pc) = exported_symbols
                .get("_main")
                .or_else(|| exported_symbols.get("start"))
            {
                entry_point_pc = Some(main_pc);
                entry_point_is_lc_main = true;
            }
        }

        let sections = all_sections
            .iter()
            .map(|section| {
                let section = &**section;

                let name = section.sectname.clone();
                let raw_addr: u32 = if is_64bit {
                    rebase_arm64_addr(section.addr as u64)
                } else {
                    TryInto::<u32>::try_into(section.addr).unwrap()
                };
                let addr: u32 = raw_addr + slide;
                let size: u32 = section.size.try_into().unwrap();
                let type_ = section.flags.sect_type();

                log_dbg!(
                    "Section: {:?} {:#x} ({:#x} bytes), type {}",
                    name,
                    addr,
                    size,
                    type_,
                );

                let ptr_size: u32 = if is_64bit { 8 } else { 4 };
                use SectionType as ST;
                let (type_, dyld_entry_size) = match type_ {
                    S_MOD_INIT_FUNC_POINTERS => (ST::ModInitFuncPointers, None),
                    // Symbol stub sections have a variable size depending on
                    // whether PIC is in use.
                    S_SYMBOL_STUBS => (ST::SymbolStubs, Some(section.reserved2)),
                    // 4 bytes per pointer (8 bytes on arm64)
                    S_LAZY_SYMBOL_POINTERS => (ST::LazySymbolPointers, Some(ptr_size)),
                    S_NON_LAZY_SYMBOL_POINTERS => (ST::NonLazySymbolPointers, Some(ptr_size)),
                    _ => (ST::Normal, None),
                };
                let dyld_indirect_symbol_info = dyld_entry_size.and_then(|entry_size| {
                    if entry_size == 0 || !size.is_multiple_of(entry_size) {
                        log!(
                            "Warning: section {:?} has invalid entry size {} for size {:#x}; skipping indirect symbol info.",
                            name,
                            entry_size,
                            size
                        );
                        return None;
                    }
                    let indirect_start = section.reserved1 as usize;
                    let indirect_count = (size / entry_size) as usize;
                    // Corrupt or trimmed binaries (e.g. Bug Heroes Quest) can
                    // declare an indirect symbol table that is past EOF; in
                    // that case the table above was skipped and is empty.
                    // Pad with anonymous entries instead of panicking: the
                    // dyld code skips `None` entries gracefully.
                    let indirects = match indirect_undef_symbols
                        .get(indirect_start..)
                        .and_then(|s| s.get(..indirect_count))
                    {
                        Some(s) => s.to_vec(),
                        None => {
                            log!(
                                "Warning: indirect symbol entries [{}, {}) out of range for table of length {}; treating as anonymous.",
                                indirect_start,
                                indirect_start + indirect_count,
                                indirect_undef_symbols.len()
                            );
                            vec![None; indirect_count]
                        }
                    };
                    Some(DyldIndirectSymbolInfo {
                        entry_size,
                        indirect_undef_symbols: indirects,
                    })
                });

                Section {
                    name,
                    addr,
                    size,
                    type_,
                    dyld_indirect_symbol_info,
                }
            })
            .collect();

        Ok(MachO {
            name,
            dynamic_libraries,
            sections,
            exported_symbols,
            external_symbols,
            external_relocations,
            entry_point_pc,
            entry_point_is_lc_main,
            last_segment_end,
            text_base: text_segment_base.unwrap_or(first_segment_base.unwrap_or(0)),
            is_arm64,
        })
    }

    /// Load the all the sections from a Mach-O binary (from `path`) into the
    /// guest memory (`into_mem`), and return a struct containing metadata
    /// (e.g. symbols).
    pub fn load_from_file<P: AsRef<GuestPath>>(
        path: P,
        fs: &Fs,
        into_mem: &mut Mem,
        slide_to_address: u32,
    ) -> Result<MachO, &'static str> {
        let name = path.as_ref().file_name().unwrap().to_string();
        Self::load_from_bytes(
            &fs.read(path.as_ref())
                .map_err(|_| "Could not read executable file")?,
            into_mem,
            name,
            slide_to_address,
        )
    }

    /// Get a section by its name (`&str`) or type ([SectionType]).
    pub fn get_section<P: SectionPredicate>(&self, by: P) -> Option<&Section> {
        self.sections.iter().find(|section| by.test(section))
    }
}

#[cfg(test)]
mod segment_validation_tests {
    use super::*;

    #[test]
    fn missing_file_bytes_are_not_bss() {
        // Ranges from the BH Quest crash report.
        assert!(validate_segment_file_range(0, 0x427000, 0x427000, 0x32fad0).is_err());
        assert!(validate_segment_file_range(0x427000, 0x23000, 0x23000, 0x32fad0).is_err());
        assert!(validate_segment_file_range(0x100, 0x100, 0x1000, 0x200).is_ok());
        assert!(validate_segment_file_range(0x100, 0x100, 0x1000, 0x1ff).is_err());
    }

    #[test]
    fn empty_segments_and_overflow() {
        assert!(validate_segment_file_range(u64::MAX, 0, 0x1000, 0).is_ok());
        assert!(validate_segment_file_range(0, 0, 0, 0).is_ok());
        assert!(validate_segment_file_range(u64::MAX, 1, 1, usize::MAX).is_err());
        assert!(validate_segment_file_range(0, 2, 1, 2).is_err());
    }

    // Minimal 32-bit ARM Mach-O with a single LC_SEGMENT and no sections.
    fn thin(subtype: u32, address: u32, file_size: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        for word in [0xfeedfaceu32, 12, subtype, 2, 1, 56, 0, 1, 56] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        let mut name = [0u8; 16];
        name[..6].copy_from_slice(b"__TEXT");
        bytes.extend_from_slice(&name);
        for word in [address, 0x2000, 0, file_size, 7, 5, 0, 0] {
            bytes.extend_from_slice(&word.to_le_bytes());
        }
        assert_eq!(bytes.len(), 84);
        bytes
    }

    #[test]
    fn thin_loader_rejects_truncation() {
        let bytes = thin(6, 0x1000, 128);
        let mut mem = Mem::new();
        let result = MachO::load_from_bytes(&bytes, &mut mem, "truncated".into(), 0);
        assert!(matches!(result, Err(e) if e.contains("extends past end of file")));
    }

    #[test]
    fn fat_loader_falls_back_from_internally_truncated_armv7() {
        let invalid = thin(9, 0x1000, 128); // ARMv7, preferred but incomplete
        let valid = thin(6, 0x9000, 84); // ARMv6, complete with a virtual zero tail
        let mut bytes = Vec::new();
        for word in [0xcafebabeu32, 2, 12, 9, 48, 84, 0, 12, 6, 132, 84, 0] {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
        bytes.extend_from_slice(&invalid);
        bytes.extend_from_slice(&valid);
        let mut mem = Mem::new();
        let image = MachO::load_from_bytes(&bytes, &mut mem, "fat-test".into(), 0).unwrap();
        assert_eq!(image.text_base, 0x9000);
        assert_eq!(
            mem.bytes_at(Ptr::<u8, false>::from_bits(0x9000), 84),
            &valid[..]
        );
        assert!(mem
            .bytes_at(Ptr::<u8, false>::from_bits(0x9000 + 84), 16)
            .iter()
            .all(|&b| b == 0));
    }

    #[test]
    fn arm_subtype_allowlist_and_capabilities() {
        for subtype in [0, 6, 9, 10, 11] {
            assert!(arm_subtype_priority(subtype).is_some());
        }
        for subtype in [
            5u32,
            7,
            8,
            12,
            13,
            14,
            0x1234,
            0x8000_0009,
            0x0100_0006,
            u32::MAX,
        ] {
            assert!(arm_subtype_priority(subtype as cpu_subtype_t).is_none());
        }
        let priorities: Vec<_> = [0, 6, 10, 11, 9]
            .into_iter()
            .map(|s| arm_subtype_priority(s).unwrap())
            .collect();
        assert!(priorities.windows(2).all(|p| p[0] < p[1]));
    }

    #[test]
    fn thin_loader_rejects_unsupported_cpu_subtype() {
        for subtype in [12, 13, 0x1234, 0x8000_0009] {
            let bytes = thin(subtype, 0x1000, 84);
            let mut mem = Mem::new();
            let result = MachO::load_from_bytes(&bytes, &mut mem, "unsupported".into(), 0);
            assert!(matches!(result, Err(e) if e.contains("Unsupported ARM CPU subtype")));
        }
    }

    fn fat_pair(first_type: u32, first: &[u8], second_type: u32, second: &[u8]) -> Vec<u8> {
        assert_eq!((first.len(), second.len()), (84, 84));
        let mut bytes = Vec::new();
        for word in [
            0xcafebabeu32,
            2,
            12,
            first_type,
            48,
            84,
            0,
            12,
            second_type,
            132,
            84,
            0,
        ] {
            bytes.extend_from_slice(&word.to_be_bytes());
        }
        bytes.extend_from_slice(first);
        bytes.extend_from_slice(second);
        bytes
    }

    #[test]
    fn fat_loader_skips_unsupported_and_mismatched_subtypes() {
        for (table_subtype, header_subtype) in [(12, 12), (9, 6), (9, 13)] {
            let bytes = fat_pair(
                table_subtype,
                &thin(header_subtype, 0x1000, 84),
                6,
                &thin(6, 0x9000, 84),
            );
            let mut mem = Mem::new();
            let image = MachO::load_from_bytes(&bytes, &mut mem, "subtypes".into(), 0).unwrap();
            assert_eq!(image.text_base, 0x9000);
        }
    }

    #[test]
    fn fat_loader_prefers_armv7_regardless_of_slice_order() {
        for reverse in [false, true] {
            let v6 = thin(6, 0x1000, 84);
            let v7 = thin(9, 0x9000, 84);
            let bytes = if reverse {
                fat_pair(9, &v7, 6, &v6)
            } else {
                fat_pair(6, &v6, 9, &v7)
            };
            let mut mem = Mem::new();
            let image = MachO::load_from_bytes(&bytes, &mut mem, "preference".into(), 0).unwrap();
            assert_eq!(image.text_base, 0x9000);
        }
    }

    #[test]
    fn fat_loader_rejects_images_with_no_supported_subtype() {
        let bytes = fat_pair(12, &thin(12, 0x1000, 84), 13, &thin(13, 0x9000, 84));
        let mut mem = Mem::new();
        assert!(MachO::load_from_bytes(&bytes, &mut mem, "unsupported-fat".into(), 0).is_err());
    }

    #[test]
    fn thin_loader_accepts_supported_cpu_subtypes() {
        for subtype in [0, 6, 9, 10, 11] {
            let bytes = thin(subtype, 0x9000, 84);
            let mut mem = Mem::new();
            let image = MachO::load_from_bytes(&bytes, &mut mem, "supported".into(), 0).unwrap();
            assert_eq!(image.text_base, 0x9000);
        }
    }
}

#[cfg(test)]
mod symbol_visibility_tests {
    use super::*;

    #[test]
    fn local_symbols_are_not_visible_to_dependent_dylibs() {
        let mut defined_symbols = HashMap::new();
        let mut external_symbols = HashMap::new();

        record_defined_symbol(
            &mut defined_symbols,
            &mut external_symbols,
            "_log2",
            0x0048_85c0,
            false,
        );
        record_defined_symbol(
            &mut defined_symbols,
            &mut external_symbols,
            "_memcpy",
            0x0030_1001,
            true,
        );

        assert_eq!(defined_symbols.get("_log2"), Some(&0x0048_85c0));
        assert!(!external_symbols.contains_key("_log2"));
        assert_eq!(external_symbols.get("_memcpy"), Some(&0x0030_1001));
    }
}
