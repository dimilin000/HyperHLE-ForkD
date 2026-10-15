/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/
 */
//! Tells "an instruction dynarmic does not implement" apart from "the PC is
//! executing data".
//!
//! A guest `UndefinedInstruction` covers two very different situations. Either
//! the guest really did execute an instruction this dynarmic build does not
//! implement, in which case the function around it is still meaningful and the
//! instruction can be stepped over as a no-op; or the PC left the instruction
//! stream altogether and is running through bytes that were never code — a
//! literal pool or switch table inside `__text`, a compressed or encrypted
//! asset, the target of a bad function pointer. In the second case stepping
//! over the bytes walks the guest deeper into the blob, one undecodable
//! encoding (and one log line) at a time, and the only useful recovery is to
//! get out of it.
//!
//! Telling the two apart properly needs an instruction decoder, which is
//! exactly what just failed. The structural test used here is the cheap one
//! that works anyway: compiled Thumb-2 interleaves 16-bit and 32-bit
//! instructions constantly, so an unbroken run of 32-bit encodings means the
//! bytes were not laid out by a compiler.

/// How many consecutive 32-bit Thumb encodings still look like code.
///
/// Four 32-bit encodings back to back — 16 bytes with no 16-bit instruction
/// among them — is already rare in compiled Thumb-2, and this test only ever
/// runs at an address where dynarmic has refused to decode, which should not
/// happen in a real instruction stream at all.
const MAX_CONSECUTIVE_32BIT_ENCODINGS: u32 = 3;

/// How far the run scan walks in each direction, in instructions. This only
/// bounds the work done for a large data blob; a run long enough to matter is
/// decided well before it.
const MAX_RUN_SCAN: u32 = 8;

/// Whether `hw` can be the first halfword of a 32-bit Thumb instruction, i.e.
/// bits 15-11 are 0b11101, 0b11110 or 0b11111. Those three spaces are
/// contiguous, so this is just "0xe800 or above". 0b11100 (0xe000-0xe7ff) is
/// not one of them: that is where the 16-bit branches, `udf` and `svc` live.
fn is_32bit_first_halfword(hw: u16) -> bool {
    hw >= 0xe800
}

/// How many 32-bit Thumb encodings in a row cover `pc`.
///
/// Scanning forwards is unambiguous: a halfword with bits 15-11 in {11101,
/// 11110, 11111} starts a 32-bit encoding, so the next instruction begins 4
/// bytes later. Scanning backwards applies the same test to `pc - 4`, `pc -
/// 8`, …: if the halfword there starts a 32-bit encoding, that encoding ends
/// exactly where the next one begins.
fn consecutive_32bit_encodings(pc: u32, read_u16: impl Fn(u32) -> Option<u16>) -> u32 {
    let starts_32bit = |addr: u32| read_u16(addr).is_some_and(is_32bit_first_halfword);
    let mut count = 1;
    let mut addr = pc.wrapping_add(4);
    for _ in 0..MAX_RUN_SCAN {
        if !starts_32bit(addr) {
            break;
        }
        count += 1;
        addr = addr.wrapping_add(4);
    }
    let mut addr = pc.wrapping_sub(4);
    for _ in 0..MAX_RUN_SCAN {
        if !starts_32bit(addr) {
            break;
        }
        count += 1;
        addr = addr.wrapping_sub(4);
    }
    count
}

/// Whether the instruction dynarmic could not decode at `pc` is plausibly
/// real code — one this dynarmic build does not implement — rather than the
/// PC running through data that happens to sit inside a code section.
///
/// `read_u16` reads a guest halfword and returns `None` for unmapped memory,
/// so a run also ends at the edge of a section.
pub(super) fn undecodable_site_is_likely_code(
    pc: u32,
    thumb: bool,
    instruction_len: u32,
    read_u16: impl Fn(u32) -> Option<u16>,
) -> bool {
    // In A32 state every word is some encoding, so there is no cheap
    // structural test that separates the two cases here. Keep stepping over
    // it, as before.
    if !thumb {
        return true;
    }
    // A 2-byte fault is a 16-bit encoding dynarmic rejected, which only the
    // permanently-undefined halfwords can be. The run below counts 32-bit
    // encodings, which this instruction is not part of.
    if instruction_len != 4 {
        return true;
    }
    consecutive_32bit_encodings(pc, read_u16) <= MAX_CONSECUTIVE_32BIT_ENCODINGS
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Halfword reader for a contiguous run of halfwords starting at `base`,
    /// standing in for guest memory: anything outside it is unmapped.
    fn reader(base: u32, halfwords: &[u16]) -> impl Fn(u32) -> Option<u16> + '_ {
        move |addr: u32| {
            if addr < base {
                return None;
            }
            let offset = (addr - base) as usize;
            if offset % 2 != 0 {
                return None;
            }
            halfwords.get(offset / 2).copied()
        }
    }

    /// Parse the `"02ee c9c5 …"` halfword listing that
    /// `Environment::dump_guest_code_around` puts in the log, so the tests
    /// below can use it verbatim.
    fn parse(base: u32, listing: &str) -> (u32, Vec<u16>) {
        let halfwords = listing
            .split_whitespace()
            .map(|text| u16::from_str_radix(text, 16).unwrap())
            .collect();
        (base, halfwords)
    }

    fn is_likely_code(base: u32, halfwords: &[u16], pc: u32, instruction_len: u32) -> bool {
        undecodable_site_is_likely_code(pc, true, instruction_len, reader(base, halfwords))
    }

    /// Asphalt 8: `dump_guest_code_around` output for the fault at 0x1dd34,
    /// where the PC arrived through an indirect call at 0x1e3c2 and landed in
    /// a blob of data inside `__text`. Faults were reported at 0x1dd34,
    /// 0x1dd40, 0x1dd48, 0x1dd4c and 0x1dd50 as the guest was stepped through
    /// it.
    const DATA_BLOB_AT_0X1DD24: &str = "02ee c9c5 0128 c9c5 ffb2 cdf4 fe83 d2f4 \
                                        fb6a d5f2 fd7b d8a2 00bd dc41 ff39 d902 \
                                        fcc2 d712 face d9ac f94c d5c9 f82d d5c9 \
                                        f763 d4ad f853 d2af f742 d1ca";

    #[test]
    fn data_blob_inside_text_is_not_code() {
        let (base, halfwords) = parse(0x1dd24, DATA_BLOB_AT_0X1DD24);
        for pc in [0x1dd34u32, 0x1dd40, 0x1dd48, 0x1dd4c, 0x1dd50] {
            assert!(
                !is_likely_code(base, &halfwords, pc, 4),
                "{pc:#x} should be recognised as data, not code"
            );
        }
    }

    /// The other dump from the same log: faults at 0x1e1ec and 0x1e1f0, with
    /// LR still 0x1e3c7.
    const DATA_BLOB_AT_0X1E1DC: &str = "0ffa 1946 0a07 173d 079a 1e8d 00e7 20be \
                                        f95f 1ec6 f713 2412 f057 1dba eb1c 1bd4";

    #[test]
    fn second_data_blob_from_the_same_log_is_not_code() {
        let (base, halfwords) = parse(0x1e1dc, DATA_BLOB_AT_0X1E1DC);
        for pc in [0x1e1ecu32, 0x1e1f0] {
            assert!(
                !is_likely_code(base, &halfwords, pc, 4),
                "{pc:#x} should be recognised as data, not code"
            );
        }
    }

    #[test]
    fn unimplemented_instruction_in_real_code_is_code() {
        // push {r4, lr} / mov r4, r0 / a 32-bit VFP encoding / adds r0, #1 /
        // pop {r4, pc}: a 32-bit instruction with 16-bit ones on both sides,
        // which is how a compiler emits one dynarmic may not implement.
        let (base, halfwords) = parse(0x2000, "b510 4604 ee20 0a00 3001 bd10");
        assert!(is_likely_code(base, &halfwords, 0x2004, 4));
    }

    #[test]
    fn run_of_three_32bit_encodings_is_still_code() {
        // push {r4, lr} / movw r0 / movt r0 / bl / pop {r4, pc}
        let (base, halfwords) = parse(0x2000, "b510 f240 0030 f2c0 0030 f7ff fffe bd10");
        assert!(is_likely_code(base, &halfwords, 0x2002, 4));
    }

    #[test]
    fn run_of_four_32bit_encodings_is_data() {
        // The same function body with one more 32-bit encoding in a row.
        let (base, halfwords) = parse(0x2000, "b510 f240 0030 f2c0 0030 f7ff fffe f7ff fffe bd10");
        assert!(!is_likely_code(base, &halfwords, 0x2002, 4));
    }

    #[test]
    fn arm_state_is_left_alone() {
        // A32 faults keep the historical behaviour: no halfword reader is
        // even consulted, so an empty one is enough.
        let read_u16 = reader(0, &[]);
        assert!(undecodable_site_is_likely_code(0x1000, false, 4, read_u16));
    }

    #[test]
    fn two_byte_fault_is_left_alone() {
        // Even inside a long run of 32-bit-looking data: a 2-byte fault is a
        // 16-bit encoding, which the run says nothing about.
        let (base, halfwords) = parse(0x1dd24, DATA_BLOB_AT_0X1DD24);
        assert!(is_likely_code(base, &halfwords, 0x1dd34, 2));
    }

    #[test]
    fn first_halfword_mask_covers_all_three_32bit_spaces() {
        // Bits 15-11 of 0b11101, 0b11110 and 0b11111 start a 32-bit encoding;
        // 0b11100 does not.
        assert!(is_32bit_first_halfword(0xe800)); // ldm.w/stm.w, ldrd/strd
        assert!(is_32bit_first_halfword(0xebff));
        assert!(is_32bit_first_halfword(0xf000)); // bl, movw, data-processing
        assert!(is_32bit_first_halfword(0xf7ff));
        assert!(is_32bit_first_halfword(0xf800)); // ldr.w/str.w, multiply
        assert!(is_32bit_first_halfword(0xffff));
        assert!(!is_32bit_first_halfword(0xe7ff)); // b, svc, udf
        assert!(!is_32bit_first_halfword(0xe000));
        assert!(!is_32bit_first_halfword(0xb510)); // push {r4, lr}
        assert!(!is_32bit_first_halfword(0x4604)); // mov r4, r0
    }

    #[test]
    fn unmapped_memory_ends_the_run() {
        let (base, halfwords) = parse(0x1000, "f000 f800");
        assert_eq!(
            consecutive_32bit_encodings(0x1000, reader(base, &halfwords)),
            1
        );
    }

    #[test]
    fn run_counts_both_directions() {
        // Three 32-bit encodings either side of the middle one.
        let (base, halfwords) = parse(
            0x1000,
            "b510 f000 f800 f240 0030 f2c0 0030 f850 0020 f7ff fffe bd10",
        );
        assert_eq!(
            consecutive_32bit_encodings(0x1006, reader(base, &halfwords)),
            5
        );
    }
}
