/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! CPU emulation.
//!
//! Implemented using the C++ library dynarmic (for 32-bit ARMv6/ARMv7-A) and
//! the built-in 64-bit ARMv8-A (`AArch64`) interpreter (`arm64`).
// KlugKlugTG was here!
pub mod arm64 {
/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! 64-bit ARMv8-A (AArch64 / A64) CPU execution engine and AAPCS64 bridge.
//!
//! When a 64-bit (`CPU_TYPE_ARM64`) Mach-O binary is loaded into the 32-bit
//! `< 4 GiB` guest address space (with `__TEXT` rebased from `0x1_0000_0000`
//! to `0x0010_0000`), all valid guest pointers fit in 32 bits (`0x0000_0000_xxxx_xxxx`).
//! This engine executes A64 instructions directly against [crate::mem::Mem]
//! and synchronises `X0..X12`, `SP`, `X30 (LR)`, and `PC` with the 16-register
//! array exposed by [super::Cpu] at host call (`SVC`) boundaries.

use crate::mem::{ConstPtr, Mem, Ptr};
use super::{CpuError, CpuState};

#[derive(Clone)]
pub struct Arm64Cpu {
    /// General-purpose registers X0..X30 (`X31` is ZR or SP depending on instruction).
    pub x: [u64; 31],
    /// Stack pointer (`SP`).
    pub sp: u64,
    /// Program counter (`PC`).
    pub pc: u64,
    /// 128-bit SIMD / FP registers V0..V31 (stored as `[low64, high64]`).
    pub v: [[u64; 2]; 32],
    /// Condition flags (N, Z, C, V in bits 31..28).
    pub nzcv: u32,
    pub fpcr: u32,
    pub fpsr: u32,
    pub tpidrro_el0: u64,
    pub tpidr_el0: u64,
    pub last_exception_pc: u32,
    /// Snapshot of `regs[0..16]` at the end of the previous `run_or_step`, used
    /// to detect which registers were modified by host code between runs.
    last_synced_regs: [u32; 16],
    /// Scratch buffer in guest memory used for `R::SIZE_IN_MEM` struct returns
    /// in 64-bit mode (where the caller does not pass a hidden `r0` pointer).
    pub stret_scratch_addr: u32,
}

impl Default for Arm64Cpu {
    fn default() -> Self {
        Self::new()
    }
}

impl Arm64Cpu {
    pub fn new() -> Self {
        Self {
            x: [0; 31],
            sp: 0,
            pc: 0,
            v: [[0; 2]; 32],
            nzcv: 0,
            fpcr: 0,
            fpsr: 0,
            tpidrro_el0: 0,
            tpidr_el0: 0,
            last_exception_pc: 0,
            last_synced_regs: [0; 16],
            stret_scratch_addr: 0,
        }
    }

    #[inline]
    pub fn read_x(&self, reg: usize) -> u64 {
        if reg < 31 {
            self.x[reg]
        } else {
            0
        }
    }

    #[inline]
    pub fn read_x_or_sp(&self, reg: usize) -> u64 {
        if reg < 31 {
            self.x[reg]
        } else {
            self.sp
        }
    }

    #[inline]
    pub fn write_x(&mut self, reg: usize, val: u64) {
        if reg < 31 {
            self.x[reg] = val;
        }
    }

    #[inline]
    pub fn write_x_or_sp(&mut self, reg: usize, val: u64) {
        if reg < 31 {
            self.x[reg] = val;
        } else {
            self.sp = val;
        }
    }

    #[inline]
    pub fn write_w(&mut self, reg: usize, val: u32) {
        if reg < 31 {
            self.x[reg] = val as u64;
        }
    }

    #[inline]
    pub fn write_w_or_sp(&mut self, reg: usize, val: u32) {
        if reg < 31 {
            self.x[reg] = val as u64;
        } else {
            self.sp = val as u64;
        }
    }

    #[inline]
    pub fn read_d(&self, reg: usize) -> f64 {
        f64::from_bits(self.v[reg & 31][0])
    }

    #[inline]
    pub fn write_d(&mut self, reg: usize, val: f64) {
        self.v[reg & 31][0] = val.to_bits();
        self.v[reg & 31][1] = 0;
    }

    #[inline]
    pub fn read_s(&self, reg: usize) -> f32 {
        let raw = self.v[reg & 31][0];
        if (raw >> 32) == 0 {
            f32::from_bits(raw as u32)
        } else {
            f64::from_bits(raw) as f32
        }
    }

    #[inline]
    pub fn write_s(&mut self, reg: usize, val: f32) {
        self.v[reg & 31][0] = val.to_bits() as u64;
        self.v[reg & 31][1] = 0;
    }

    /// Synchronise host changes from the 32-bit `regs[0..16]` array into the
    /// 64-bit register file before resuming execution.
    pub fn sync_from_regs32(&mut self, regs: &[u32; 16]) {
        for i in 0..13 {
            if regs[i] != self.last_synced_regs[i] {
                self.x[i] = regs[i] as u64;
            }
        }
        if regs[13] != self.last_synced_regs[13] {
            self.sp = regs[13] as u64;
        }
        if regs[14] != self.last_synced_regs[14] {
            self.x[30] = (regs[14] & !1) as u64;
        }
        if regs[15] != self.last_synced_regs[15] {
            self.pc = (regs[15] & !1) as u64;
        }
    }

    /// Synchronise the 64-bit register file back into the 32-bit `regs[0..16]`
    /// array when halting execution.
    pub fn sync_to_regs32(&mut self, regs: &mut [u32; 16]) {
        for i in 0..13 {
            regs[i] = self.x[i] as u32;
        }
        regs[13] = self.sp as u32;
        regs[14] = self.x[30] as u32;
        regs[15] = self.pc as u32;
        self.last_synced_regs = *regs;
    }

    #[inline]
    fn cond_holds(&self, cond: u32) -> bool {
        let n = (self.nzcv >> 31) & 1 != 0;
        let z = (self.nzcv >> 30) & 1 != 0;
        let c = (self.nzcv >> 29) & 1 != 0;
        let v = (self.nzcv >> 28) & 1 != 0;
        let base = match (cond >> 1) & 7 {
            0 => z,
            1 => c,
            2 => n,
            3 => v,
            4 => c && !z,
            5 => n == v,
            6 => (n == v) && !z,
            _ => true,
        };
        if (cond & 1) != 0 && (cond & 0xe) != 0xe {
            !base
        } else {
            base
        }
    }

    #[inline]
    fn set_nzcv(&mut self, n: bool, z: bool, c: bool, v: bool) {
        self.nzcv = ((n as u32) << 31)
            | ((z as u32) << 30)
            | ((c as u32) << 29)
            | ((v as u32) << 28);
    }

    pub fn run_or_step(
        &mut self,
        mem: &mut Mem,
        mut ticks: Option<&mut u64>,
        regs32: &mut [u32; 16],
    ) -> CpuState {
        self.sync_from_regs32(regs32);

        let max_steps: u64 = match ticks.as_deref() {
            Some(t) => (*t).max(1),
            None => 1,
        };
        let mut steps_done: u64 = 0;

        let state = loop {
            if steps_done >= max_steps {
                break CpuState::Normal;
            }
            let pc32 = self.pc as u32;
            if pc32 < mem.null_segment_size() || (pc32 & 3) != 0 {
                self.last_exception_pc = pc32;
                break CpuState::Error(CpuError::MemoryError);
            }
            let Some(instr_bytes) = mem.get_bytes_fallible(Ptr::from_bits(pc32), 4) else {
                self.last_exception_pc = pc32;
                break CpuState::Error(CpuError::MemoryError);
            };
            let insn = u32::from_le_bytes([
                instr_bytes[0],
                instr_bytes[1],
                instr_bytes[2],
                instr_bytes[3],
            ]);
            steps_done += 1;

            // Check touchHLE's 4 special A32 trampoline opcodes first:
            // 1. encode_a32_svc(svc): 0xef00_0000 | svc
            if (insn & 0xff00_0000) == 0xef00_0000 {
                let svc = insn & 0x00ff_ffff;
                self.pc = self.pc.wrapping_add(4);
                break CpuState::Svc(svc);
            }
            // 2. encode_a32_ret(): 0xe12f_ff1e (bx lr)
            if insn == 0xe12f_ff1e {
                self.pc = self.x[30] & !3;
                continue;
            }
            // 3. mov r0, #0 in alloc_a32_ret_stub: 0xe3a0_0000
            if insn == 0xe3a0_0000 {
                self.x[0] = 0;
                self.pc = self.pc.wrapping_add(4);
                continue;
            }
            // 4. encode_a32_trap(): 0xe7ff_defe
            if insn == 0xe7ff_defe {
                self.last_exception_pc = pc32;
                self.pc = self.pc.wrapping_add(4);
                break CpuState::Error(CpuError::Breakpoint);
            }

            match self.step_insn(mem, insn, pc32) {
                StepOutcome::Continue => {}
                StepOutcome::Svc(svc) => break CpuState::Svc(svc),
                StepOutcome::Error(err) => {
                    self.last_exception_pc = pc32;
                    break CpuState::Error(err);
                }
            }
        };

        if let Some(ref mut t) = ticks {
            **t = (**t).saturating_sub(steps_done);
        }
        self.sync_to_regs32(regs32);
        state
    }

    fn step_insn(&mut self, mem: &mut Mem, insn: u32, pc32: u32) -> StepOutcome {
        let op0 = (insn >> 25) & 0xf;
        match op0 {
            // 1000, 1001: Data Processing -- Immediate
            0b1000 | 0b1001 => self.exec_dp_imm(insn),
            // 1010, 1011: Branches, Exception Generating and System instructions
            0b1010 | 0b1011 => self.exec_branch_sys(insn),
            // 0100, 0110, 1100, 1110: Loads and Stores
            0b0100 | 0b0110 | 0b1100 | 0b1110 => self.exec_load_store(mem, insn, pc32),
            // 0101, 1101: Data Processing -- Register
            0b0101 | 0b1101 => self.exec_dp_reg(insn),
            // 0111, 1111: Data Processing -- Scalar Floating-Point and Advanced SIMD
            0b0111 | 0b1111 => self.exec_fp_simd(insn),
            _ => {
                // Treat unallocated/unknown encoding as a no-op advance
                self.pc = self.pc.wrapping_add(4);
                StepOutcome::Continue
            }
        }
    }

    fn exec_dp_imm(&mut self, insn: u32) -> StepOutcome {
        let sf = (insn >> 31) & 1 != 0;
        let op = (insn >> 23) & 0x7;
        let rd = (insn & 0x1f) as usize;

        match op {
            // PC-rel. addressing (ADR / ADRP)
            0b000 | 0b001 => {
                let is_adrp = (insn >> 31) & 1 != 0;
                let immlo = ((insn >> 29) & 0x3) as i64;
                let immhi = (((insn >> 5) & 0x7ffff) as i32) << 13 >> 13;
                let imm = ((immhi as i64) << 2) | immlo;
                let val = if is_adrp {
                    (self.pc & !0xfff).wrapping_add((imm << 12) as u64)
                } else {
                    self.pc.wrapping_add(imm as u64)
                };
                self.write_x(rd, val);
            }
            // Add/subtract (immediate)
            0b010 | 0b011 => {
                let is_sub = (insn >> 30) & 1 != 0;
                let set_flags = (insn >> 29) & 1 != 0;
                let sh = (insn >> 22) & 1 != 0;
                let imm12 = ((insn >> 10) & 0xfff) as u64;
                let rn = ((insn >> 5) & 0x1f) as usize;
                let imm = if sh { imm12 << 12 } else { imm12 };
                let a = self.read_x_or_sp(rn);
                if sf {
                    let (res, c, v) = if is_sub {
                        add_with_carry_64(a, !imm, true)
                    } else {
                        add_with_carry_64(a, imm, false)
                    };
                    if set_flags {
                        self.set_nzcv((res as i64) < 0, res == 0, c, v);
                        self.write_x(rd, res);
                    } else {
                        self.write_x_or_sp(rd, res);
                    }
                } else {
                    let (res, c, v) = if is_sub {
                        add_with_carry_32(a as u32, !(imm as u32), true)
                    } else {
                        add_with_carry_32(a as u32, imm as u32, false)
                    };
                    if set_flags {
                        self.set_nzcv((res as i32) < 0, res == 0, c, v);
                        self.write_w(rd, res);
                    } else {
                        self.write_w_or_sp(rd, res);
                    }
                }
            }
            // Logical (immediate)
            0b100 => {
                let opc = (insn >> 29) & 0x3;
                let n = (insn >> 22) & 1;
                let immr = (insn >> 16) & 0x3f;
                let imms = (insn >> 10) & 0x3f;
                let rn = ((insn >> 5) & 0x1f) as usize;
                let (wmask, _) = decode_bit_masks(n, imms, immr, true, if sf { 64 } else { 32 });
                let a = self.read_x(rn);
                let res = match opc {
                    0b00 => a & wmask,
                    0b01 => a | wmask,
                    0b10 => a ^ wmask,
                    _ => a & wmask,
                };
                if sf {
                    if opc == 0b11 {
                        self.set_nzcv((res as i64) < 0, res == 0, false, false);
                        self.write_x(rd, res);
                    } else {
                        self.write_x_or_sp(rd, res);
                    }
                } else {
                    let res32 = res as u32;
                    if opc == 0b11 {
                        self.set_nzcv((res32 as i32) < 0, res32 == 0, false, false);
                        self.write_w(rd, res32);
                    } else {
                        self.write_w_or_sp(rd, res32);
                    }
                }
            }
            // Move wide (immediate): MOVN / MOVZ / MOVK
            0b101 => {
                let opc = (insn >> 29) & 0x3;
                let hw = ((insn >> 21) & 0x3) * 16;
                let imm16 = ((insn >> 5) & 0xffff) as u64;
                let shifted = imm16 << hw;
                let res = match opc {
                    0b00 => !shifted, // MOVN
                    0b10 => shifted,  // MOVZ
                    0b11 => {
                        // MOVK
                        let old = self.read_x(rd);
                        (old & !(0xffffu64 << hw)) | shifted
                    }
                    _ => shifted,
                };
                if sf {
                    self.write_x(rd, res);
                } else {
                    self.write_w(rd, res as u32);
                }
            }
            // Bitfield (SBFM / BFM / UBFM)
            0b110 => {
                let opc = (insn >> 29) & 0x3;
                let n = (insn >> 22) & 1;
                let immr = (insn >> 16) & 0x3f;
                let imms = (insn >> 10) & 0x3f;
                let rn = ((insn >> 5) & 0x1f) as usize;
                let bits = if sf { 64 } else { 32 };
                let src = if sf {
                    self.read_x(rn)
                } else {
                    self.read_x(rn) & 0xffff_ffff
                };
                let dst = if sf {
                    self.read_x(rd)
                } else {
                    self.read_x(rd) & 0xffff_ffff
                };
                let (wmask, tmask) = decode_bit_masks(n, imms, immr, false, bits);
                let ror_src = if sf {
                    src.rotate_right(immr)
                } else {
                    ((src as u32).rotate_right(immr)) as u64
                };
                let res = match opc {
                    0b00 => {
                        // SBFM
                        let bot = ror_src & wmask;
                        let sign_bit = (src >> imms) & 1;
                        let top = if sign_bit != 0 { !0u64 } else { 0u64 };
                        (top & !tmask) | (bot & tmask)
                    }
                    0b01 => {
                        // BFM
                        let bot = (dst & !wmask) | (ror_src & wmask);
                        (dst & !tmask) | (bot & tmask)
                    }
                    _ => {
                        // UBFM
                        (ror_src & wmask) & tmask
                    }
                };
                if sf {
                    self.write_x(rd, res);
                } else {
                    self.write_w(rd, res as u32);
                }
            }
            // Extract (EXTR / ROR immediate)
            0b111 => {
                let rm = ((insn >> 16) & 0x1f) as usize;
                let imms = (insn >> 10) & 0x3f;
                let rn = ((insn >> 5) & 0x1f) as usize;
                let n_val = self.read_x(rn);
                let m_val = self.read_x(rm);
                if sf {
                    let res = if imms == 0 {
                        m_val
                    } else {
                        (m_val >> imms) | (n_val << (64 - imms))
                    };
                    self.write_x(rd, res);
                } else {
                    let n32 = n_val as u32;
                    let m32 = m_val as u32;
                    let s = imms & 31;
                    let res = if s == 0 {
                        m32
                    } else {
                        (m32 >> s) | (n32 << (32 - s))
                    };
                    self.write_w(rd, res);
                }
            }
            _ => {}
        }
        self.pc = self.pc.wrapping_add(4);
        StepOutcome::Continue
    }

    fn exec_branch_sys(&mut self, insn: u32) -> StepOutcome {
        let top = (insn >> 26) & 0x3f;
        match top {
            // Unconditional branch (immediate): B
            0b000101 => {
                let imm26 = ((insn & 0x03ff_ffff) as i32) << 6 >> 6;
                self.pc = self.pc.wrapping_add(((imm26 as i64) * 4) as u64);
                return StepOutcome::Continue;
            }
            // Branch with link (immediate): BL
            0b100101 => {
                let imm26 = ((insn & 0x03ff_ffff) as i32) << 6 >> 6;
                self.x[30] = self.pc.wrapping_add(4);
                self.pc = self.pc.wrapping_add(((imm26 as i64) * 4) as u64);
                return StepOutcome::Continue;
            }
            // Compare and branch (immediate): CBZ / CBNZ
            0b011010 | 0b111010 => {
                let sf = (insn >> 31) & 1 != 0;
                let is_cbnz = (insn >> 24) & 1 != 0;
                let imm19 = (((insn >> 5) & 0x7ffff) as i32) << 13 >> 13;
                let rt = (insn & 0x1f) as usize;
                let val = if sf {
                    self.read_x(rt)
                } else {
                    self.read_x(rt) & 0xffff_ffff
                };
                let take = if is_cbnz { val != 0 } else { val == 0 };
                if take {
                    self.pc = self.pc.wrapping_add(((imm19 as i64) * 4) as u64);
                } else {
                    self.pc = self.pc.wrapping_add(4);
                }
                return StepOutcome::Continue;
            }
            // Test and branch (immediate): TBZ / TBNZ
            0b011011 | 0b111011 => {
                let b5 = (insn >> 31) & 1;
                let is_tbnz = (insn >> 24) & 1 != 0;
                let b40 = (insn >> 19) & 0x1f;
                let bit_pos = (b5 << 5) | b40;
                let imm14 = (((insn >> 5) & 0x3fff) as i32) << 18 >> 18;
                let rt = (insn & 0x1f) as usize;
                let bit = (self.read_x(rt) >> bit_pos) & 1;
                let take = if is_tbnz { bit != 0 } else { bit == 0 };
                if take {
                    self.pc = self.pc.wrapping_add(((imm14 as i64) * 4) as u64);
                } else {
                    self.pc = self.pc.wrapping_add(4);
                }
                return StepOutcome::Continue;
            }
            // Conditional branch (immediate): B.cond
            0b010101 => {
                let cond = insn & 0xf;
                let imm19 = (((insn >> 5) & 0x7ffff) as i32) << 13 >> 13;
                if self.cond_holds(cond) {
                    self.pc = self.pc.wrapping_add(((imm19 as i64) * 4) as u64);
                } else {
                    self.pc = self.pc.wrapping_add(4);
                }
                return StepOutcome::Continue;
            }
            // Exception generation / System / Unconditional branch (register)
            0b110101 => {
                let sub = (insn >> 22) & 0xf;
                if sub == 0b0000 {
                    // SVC / BRK / HLT
                    let opc = (insn >> 21) & 0x7;
                    let imm16 = (insn >> 5) & 0xffff;
                    let ll = insn & 0x3;
                    if opc == 0 && ll == 1 {
                        // SVC #imm16
                        self.pc = self.pc.wrapping_add(4);
                        return StepOutcome::Svc(imm16);
                    }
                    if opc == 1 && ll == 0 {
                        // BRK #imm16
                        self.pc = self.pc.wrapping_add(4);
                        return StepOutcome::Error(CpuError::Breakpoint);
                    }
                    self.pc = self.pc.wrapping_add(4);
                    return StepOutcome::Continue;
                }
                if (insn & 0xffff_f000) == 0xd503_2000 || (insn & 0xffff_f000) == 0xd503_3000 {
                    // Hints, NOP, barriers (DMB, DSB, ISB, CLREX)
                    self.pc = self.pc.wrapping_add(4);
                    return StepOutcome::Continue;
                }
                if (insn >> 20) == 0xd53 {
                    // MRS Xt, sysreg
                    let rt = (insn & 0x1f) as usize;
                    let sysreg = (insn >> 5) & 0x7fff;
                    let val = match sysreg {
                        // TPIDR_EL0 (0b11_011_1101_0000_010 = 0x5e82)
                        0x5e82 => self.tpidr_el0,
                        // TPIDRRO_EL0 (0b11_011_1101_0000_011 = 0x5e83)
                        0x5e83 => self.tpidrro_el0,
                        // NZCV (0b11_011_0100_0010_000 = 0x5a10)
                        0x5a10 => self.nzcv as u64,
                        // FPCR (0b11_011_0100_0100_000 = 0x5a20)
                        0x5a20 => self.fpcr as u64,
                        // FPSR (0b11_011_0100_0100_001 = 0x5a21)
                        0x5a21 => self.fpsr as u64,
                        _ => 0,
                    };
                    self.write_x(rt, val);
                    self.pc = self.pc.wrapping_add(4);
                    return StepOutcome::Continue;
                }
                if (insn >> 20) == 0xd51 {
                    // MSR sysreg, Xt
                    let rt = (insn & 0x1f) as usize;
                    let sysreg = (insn >> 5) & 0x7fff;
                    let val = self.read_x(rt);
                    match sysreg {
                        0x5e82 => self.tpidr_el0 = val,
                        0x5e83 => self.tpidrro_el0 = val,
                        0x5a10 => self.nzcv = (val as u32) & 0xf000_0000,
                        0x5a20 => self.fpcr = val as u32,
                        0x5a21 => self.fpsr = val as u32,
                        _ => {}
                    }
                    self.pc = self.pc.wrapping_add(4);
                    return StepOutcome::Continue;
                }
                if (insn >> 25) == 0b1101011 {
                    // Unconditional branch (register): BR / BLR / RET (including PAC variants)
                    let opc = (insn >> 21) & 0xf;
                    let rn = ((insn >> 5) & 0x1f) as usize;
                    match opc {
                        0b0000 | 0b1000 => {
                            // BR / BRAA
                            let target = self.read_x(rn) & !3;
                            self.pc = target;
                            return StepOutcome::Continue;
                        }
                        0b0001 | 0b1001 => {
                            // BLR / BLRAA
                            let target = self.read_x(rn) & !3;
                            self.x[30] = self.pc.wrapping_add(4);
                            self.pc = target;
                            return StepOutcome::Continue;
                        }
                        0b0010 | 0b0100 => {
                            // RET / RETAA / RETAB
                            let reg = if (opc & 0b0100) != 0 { 30 } else { rn };
                            let target = self.read_x(reg) & !3;
                            self.pc = target;
                            return StepOutcome::Continue;
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        self.pc = self.pc.wrapping_add(4);
        StepOutcome::Continue
    }

    fn exec_dp_reg(&mut self, insn: u32) -> StepOutcome {
        let sf = (insn >> 31) & 1 != 0;
        let op0 = (insn >> 30) & 1;
        let op1 = (insn >> 28) & 1;
        let op2 = (insn >> 21) & 0xf;
        let rd = (insn & 0x1f) as usize;
        let rn = ((insn >> 5) & 0x1f) as usize;
        let rm = ((insn >> 16) & 0x1f) as usize;

        if op1 == 0 {
            if (op2 & 0b1000) == 0 {
                // Logical (shifted register)
                let opc = (insn >> 29) & 0x3;
                let shift_type = (insn >> 22) & 0x3;
                let n_bit = (insn >> 21) & 1 != 0;
                let imm6 = (insn >> 10) & 0x3f;
                let a = self.read_x(rn);
                let mut b = shift_reg(self.read_x(rm), shift_type, imm6, sf);
                if n_bit {
                    b = !b;
                }
                let res = match opc {
                    0b00 => a & b,
                    0b01 => a | b,
                    0b10 => a ^ b,
                    _ => a & b,
                };
                if sf {
                    if opc == 0b11 {
                        self.set_nzcv((res as i64) < 0, res == 0, false, false);
                    }
                    self.write_x(rd, res);
                } else {
                    let res32 = res as u32;
                    if opc == 0b11 {
                        self.set_nzcv((res32 as i32) < 0, res32 == 0, false, false);
                    }
                    self.write_w(rd, res32);
                }
            } else if (op2 & 0b1001) == 0b1000 {
                // Add/subtract (shifted register)
                let is_sub = op0 != 0;
                let set_flags = (insn >> 29) & 1 != 0;
                let shift_type = (insn >> 22) & 0x3;
                let imm6 = (insn >> 10) & 0x3f;
                let a = self.read_x(rn);
                let b = shift_reg(self.read_x(rm), shift_type, imm6, sf);
                if sf {
                    let (res, c, v) = if is_sub {
                        add_with_carry_64(a, !b, true)
                    } else {
                        add_with_carry_64(a, b, false)
                    };
                    if set_flags {
                        self.set_nzcv((res as i64) < 0, res == 0, c, v);
                    }
                    self.write_x(rd, res);
                } else {
                    let (res, c, v) = if is_sub {
                        add_with_carry_32(a as u32, !(b as u32), true)
                    } else {
                        add_with_carry_32(a as u32, b as u32, false)
                    };
                    if set_flags {
                        self.set_nzcv((res as i32) < 0, res == 0, c, v);
                    }
                    self.write_w(rd, res);
                }
            } else {
                // Add/subtract (extended register)
                let is_sub = op0 != 0;
                let set_flags = (insn >> 29) & 1 != 0;
                let opt = (insn >> 13) & 0x7;
                let imm3 = (insn >> 10) & 0x7;
                let a = self.read_x_or_sp(rn);
                let b = extend_reg(self.read_x(rm), opt, imm3);
                if sf {
                    let (res, c, v) = if is_sub {
                        add_with_carry_64(a, !b, true)
                    } else {
                        add_with_carry_64(a, b, false)
                    };
                    if set_flags {
                        self.set_nzcv((res as i64) < 0, res == 0, c, v);
                        self.write_x(rd, res);
                    } else {
                        self.write_x_or_sp(rd, res);
                    }
                } else {
                    let (res, c, v) = if is_sub {
                        add_with_carry_32(a as u32, !(b as u32), true)
                    } else {
                        add_with_carry_32(a as u32, b as u32, false)
                    };
                    if set_flags {
                        self.set_nzcv((res as i32) < 0, res == 0, c, v);
                        self.write_w(rd, res);
                    } else {
                        self.write_w_or_sp(rd, res);
                    }
                }
            }
        } else {
            // op1 == 1
            match op2 {
                // Add/subtract (with carry): ADC / ADCS / SBC / SBCS
                0b0000 => {
                    let is_sub = op0 != 0;
                    let set_flags = (insn >> 29) & 1 != 0;
                    let carry_in = (self.nzcv >> 29) & 1 != 0;
                    let a = self.read_x(rn);
                    let b = self.read_x(rm);
                    if sf {
                        let (res, c, v) = if is_sub {
                            add_with_carry_64(a, !b, carry_in)
                        } else {
                            add_with_carry_64(a, b, carry_in)
                        };
                        if set_flags {
                            self.set_nzcv((res as i64) < 0, res == 0, c, v);
                        }
                        self.write_x(rd, res);
                    } else {
                        let (res, c, v) = if is_sub {
                            add_with_carry_32(a as u32, !(b as u32), carry_in)
                        } else {
                            add_with_carry_32(a as u32, b as u32, carry_in)
                        };
                        if set_flags {
                            self.set_nzcv((res as i32) < 0, res == 0, c, v);
                        }
                        self.write_w(rd, res);
                    }
                }
                // Conditional compare (register / immediate): CCMP / CCMN
                0b0010 => {
                    let is_sub = op0 != 0;
                    let is_imm = (insn >> 11) & 1 != 0;
                    let cond = (insn >> 12) & 0xf;
                    let nzcv_imm = insn & 0xf;
                    if self.cond_holds(cond) {
                        let a = self.read_x(rn);
                        let b = if is_imm { rm as u64 } else { self.read_x(rm) };
                        if sf {
                            let (res, c, v) = if is_sub {
                                add_with_carry_64(a, !b, true)
                            } else {
                                add_with_carry_64(a, b, false)
                            };
                            self.set_nzcv((res as i64) < 0, res == 0, c, v);
                        } else {
                            let (res, c, v) = if is_sub {
                                add_with_carry_32(a as u32, !(b as u32), true)
                            } else {
                                add_with_carry_32(a as u32, b as u32, false)
                            };
                            self.set_nzcv((res as i32) < 0, res == 0, c, v);
                        }
                    } else {
                        self.nzcv = nzcv_imm << 28;
                    }
                }
                // Conditional select: CSEL / CSINC / CSINV / CSNEG
                0b0100 => {
                    let cond = (insn >> 12) & 0xf;
                    let o2 = (insn >> 10) & 1 != 0;
                    let res = if self.cond_holds(cond) {
                        self.read_x(rn)
                    } else {
                        let mut m = self.read_x(rm);
                        if op0 != 0 {
                            m = !m;
                        }
                        if o2 {
                            m = m.wrapping_add(1);
                        }
                        m
                    };
                    if sf {
                        self.write_x(rd, res);
                    } else {
                        self.write_w(rd, res as u32);
                    }
                }
                // Data-processing (2 source) / Data-processing (1 source)
                0b0110 => {
                    let is_1src = op0 != 0;
                    if !is_1src {
                        let opcode = (insn >> 10) & 0x3f;
                        let a = self.read_x(rn);
                        let b = self.read_x(rm);
                        if sf {
                            let res = match opcode {
                                0b000010 => {
                                    if b == 0 {
                                        0
                                    } else {
                                        a.wrapping_div(b)
                                    }
                                } // UDIV
                                0b000011 => {
                                    if b == 0 {
                                        0
                                    } else {
                                        ((a as i64).wrapping_div(b as i64)) as u64
                                    }
                                } // SDIV
                                0b001000 => a.wrapping_shl((b & 63) as u32), // LSLV
                                0b001001 => a.wrapping_shr((b & 63) as u32), // LSRV
                                0b001010 => ((a as i64).wrapping_shr((b & 63) as u32)) as u64, // ASRV
                                0b001011 => a.rotate_right((b & 63) as u32), // RORV
                                _ => 0,
                            };
                            self.write_x(rd, res);
                        } else {
                            let a32 = a as u32;
                            let b32 = b as u32;
                            let res = match opcode {
                                0b000010 => {
                                    if b32 == 0 {
                                        0
                                    } else {
                                        a32.wrapping_div(b32)
                                    }
                                } // UDIV
                                0b000011 => {
                                    if b32 == 0 {
                                        0
                                    } else {
                                        ((a32 as i32).wrapping_div(b32 as i32)) as u32
                                    }
                                } // SDIV
                                0b001000 => a32.wrapping_shl(b32 & 31), // LSLV
                                0b001001 => a32.wrapping_shr(b32 & 31), // LSRV
                                0b001010 => ((a32 as i32).wrapping_shr(b32 & 31)) as u32, // ASRV
                                0b001011 => a32.rotate_right(b32 & 31), // RORV
                                _ => 0,
                            };
                            self.write_w(rd, res);
                        }
                    } else {
                        let opcode = (insn >> 10) & 0x3f;
                        let a = self.read_x(rn);
                        if sf {
                            let res = match opcode {
                                0b000000 => a.reverse_bits(),
                                0b000001 => {
                                    ((a & 0xff00_ff00_ff00_ff00) >> 8)
                                        | ((a & 0x00ff_00ff_00ff_00ff) << 8)
                                }
                                0b000010 => (a >> 32).swap_bytes() << 32 | (a as u32).swap_bytes() as u64,
                                0b000011 => a.swap_bytes(),
                                0b000100 => a.leading_zeros() as u64,
                                0b000101 => {
                                    let s = a as i64;
                                    (if s < 0 { (!a).leading_zeros() } else { a.leading_zeros() })
                                        .saturating_sub(1) as u64
                                }
                                _ => a,
                            };
                            self.write_x(rd, res);
                        } else {
                            let a32 = a as u32;
                            let res = match opcode {
                                0b000000 => a32.reverse_bits(),
                                0b000001 => ((a32 & 0xff00_ff00) >> 8) | ((a32 & 0x00ff_00ff) << 8),
                                0b000010 => a32.swap_bytes(),
                                0b000100 => a32.leading_zeros(),
                                0b000101 => {
                                    let s = a32 as i32;
                                    (if s < 0 {
                                        (!a32).leading_zeros()
                                    } else {
                                        a32.leading_zeros()
                                    })
                                    .saturating_sub(1)
                                }
                                _ => a32,
                            };
                            self.write_w(rd, res);
                        }
                    }
                }
                // Data-processing (3 source): MADD / MSUB / SMADDL / SMSUBL / SMULH / UMADDL / UMSUBL / UMULH
                0b1000..=0b1111 => {
                    let o0 = (insn >> 15) & 1 != 0;
                    let ra = ((insn >> 10) & 0x1f) as usize;
                    let op31 = (insn >> 21) & 0x7;
                    let a = self.read_x(ra);
                    let n = self.read_x(rn);
                    let m = self.read_x(rm);
                    match op31 {
                        0b000 => {
                            // MADD / MSUB
                            if sf {
                                let prod = n.wrapping_mul(m);
                                let res = if o0 {
                                    a.wrapping_sub(prod)
                                } else {
                                    a.wrapping_add(prod)
                                };
                                self.write_x(rd, res);
                            } else {
                                let prod = (n as u32).wrapping_mul(m as u32);
                                let res = if o0 {
                                    (a as u32).wrapping_sub(prod)
                                } else {
                                    (a as u32).wrapping_add(prod)
                                };
                                self.write_w(rd, res);
                            }
                        }
                        0b001 => {
                            // SMADDL / SMSUBL
                            let prod = ((n as i32 as i64).wrapping_mul(m as i32 as i64)) as u64;
                            let res = if o0 {
                                a.wrapping_sub(prod)
                            } else {
                                a.wrapping_add(prod)
                            };
                            self.write_x(rd, res);
                        }
                        0b010 => {
                            // SMULH
                            let prod = (n as i64 as i128).wrapping_mul(m as i64 as i128);
                            self.write_x(rd, (prod >> 64) as u64);
                        }
                        0b101 => {
                            // UMADDL / UMSUBL
                            let prod = (n as u32 as u64).wrapping_mul(m as u32 as u64);
                            let res = if o0 {
                                a.wrapping_sub(prod)
                            } else {
                                a.wrapping_add(prod)
                            };
                            self.write_x(rd, res);
                        }
                        0b110 => {
                            // UMULH
                            let prod = (n as u128).wrapping_mul(m as u128);
                            self.write_x(rd, (prod >> 64) as u64);
                        }
                        _ => {}
                    }
                }
                _ => {}
            }
        }
        self.pc = self.pc.wrapping_add(4);
        StepOutcome::Continue
    }

    fn exec_load_store(&mut self, mem: &mut Mem, insn: u32, pc32: u32) -> StepOutcome {
        let op0 = (insn >> 28) & 0xf;
        let is_vector = ((insn >> 26) & 1) != 0;
        let op2 = (insn >> 23) & 0x3;

        // 1. Load literal (PC-relative): op0 = x001, op2 = 00..11
        if (op0 & 0b0011) == 0b0001 && ((insn >> 24) & 0x3) == 0b00 {
            let opc = (insn >> 30) & 0x3;
            let imm19 = (((insn >> 5) & 0x7ffff) as i32) << 13 >> 13;
            let rt = (insn & 0x1f) as usize;
            let addr = (self.pc.wrapping_add(((imm19 as i64) * 4) as u64)) as u32;
            if is_vector {
                match opc {
                    0b00 => {
                        let val: u32 = mem.read(ConstPtr::from_bits(addr));
                        self.v[rt][0] = val as u64;
                        self.v[rt][1] = 0;
                    }
                    0b01 => {
                        let val: u64 = mem.read(ConstPtr::from_bits(addr));
                        self.v[rt][0] = val;
                        self.v[rt][1] = 0;
                    }
                    0b10 => {
                        let lo: u64 = mem.read(ConstPtr::from_bits(addr));
                        let hi: u64 = mem.read(ConstPtr::from_bits(addr.wrapping_add(8)));
                        self.v[rt][0] = lo;
                        self.v[rt][1] = hi;
                    }
                    _ => {}
                }
            } else {
                match opc {
                    0b00 => {
                        let val: u32 = mem.read(ConstPtr::from_bits(addr));
                        self.write_w(rt, val);
                    }
                    0b01 => {
                        let val: u64 = mem.read(ConstPtr::from_bits(addr));
                        self.write_x(rt, val);
                    }
                    0b10 => {
                        let val: i32 = mem.read(ConstPtr::from_bits(addr));
                        self.write_x(rt, val as i64 as u64);
                    }
                    _ => {}
                }
            }
            self.pc = self.pc.wrapping_add(4);
            return StepOutcome::Continue;
        }

        // 2. Load/Store Pair (LDP / STP): bits [29:27] == 101
        if ((insn >> 27) & 0x7) == 0b101 {
            let opc = (insn >> 30) & 0x3;
            let l_bit = (insn >> 22) & 1 != 0;
            let imm7 = (((insn >> 15) & 0x7f) as i32) << 25 >> 25;
            let rt2 = ((insn >> 10) & 0x1f) as usize;
            let rn = ((insn >> 5) & 0x1f) as usize;
            let rt = (insn & 0x1f) as usize;

            let base = self.read_x_or_sp(rn);
            let scale = if is_vector {
                2 + opc
            } else {
                2 + (opc >> 1)
            };
            let offset = (imm7 as i64) << scale;
            let (addr, wb_addr) = match op2 {
                0b01 => (base, Some(base.wrapping_add(offset as u64))), // Post-index
                0b11 => {
                    let a = base.wrapping_add(offset as u64);
                    (a, Some(a))
                } // Pre-index
                _ => (base.wrapping_add(offset as u64), None), // Signed offset / No-alloc
            };
            let addr32 = addr as u32;
            let step = 1u32 << scale;

            if is_vector {
                if l_bit {
                    self.load_vec_elem(mem, rt, addr32, scale);
                    self.load_vec_elem(mem, rt2, addr32.wrapping_add(step), scale);
                } else {
                    self.store_vec_elem(mem, rt, addr32, scale);
                    self.store_vec_elem(mem, rt2, addr32.wrapping_add(step), scale);
                }
            } else if l_bit {
                match opc {
                    0b00 => {
                        let v1: u32 = mem.read(ConstPtr::from_bits(addr32));
                        let v2: u32 = mem.read(ConstPtr::from_bits(addr32.wrapping_add(4)));
                        self.write_w(rt, v1);
                        self.write_w(rt2, v2);
                    }
                    0b01 => {
                        let v1: i32 = mem.read(ConstPtr::from_bits(addr32));
                        let v2: i32 = mem.read(ConstPtr::from_bits(addr32.wrapping_add(4)));
                        self.write_x(rt, v1 as i64 as u64);
                        self.write_x(rt2, v2 as i64 as u64);
                    }
                    _ => {
                        let v1: u64 = mem.read(ConstPtr::from_bits(addr32));
                        let v2: u64 = mem.read(ConstPtr::from_bits(addr32.wrapping_add(8)));
                        self.write_x(rt, v1);
                        self.write_x(rt2, v2);
                    }
                }
            } else if opc == 0b00 {
                mem.write(Ptr::from_bits(addr32), self.read_x(rt) as u32);
                mem.write(
                    Ptr::from_bits(addr32.wrapping_add(4)),
                    self.read_x(rt2) as u32,
                );
            } else {
                mem.write(Ptr::from_bits(addr32), self.read_x(rt));
                mem.write(Ptr::from_bits(addr32.wrapping_add(8)), self.read_x(rt2));
            }

            if let Some(wb) = wb_addr {
                self.write_x_or_sp(rn, wb);
            }
            self.pc = self.pc.wrapping_add(4);
            return StepOutcome::Continue;
        }

        // 3. Load/Store Exclusive / Load-Acquire / Store-Release: bits [29:24] == 001000
        if ((insn >> 24) & 0x3f) == 0b001000 {
            let size = (insn >> 30) & 0x3;
            let l_bit = (insn >> 22) & 1 != 0;
            let o0 = (insn >> 15) & 1 != 0;
            let rs = ((insn >> 16) & 0x1f) as usize;
            let rn = ((insn >> 5) & 0x1f) as usize;
            let rt = (insn & 0x1f) as usize;
            let addr32 = self.read_x_or_sp(rn) as u32;
            let is_exclusive = ((insn >> 23) & 1) == 0;
            if l_bit {
                let val = match size {
                    0 => mem.read::<u8, false>(ConstPtr::from_bits(addr32)) as u64,
                    1 => mem.read::<u16, false>(ConstPtr::from_bits(addr32)) as u64,
                    2 => mem.read::<u32, false>(ConstPtr::from_bits(addr32)) as u64,
                    _ => mem.read::<u64, false>(ConstPtr::from_bits(addr32)),
                };
                if size == 3 {
                    self.write_x(rt, val);
                } else {
                    self.write_w(rt, val as u32);
                }
            } else {
                let val = self.read_x(rt);
                match size {
                    0 => mem.write(Ptr::from_bits(addr32), val as u8),
                    1 => mem.write(Ptr::from_bits(addr32), val as u16),
                    2 => mem.write(Ptr::from_bits(addr32), val as u32),
                    _ => mem.write(Ptr::from_bits(addr32), val),
                }
                if is_exclusive && (!o0 || o0) {
                    // STXR / STLXR: write 0 (success) to Ws
                    self.write_w(rs, 0);
                }
            }
            self.pc = self.pc.wrapping_add(4);
            return StepOutcome::Continue;
        }

        // 4. Single register Load/Store: bits [29:27] == 111
        if ((insn >> 27) & 0x7) == 0b111 {
            let size = (insn >> 30) & 0x3;
            let opc = (insn >> 22) & 0x3;
            let rn = ((insn >> 5) & 0x1f) as usize;
            let rt = (insn & 0x1f) as usize;
            let base = self.read_x_or_sp(rn);

            let scale = if is_vector {
                if (opc & 0b10) != 0 && size == 0 {
                    4
                } else {
                    size
                }
            } else {
                size
            };

            let (addr, wb_addr) = if (op2 & 0b10) != 0 {
                // Unsigned immediate offset
                let imm12 = ((insn >> 10) & 0xfff) as u64;
                (base.wrapping_add(imm12 << scale), None)
            } else {
                let mode = (insn >> 10) & 0x3;
                if ((insn >> 21) & 1) != 0 && mode == 0b10 {
                    // Register offset
                    let rm = ((insn >> 16) & 0x1f) as usize;
                    let option = (insn >> 13) & 0x7;
                    let s_bit = (insn >> 12) & 1;
                    let shift = if s_bit != 0 { scale } else { 0 };
                    let off = extend_reg(self.read_x(rm), option, shift);
                    (base.wrapping_add(off), None)
                } else {
                    // Immediate9: unscaled (00), post-index (01), pre-index (11)
                    let imm9 = (((insn >> 12) & 0x1ff) as i32) << 23 >> 23;
                    let off = (imm9 as i64) as u64;
                    match mode {
                        0b01 => (base, Some(base.wrapping_add(off))),
                        0b11 => {
                            let a = base.wrapping_add(off);
                            (a, Some(a))
                        }
                        _ => (base.wrapping_add(off), None),
                    }
                }
            };

            let addr32 = addr as u32;
            if addr32 < mem.null_segment_size() {
                let _ = pc32;
                return StepOutcome::Error(CpuError::MemoryError);
            }

            if is_vector {
                let is_load = (opc & 1) != 0;
                if is_load {
                    self.load_vec_elem(mem, rt, addr32, scale);
                } else {
                    self.store_vec_elem(mem, rt, addr32, scale);
                }
            } else {
                match (size, opc) {
                    // Stores
                    (0, 0) => mem.write(Ptr::from_bits(addr32), self.read_x(rt) as u8),
                    (1, 0) => mem.write(Ptr::from_bits(addr32), self.read_x(rt) as u16),
                    (2, 0) => mem.write(Ptr::from_bits(addr32), self.read_x(rt) as u32),
                    (3, 0) => mem.write(Ptr::from_bits(addr32), self.read_x(rt)),
                    // Unsigned loads
                    (0, 1) => {
                        let v: u8 = mem.read(ConstPtr::from_bits(addr32));
                        self.write_w(rt, v as u32);
                    }
                    (1, 1) => {
                        let v: u16 = mem.read(ConstPtr::from_bits(addr32));
                        self.write_w(rt, v as u32);
                    }
                    (2, 1) => {
                        let v: u32 = mem.read(ConstPtr::from_bits(addr32));
                        self.write_w(rt, v);
                    }
                    (3, 1) => {
                        let v: u64 = mem.read(ConstPtr::from_bits(addr32));
                        self.write_x(rt, v);
                    }
                    // Signed loads to 64-bit X
                    (0, 2) => {
                        let v: i8 = mem.read(ConstPtr::from_bits(addr32));
                        self.write_x(rt, v as i64 as u64);
                    }
                    (1, 2) => {
                        let v: i16 = mem.read(ConstPtr::from_bits(addr32));
                        self.write_x(rt, v as i64 as u64);
                    }
                    (2, 2) => {
                        let v: i32 = mem.read(ConstPtr::from_bits(addr32));
                        self.write_x(rt, v as i64 as u64);
                    }
                    // Signed loads to 32-bit W
                    (0, 3) => {
                        let v: i8 = mem.read(ConstPtr::from_bits(addr32));
                        self.write_w(rt, v as i32 as u32);
                    }
                    (1, 3) => {
                        let v: i16 = mem.read(ConstPtr::from_bits(addr32));
                        self.write_w(rt, v as i32 as u32);
                    }
                    _ => {}
                }
            }

            if let Some(wb) = wb_addr {
                self.write_x_or_sp(rn, wb);
            }
            self.pc = self.pc.wrapping_add(4);
            return StepOutcome::Continue;
        }

        self.pc = self.pc.wrapping_add(4);
        StepOutcome::Continue
    }

    fn load_vec_elem(&mut self, mem: &mut Mem, rt: usize, addr32: u32, scale: u32) {
        match scale {
            0 => {
                let v: u8 = mem.read(ConstPtr::from_bits(addr32));
                self.v[rt][0] = v as u64;
                self.v[rt][1] = 0;
            }
            1 => {
                let v: u16 = mem.read(ConstPtr::from_bits(addr32));
                self.v[rt][0] = v as u64;
                self.v[rt][1] = 0;
            }
            2 => {
                let v: u32 = mem.read(ConstPtr::from_bits(addr32));
                self.v[rt][0] = v as u64;
                self.v[rt][1] = 0;
            }
            3 => {
                let v: u64 = mem.read(ConstPtr::from_bits(addr32));
                self.v[rt][0] = v;
                self.v[rt][1] = 0;
            }
            _ => {
                let lo: u64 = mem.read(ConstPtr::from_bits(addr32));
                let hi: u64 = mem.read(ConstPtr::from_bits(addr32.wrapping_add(8)));
                self.v[rt][0] = lo;
                self.v[rt][1] = hi;
            }
        }
    }

    fn store_vec_elem(&mut self, mem: &mut Mem, rt: usize, addr32: u32, scale: u32) {
        match scale {
            0 => mem.write(Ptr::from_bits(addr32), self.v[rt][0] as u8),
            1 => mem.write(Ptr::from_bits(addr32), self.v[rt][0] as u16),
            2 => mem.write(Ptr::from_bits(addr32), self.read_s(rt).to_bits()),
            3 => mem.write(Ptr::from_bits(addr32), self.v[rt][0]),
            _ => {
                mem.write(Ptr::from_bits(addr32), self.v[rt][0]);
                mem.write(Ptr::from_bits(addr32.wrapping_add(8)), self.v[rt][1]);
            }
        }
    }

    fn exec_fp_simd(&mut self, insn: u32) -> StepOutcome {
        let rd = (insn & 0x1f) as usize;
        let rn = ((insn >> 5) & 0x1f) as usize;
        let rm = ((insn >> 16) & 0x1f) as usize;

        // Check scalar floating-point instructions: bits [28:24] == 11110
        if ((insn >> 24) & 0x1f) == 0b11110 {
            let ftype = (insn >> 22) & 0x3; // 00 = single (S), 01 = double (D)
            let is_double = ftype == 0b01;

            // 1. Conversion between FP and integer: bit 21 == 1, bits [15:10] == 000000
            if ((insn >> 21) & 1) != 0 && ((insn >> 10) & 0x3f) == 0 {
                let sf = (insn >> 31) & 1 != 0;
                let rmode = (insn >> 19) & 0x3;
                let opcode = (insn >> 16) & 0x7;
                match (rmode, opcode) {
                    // FMOV general <-> FP (00, 110 = FP->int, 111 = int->FP)
                    (0b00, 0b110) => {
                        if sf {
                            self.write_x(rd, self.v[rn][0]);
                        } else {
                            self.write_w(rd, self.v[rn][0] as u32);
                        }
                    }
                    (0b00, 0b111) => {
                        if sf {
                            self.v[rd][0] = self.read_x(rn);
                            self.v[rd][1] = 0;
                        } else {
                            self.v[rd][0] = (self.read_x(rn) as u32) as u64;
                            self.v[rd][1] = 0;
                        }
                    }
                    // SCVTF (int -> FP signed)
                    (0b00, 0b010) => {
                        let iv = if sf {
                            self.read_x(rn) as i64 as f64
                        } else {
                            self.read_x(rn) as i32 as f64
                        };
                        if is_double {
                            self.write_d(rd, iv);
                        } else {
                            self.write_s(rd, iv as f32);
                        }
                    }
                    // UCVTF (int -> FP unsigned)
                    (0b00, 0b011) => {
                        let uv = if sf {
                            self.read_x(rn) as f64
                        } else {
                            (self.read_x(rn) as u32) as f64
                        };
                        if is_double {
                            self.write_d(rd, uv);
                        } else {
                            self.write_s(rd, uv as f32);
                        }
                    }
                    // FCVT*S (FP -> signed int) / FCVT*U (FP -> unsigned int)
                    (_, 0b000 | 0b001 | 0b100 | 0b101) => {
                        let fv = if is_double {
                            self.read_d(rn)
                        } else {
                            self.read_s(rn) as f64
                        };
                        let rounded = match rmode {
                            0b00 => fv.round(),
                            0b01 => fv.ceil(),
                            0b10 => fv.floor(),
                            _ => fv.trunc(),
                        };
                        let is_unsigned = (opcode & 1) != 0;
                        if sf {
                            if is_unsigned {
                                self.write_x(rd, rounded as u64);
                            } else {
                                self.write_x(rd, (rounded as i64) as u64);
                            }
                        } else if is_unsigned {
                            self.write_w(rd, rounded as u32);
                        } else {
                            self.write_w(rd, (rounded as i32) as u32);
                        }
                    }
                    _ => {}
                }
                self.pc = self.pc.wrapping_add(4);
                return StepOutcome::Continue;
            }

            // 2. FP immediate (FMOV Sd/Dd, #imm8): bits [21] == 1, bits [12:10] == 100
            if ((insn >> 21) & 1) != 0 && ((insn >> 10) & 0x7) == 0b100 {
                let imm8 = ((insn >> 13) & 0xff) as u8;
                let f = decode_fp_imm8(imm8);
                if is_double {
                    self.write_d(rd, f);
                } else {
                    self.write_s(rd, f as f32);
                }
                self.pc = self.pc.wrapping_add(4);
                return StepOutcome::Continue;
            }

            // 3. FP compare (FCMP / FCMPE): bits [21] == 1, bits [13:10] == 1000
            if ((insn >> 21) & 1) != 0 && ((insn >> 10) & 0xf) == 0b1000 {
                let cmp_with_zero = (insn & 0x8) != 0;
                let a = if is_double {
                    self.read_d(rn)
                } else {
                    self.read_s(rn) as f64
                };
                let b = if cmp_with_zero {
                    0.0
                } else if is_double {
                    self.read_d(rm)
                } else {
                    self.read_s(rm) as f64
                };
                if a.is_nan() || b.is_nan() {
                    self.set_nzcv(false, false, true, true);
                } else if a == b {
                    self.set_nzcv(false, true, true, false);
                } else if a < b {
                    self.set_nzcv(true, false, false, false);
                } else {
                    self.set_nzcv(false, false, true, false);
                }
                self.pc = self.pc.wrapping_add(4);
                return StepOutcome::Continue;
            }

            // 4. FP conditional select (FCSEL): bits [21] == 1, bits [11:10] == 11
            if ((insn >> 21) & 1) != 0 && ((insn >> 10) & 0x3) == 0b11 {
                let cond = (insn >> 12) & 0xf;
                let src = if self.cond_holds(cond) { rn } else { rm };
                if is_double {
                    self.write_d(rd, self.read_d(src));
                } else {
                    self.write_s(rd, self.read_s(src));
                }
                self.pc = self.pc.wrapping_add(4);
                return StepOutcome::Continue;
            }

            // 5. FP data-processing (1 source): bits [21] == 1, bits [14:10] == 10000
            if ((insn >> 21) & 1) != 0 && ((insn >> 10) & 0x1f) == 0b10000 {
                let opcode = (insn >> 15) & 0x3f;
                let a = if is_double {
                    self.read_d(rn)
                } else {
                    self.read_s(rn) as f64
                };
                match opcode {
                    0b000000 => {
                        if is_double {
                            self.write_d(rd, a);
                        } else {
                            self.write_s(rd, a as f32);
                        }
                    } // FMOV
                    0b000001 => {
                        if is_double {
                            self.write_d(rd, a.abs());
                        } else {
                            self.write_s(rd, (a as f32).abs());
                        }
                    } // FABS
                    0b000010 => {
                        if is_double {
                            self.write_d(rd, -a);
                        } else {
                            self.write_s(rd, -(a as f32));
                        }
                    } // FNEG
                    0b000011 => {
                        if is_double {
                            self.write_d(rd, a.sqrt());
                        } else {
                            self.write_s(rd, (a as f32).sqrt());
                        }
                    } // FSQRT
                    0b000100 => self.write_s(rd, a as f32), // FCVT D -> S
                    0b000101 => self.write_d(rd, a),        // FCVT S -> D
                    0b001000 => {
                        if is_double {
                            self.write_d(rd, a.round());
                        } else {
                            self.write_s(rd, (a as f32).round());
                        }
                    }
                    0b001001 => {
                        if is_double {
                            self.write_d(rd, a.ceil());
                        } else {
                            self.write_s(rd, (a as f32).ceil());
                        }
                    }
                    0b001010 => {
                        if is_double {
                            self.write_d(rd, a.floor());
                        } else {
                            self.write_s(rd, (a as f32).floor());
                        }
                    }
                    0b001011..=0b001111 => {
                        if is_double {
                            self.write_d(rd, a.trunc());
                        } else {
                            self.write_s(rd, (a as f32).trunc());
                        }
                    }
                    _ => {}
                }
                self.pc = self.pc.wrapping_add(4);
                return StepOutcome::Continue;
            }

            // 6. FP data-processing (2 source): bits [21] == 1, bits [11:10] == 10
            if ((insn >> 21) & 1) != 0 && ((insn >> 10) & 0x3) == 0b10 {
                let opcode = (insn >> 12) & 0xf;
                if is_double {
                    let a = self.read_d(rn);
                    let b = self.read_d(rm);
                    let res = match opcode {
                        0b0000 => a * b,
                        0b0001 => a / b,
                        0b0010 => a + b,
                        0b0011 => a - b,
                        0b0100 | 0b0110 => a.max(b),
                        0b0101 | 0b0111 => a.min(b),
                        0b1000 => -(a * b),
                        _ => a,
                    };
                    self.write_d(rd, res);
                } else {
                    let a = self.read_s(rn);
                    let b = self.read_s(rm);
                    let res = match opcode {
                        0b0000 => a * b,
                        0b0001 => a / b,
                        0b0010 => a + b,
                        0b0011 => a - b,
                        0b0100 | 0b0110 => a.max(b),
                        0b0101 | 0b0111 => a.min(b),
                        0b1000 => -(a * b),
                        _ => a,
                    };
                    self.write_s(rd, res);
                }
                self.pc = self.pc.wrapping_add(4);
                return StepOutcome::Continue;
            }
        }

        // 7. FP data-processing (3 source): FMADD / FMSUB / FNMADD / FNMSUB (bits [28:24] == 11111)
        if ((insn >> 24) & 0x1f) == 0b11111 {
            let is_double = ((insn >> 22) & 0x3) == 0b01;
            let o1 = (insn >> 21) & 1 != 0;
            let o0 = (insn >> 15) & 1 != 0;
            let ra = ((insn >> 10) & 0x1f) as usize;
            if is_double {
                let n = self.read_d(rn);
                let m = self.read_d(rm);
                let a = self.read_d(ra);
                let prod = if o0 { -(n * m) } else { n * m };
                let res = if o1 { -a + prod } else { a + prod };
                self.write_d(rd, res);
            } else {
                let n = self.read_s(rn);
                let m = self.read_s(rm);
                let a = self.read_s(ra);
                let prod = if o0 { -(n * m) } else { n * m };
                let res = if o1 { -a + prod } else { a + prod };
                self.write_s(rd, res);
            }
            self.pc = self.pc.wrapping_add(4);
            return StepOutcome::Continue;
        }

        // 8. SIMD vector logical (MOVI #0 / ORR / EOR / AND / BIC)
        if ((insn >> 24) & 0xbf) == 0b00001110 {
            let q = (insn >> 30) & 1 != 0;
            let u_bit = (insn >> 29) & 1 != 0;
            let size = (insn >> 22) & 0x3;
            if ((insn >> 10) & 0x3f) == 0b000111 {
                // AND / BIC / ORR / ORN
                let lo = match (u_bit, size) {
                    (false, 0b00) => self.v[rn][0] & self.v[rm][0],
                    (false, 0b01) => self.v[rn][0] & !self.v[rm][0],
                    (false, 0b10) => self.v[rn][0] | self.v[rm][0],
                    (false, 0b11) => self.v[rn][0] | !self.v[rm][0],
                    (true, 0b00) => self.v[rn][0] ^ self.v[rm][0],
                    _ => self.v[rn][0] | self.v[rm][0],
                };
                let hi = if q {
                    match (u_bit, size) {
                        (false, 0b00) => self.v[rn][1] & self.v[rm][1],
                        (false, 0b01) => self.v[rn][1] & !self.v[rm][1],
                        (false, 0b10) => self.v[rn][1] | self.v[rm][1],
                        (false, 0b11) => self.v[rn][1] | !self.v[rm][1],
                        (true, 0b00) => self.v[rn][1] ^ self.v[rm][1],
                        _ => self.v[rn][1] | self.v[rm][1],
                    }
                } else {
                    0
                };
                self.v[rd][0] = lo;
                self.v[rd][1] = hi;
            }
        } else if ((insn >> 24) & 0xbf) == 0b00001111 {
            // MOVI immediate (commonly `movi v0.2d, #0`)
            let abc = (insn >> 16) & 0x7;
            let defgh = (insn >> 5) & 0x1f;
            if abc == 0 && defgh == 0 {
                self.v[rd][0] = 0;
                self.v[rd][1] = 0;
            }
        }

        self.pc = self.pc.wrapping_add(4);
        StepOutcome::Continue
    }
}

enum StepOutcome {
    Continue,
    Svc(u32),
    Error(CpuError),
}

#[inline]
fn add_with_carry_64(a: u64, b: u64, carry_in: bool) -> (u64, bool, bool) {
    let (r1, c1) = a.overflowing_add(b);
    let (res, c2) = r1.overflowing_add(carry_in as u64);
    let c = c1 || c2;
    let v = (((!(a ^ b)) & (a ^ res)) >> 63) != 0;
    (res, c, v)
}

#[inline]
fn add_with_carry_32(a: u32, b: u32, carry_in: bool) -> (u32, bool, bool) {
    let (r1, c1) = a.overflowing_add(b);
    let (res, c2) = r1.overflowing_add(carry_in as u32);
    let c = c1 || c2;
    let v = (((!(a ^ b)) & (a ^ res)) >> 31) != 0;
    (res, c, v)
}

#[inline]
fn shift_reg(val: u64, shift_type: u32, amount: u32, sf: bool) -> u64 {
    if sf {
        let amt = amount & 63;
        match shift_type {
            0b00 => val.wrapping_shl(amt),
            0b01 => val.wrapping_shr(amt),
            0b10 => ((val as i64).wrapping_shr(amt)) as u64,
            _ => val.rotate_right(amt),
        }
    } else {
        let v32 = val as u32;
        let amt = amount & 31;
        let r32 = match shift_type {
            0b00 => v32.wrapping_shl(amt),
            0b01 => v32.wrapping_shr(amt),
            0b10 => ((v32 as i32).wrapping_shr(amt)) as u32,
            _ => v32.rotate_right(amt),
        };
        r32 as u64
    }
}

#[inline]
fn extend_reg(val: u64, option: u32, shift: u32) -> u64 {
    let extended: u64 = match option & 0x7 {
        0b000 => (val as u8) as u64,
        0b001 => (val as u16) as u64,
        0b010 => (val as u32) as u64,
        0b011 => val,
        0b100 => (val as i8 as i64) as u64,
        0b101 => (val as i16 as i64) as u64,
        0b110 => (val as i32 as i64) as u64,
        _ => val,
    };
    extended.wrapping_shl(shift & 7)
}

fn decode_bit_masks(
    imm_n: u32,
    imms: u32,
    immr: u32,
    immediate: bool,
    reg_bits: u32,
) -> (u64, u64) {
    let combined = (imm_n << 6) | (!imms & 0x3f);
    if combined == 0 {
        return (!0u64, !0u64);
    }
    let len = 31 - combined.leading_zeros();
    let esize = 1u32 << len;
    let levels = esize - 1;
    let s = imms & levels;
    let r = immr & levels;
    if immediate && s == levels {
        return (!0u64, !0u64);
    }
    let diff = s.wrapping_sub(r) & levels;
    let welem = if s == 63 { !0u64 } else { (1u64 << (s + 1)) - 1 };
    let telem = if diff == 63 {
        !0u64
    } else {
        (1u64 << (diff + 1)) - 1
    };
    let w_rotated = rotate_right_esize(welem, r, esize);
    let wmask = replicate_esize(w_rotated, esize, reg_bits);
    let tmask = replicate_esize(telem, esize, reg_bits);
    (wmask, tmask)
}

fn rotate_right_esize(val: u64, rot: u32, esize: u32) -> u64 {
    let r = rot % esize;
    if r == 0 {
        return val;
    }
    let mask = if esize == 64 {
        !0u64
    } else {
        (1u64 << esize) - 1
    };
    let v = val & mask;
    ((v >> r) | (v << (esize - r))) & mask
}

fn replicate_esize(val: u64, esize: u32, reg_bits: u32) -> u64 {
    let mut out = 0u64;
    let mut shift = 0;
    while shift < reg_bits {
        out |= val << shift;
        shift += esize;
    }
    out
}

fn decode_fp_imm8(imm8: u8) -> f64 {
    let sign = if (imm8 & 0x80) != 0 { -1.0 } else { 1.0 };
    let exp_bits = ((imm8 >> 4) & 0x7) as i32;
    let exp = if (exp_bits & 4) != 0 {
        exp_bits - 4
    } else {
        exp_bits - 3
    };
    let mant = 1.0 + ((imm8 & 0xf) as f64) / 16.0;
    sign * mant * (2.0f64).powi(exp)
}

}

use crate::abi::GuestFunction;
use crate::mem::{ConstPtr, GuestUSize, Mem, MutPtr, Ptr, SafeRead, SafeWrite};

// Import functions from C++
use touchHLE_dynarmic_wrapper::*;

type VAddr = u32;

#[derive(Default)]
pub struct CpuContext {
    dynarmic: touchHLE_DynarmicContext,
    arm64: Option<Box<arm64::Arm64Cpu>>,
}

impl CpuContext {
    pub fn new() -> Self {
        Self::default()
    }
}

impl std::ops::Deref for CpuContext {
    type Target = touchHLE_DynarmicContext;
    fn deref(&self) -> &Self::Target {
        &self.dynarmic
    }
}

impl std::ops::DerefMut for CpuContext {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.dynarmic
    }
}

fn touchHLE_cpu_read_impl<T: SafeRead + Default>(
    mem: *mut touchHLE_Mem,
    addr: VAddr,
    error: *mut bool,
) -> T {
    // If a panic occurs (probably due to a null-pointer access), we can't let
    // it keep unwinding as it will hit non-Rust stack frames (dynarmic).
    // Instead we catch the unwind and then tell the C++ code a problem occurred
    // so it can immediately halt CPU execution and then panic itself, now
    // with only Rust stack frames to worry about and with CPU state information
    // available that's useful for debugging.
    //
    // TODO: Disable this in debug mode? This relies on dynarmic's
    // check_halt_on_memory_access option which surely has a significant
    // performance impact.
    //
    // I'm not sure if this actually is unwind-safe, but considering
    // the emulator will crash anyway, maybe this is okay.
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mem = unsafe { &mut *mem.cast::<Mem>() };
        let ptr: ConstPtr<T> = Ptr::from_bits(addr);
        mem.read(ptr)
    }));
    unsafe {
        error.write(res.is_err());
    }
    res.unwrap_or_default()
}

fn touchHLE_cpu_write_impl<T: SafeWrite>(mem: *mut touchHLE_Mem, addr: VAddr, value: T) -> bool {
    // See comments above about catch_unwind
    let mut null_segment_size = 0;
    let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mem = unsafe { &mut *mem.cast::<Mem>() };
        let ptr: MutPtr<T> = Ptr::from_bits(addr);
        null_segment_size = mem.null_segment_size();
        mem.write(ptr, value)
    }));
    if res.is_err() {
        return true;
    }
    // Diagnostic: report a `true` (memory abort) for the first guest store
    // into the null segment, so the emulator can print the exact faulting PC
    // and stack. Returning false for it, as before, lets the guest write
    // through NULL and carry on with no clue where it happened.
    addr < null_segment_size && crate::mem::null_write_probe_claim()
}

// Export functions for use by C++
#[no_mangle]
extern "C" fn touchHLE_cpu_read_u8(mem: *mut touchHLE_Mem, addr: VAddr, error: *mut bool) -> u8 {
    touchHLE_cpu_read_impl(mem, addr, error)
}
#[no_mangle]
extern "C" fn touchHLE_cpu_read_u16(mem: *mut touchHLE_Mem, addr: VAddr, error: *mut bool) -> u16 {
    touchHLE_cpu_read_impl(mem, addr, error)
}
#[no_mangle]
extern "C" fn touchHLE_cpu_read_u32(mem: *mut touchHLE_Mem, addr: VAddr, error: *mut bool) -> u32 {
    touchHLE_cpu_read_impl(mem, addr, error)
}
#[no_mangle]
extern "C" fn touchHLE_cpu_read_u64(mem: *mut touchHLE_Mem, addr: VAddr, error: *mut bool) -> u64 {
    touchHLE_cpu_read_impl(mem, addr, error)
}
#[no_mangle]
extern "C" fn touchHLE_cpu_write_u8(mem: *mut touchHLE_Mem, addr: VAddr, value: u8) -> bool {
    touchHLE_cpu_write_impl(mem, addr, value)
}
#[no_mangle]
extern "C" fn touchHLE_cpu_write_u16(mem: *mut touchHLE_Mem, addr: VAddr, value: u16) -> bool {
    touchHLE_cpu_write_impl(mem, addr, value)
}
#[no_mangle]
extern "C" fn touchHLE_cpu_write_u32(mem: *mut touchHLE_Mem, addr: VAddr, value: u32) -> bool {
    touchHLE_cpu_write_impl(mem, addr, value)
}
#[no_mangle]
extern "C" fn touchHLE_cpu_write_u64(mem: *mut touchHLE_Mem, addr: VAddr, value: u64) -> bool {
    touchHLE_cpu_write_impl(mem, addr, value)
}

pub struct Cpu {
    dynarmic_wrapper: *mut touchHLE_DynarmicWrapper,
    /// Copy of the direct memory access pointer used to check it has not
    /// changed. If this is null, direct memory access is not in use.
    direct_memory_access_ptr: *const std::ffi::c_void,
    /// Present when running a 64-bit (`CPU_TYPE_ARM64`) guest binary.
    arm64: Option<Box<arm64::Arm64Cpu>>,
}

impl Drop for Cpu {
    fn drop(&mut self) {
        unsafe { touchHLE_DynarmicWrapper_delete(self.dynarmic_wrapper) }
    }
}

/// Why CPU execution ended.
#[derive(Debug)]
pub enum CpuState {
    /// Execution halted due to using up all remaining ticks (normal execution)
    /// or after the single instruction was executed (step execution).
    Normal,
    /// SVC instruction encountered.
    Svc(u32),
    /// An error was encountered.
    Error(CpuError),
}

/// A reason that can cause CPU execution to be interrupted.
#[derive(Debug, Clone, PartialEq)]
pub enum CpuError {
    /// Memory error during execution (probably a null page access).
    MemoryError,
    /// Undefined instruction (perhaps from a GDB software breakpoint).
    UndefinedInstruction,
    /// Breakpoint (`bkpt` instruction).
    Breakpoint,
}

impl Cpu {
    /// The register number of the stack pointer.
    pub const SP: usize = 13;
    /// The register number of the link register.
    #[allow(unused)]
    pub const LR: usize = 14;
    /// The register number of the program counter.
    pub const PC: usize = 15;

    /// When this bit is set in CPSR, the CPU is in Thumb mode.
    pub const CPSR_THUMB: u32 = 0x00000020;

    /// When this bit is set in CPSR, the CPU is in user mode.
    pub const CPSR_USER_MODE: u32 = 0x00000010;

    /// Construct a new CPU instance. If a mutable reference to a [Mem] instance
    /// is provided, direct memory access is enabled, and the CPU instance
    /// becomes bound to that [Mem] instance (subsequent calls must use the same
    /// one).
    pub fn new(direct_memory_access: Option<&mut Mem>) -> Cpu {
        // Null page count is in pages rather than bytes. Mem ensures it is
        // page aligned.
        let null_page_count: usize = direct_memory_access
            .as_ref()
            .map_or(0, |mem| mem.null_segment_size() / 0x1000)
            .try_into()
            .unwrap();
        // Safety: the direct memory access pointer will be retained directly by
        // the dynarmic wrapper and indirectly by cached JIT code, so we must
        // ensure we only execute the CPU while holding a &mut on the Mem object
        // to which that pointer belongs.
        let direct_memory_access_ptr = direct_memory_access
            .map_or(std::ptr::null_mut(), |mem| unsafe {
                mem.direct_memory_access_ptr()
            });
        let dynarmic_wrapper =
            unsafe { touchHLE_DynarmicWrapper_new(direct_memory_access_ptr, null_page_count) };
        Cpu {
            dynarmic_wrapper,
            direct_memory_access_ptr,
            arm64: None,
        }
    }

    pub fn enable_arm64(&mut self) {
        if self.arm64.is_none() {
            self.arm64 = Some(Box::new(arm64::Arm64Cpu::new()));
        }
    }

    pub fn is_arm64(&self) -> bool {
        self.arm64.is_some()
    }

    pub fn arm64(&self) -> Option<&arm64::Arm64Cpu> {
        self.arm64.as_deref()
    }

    pub fn arm64_mut(&mut self) -> Option<&mut arm64::Arm64Cpu> {
        self.arm64.as_deref_mut()
    }

    pub fn regs(&self) -> &[u32; 16] {
        unsafe {
            let ptr = touchHLE_DynarmicWrapper_regs_const(self.dynarmic_wrapper);
            &*(ptr as *const [u32; 16])
        }
    }
    pub fn regs_mut(&mut self) -> &mut [u32; 16] {
        unsafe {
            let ptr = touchHLE_DynarmicWrapper_regs_mut(self.dynarmic_wrapper);
            &mut *(ptr as *mut [u32; 16])
        }
    }

    /// Dump the registers of the current cpu to the log output.
    /// Silently ignores panics.
    pub fn dump_regs(&self) {
        let regs = self.regs();
        Self::echo_regs(regs);
    }

    pub fn echo_regs(regs: &[u32; 16]) {
        // Silently ignore panics so it's safe to use in contexts where we
        // can't panic.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for row in 0..4 {
                use std::fmt::Write;
                let mut line = String::new();
                for col in 0..4 {
                    let reg_idx = row * 4 + col;
                    match reg_idx {
                        Self::SP => write!(&mut line, "\t SP: "),
                        Self::LR => write!(&mut line, "\t LR: "),
                        Self::PC => write!(&mut line, "\t PC: "),
                        _ if reg_idx <= 9 => write!(&mut line, "\t R{reg_idx}: "),
                        _ => write!(&mut line, "\tR{reg_idx}: "),
                    }
                    .unwrap();
                    write!(&mut line, "{:#010x}", regs[reg_idx]).unwrap();
                }
                echo!("{}", line);
            }
        }));
    }

    pub fn cpsr(&self) -> u32 {
        unsafe { touchHLE_DynarmicWrapper_cpsr(self.dynarmic_wrapper) }
    }
    pub fn set_cpsr(&mut self, cpsr: u32) {
        unsafe { touchHLE_DynarmicWrapper_set_cpsr(self.dynarmic_wrapper, cpsr) }
    }

    /// Start address of the instruction that raised the most recent
    /// [CpuError::UndefinedInstruction] or [CpuError::Breakpoint].
    ///
    /// When dynarmic raises one of these exceptions it leaves the PC register
    /// pointing at the *next* instruction, and because Thumb instructions can
    /// be either 2 or 4 bytes long, the faulting address can't be recovered
    /// from the register file alone. Dynarmic does pass the exact address to
    /// the exception callback though, so the wrapper records it.
    ///
    /// Only meaningful immediately after [Self::run_or_step] returned one of
    /// the errors above; the value is stale at any other time.
    pub fn last_exception_pc(&self) -> VAddr {
        if let Some(arm64) = self.arm64.as_ref() {
            return arm64.last_exception_pc;
        }
        unsafe { touchHLE_DynarmicWrapper_last_exception_pc(self.dynarmic_wrapper) }
    }

    /// Swap the current state of the CPU (registers etc) with the state stored
    /// in the context object.
    pub fn swap_context(&mut self, context: &mut CpuContext) {
        unsafe {
            touchHLE_DynarmicWrapper_swap_context(self.dynarmic_wrapper, &mut context.dynarmic)
        }
        if self.arm64.is_some() || context.arm64.is_some() {
            if context.arm64.is_none() {
                context.arm64 = Some(Box::new(arm64::Arm64Cpu::new()));
            }
            std::mem::swap(&mut self.arm64, &mut context.arm64);
        }
    }

    /// Get PC with the Thumb bit appropriately set.
    pub fn pc_with_thumb_bit(&self) -> GuestFunction {
        let pc = self.regs()[Self::PC];
        if self.arm64.is_some() {
            return GuestFunction::from_addr_and_thumb_flag(pc & !1, false);
        }
        let thumb = (self.cpsr() & Self::CPSR_THUMB) == Self::CPSR_THUMB;
        GuestFunction::from_addr_and_thumb_flag(pc, thumb)
    }

    /// Set PC and the Thumb flag for executing a guest function. Note that this
    /// does not touch LR.
    pub fn branch(&mut self, new_pc: GuestFunction) {
        if self.arm64.is_some() {
            let addr = new_pc.addr_without_thumb_bit();
            self.regs_mut()[Self::PC] = addr;
            if let Some(arm64) = self.arm64.as_mut() {
                arm64.pc = addr as u64;
            }
            return;
        }
        self.regs_mut()[Self::PC] = new_pc.addr_without_thumb_bit();
        let cpsr_without_thumb = self.cpsr() & (!Self::CPSR_THUMB);
        self.set_cpsr(cpsr_without_thumb | ((new_pc.is_thumb() as u32) * Self::CPSR_THUMB))
    }

    /// Set the PC and Thumb flag (like [Self::branch]), but also set the LR,
    /// and return the original PC and LR.
    pub fn branch_with_link(
        &mut self,
        new_pc: GuestFunction,
        new_lr: GuestFunction,
    ) -> (GuestFunction, GuestFunction) {
        let old_pc = self.pc_with_thumb_bit();
        let old_lr = GuestFunction::from_addr_with_thumb_bit(self.regs()[Self::LR]);
        self.branch(new_pc);
        self.regs_mut()[Self::LR] = new_lr.addr_with_thumb_bit();
        if let Some(arm64) = self.arm64.as_mut() {
            arm64.x[30] = new_lr.addr_without_thumb_bit() as u64;
        }
        (old_pc, old_lr)
    }

    /// Clear dynarmic's instruction cache for some range of addresses.
    /// This is of interest to the dynamic linker, which will sometimes rewrite
    /// code.
    pub fn invalidate_cache_range(&mut self, base: VAddr, size: GuestUSize) {
        if self.arm64.is_some() {
            return;
        }
        unsafe {
            touchHLE_DynarmicWrapper_invalidate_cache_range(self.dynarmic_wrapper, base, size)
        }
    }

    /// Start CPU execution.
    ///
    /// If `ticks` is [Some], it is used as an abstract time limit. The value
    /// will be reduced proportionately with the amount of ticks expended.
    ///
    /// If `ticks` is [None], the CPU executes only a single instruction. This
    /// is also known as "stepping".
    ///
    /// This will return either because the CPU ran out of time, or because
    /// something else happened which requires attention from the host.
    #[must_use]
    pub fn run_or_step(&mut self, mem: &mut Mem, ticks: Option<&mut u64>) -> CpuState {
        // See ::new() for why this is done.
        if !self.direct_memory_access_ptr.is_null() {
            assert!(self.direct_memory_access_ptr == unsafe { mem.direct_memory_access_ptr() });
        }

        if self.arm64.is_some() {
            let regs_mut = unsafe {
                let ptr = touchHLE_DynarmicWrapper_regs_mut(self.dynarmic_wrapper);
                &mut *(ptr as *mut [u32; 16])
            };
            return self
                .arm64
                .as_mut()
                .unwrap()
                .run_or_step(mem, ticks, regs_mut);
        }

        let res = unsafe {
            touchHLE_DynarmicWrapper_run_or_step(
                self.dynarmic_wrapper,
                mem as *mut Mem as *mut touchHLE_Mem,
                ticks,
            )
        };
        match res {
            -1 => CpuState::Normal,
            -2 => CpuState::Error(CpuError::MemoryError),
            -3 => CpuState::Error(CpuError::UndefinedInstruction),
            -4 => CpuState::Error(CpuError::Breakpoint),
            _ if res < -4 => panic!("Unexpected CPU execution result"),
            svc => CpuState::Svc(svc as u32),
        }
    }
}
