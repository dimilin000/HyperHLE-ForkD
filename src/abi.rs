/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Traits for application binary interface (ABI) translation, in particular
//! calling conventions.
//!
//! Useful resources:
//!
//! * Apple's [Writing ARMv6 code for iOS](https://developer.apple.com/documentation/xcode/writing-armv6-code-for-ios), read together with Arm's [Procedure Call Standard for the Arm Architecture (AAPCS32)](https://github.com/ARM-software/abi-aa/blob/main/aapcs32/aapcs32.rst).
//! * Apple's [Writing ARM64 code for Apple Platforms](https://developer.apple.com/documentation/xcode/writing-arm64-code-for-apple-platforms).
//!
//! See also: [crate::mem::SafeRead] and [crate::mem::SafeWrite].

use crate::cpu::Cpu;
use crate::mem::{ConstPtr, ConstVoidPtr, GuestUSize, Mem, MutPtr, MutVoidPtr, Ptr, SafeRead};
use crate::Environment;

/// The register number of the frame pointer in Apple's ABI.
pub const FRAME_POINTER: usize = 7;

/// Register class of an argument or return value when translating to/from
/// 64-bit `AAPCS64` (Apple Darwin ARM64 PCS).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Arm64ArgClass {
    /// Passed in general-purpose registers `X0..X7` (one `X` register per `u32` slot).
    General,
    /// 64-bit integer (`u64` / `i64`), passed in a single `X` register.
    Int64,
    /// Single-precision float (`f32`) or `CGFloat` (which is `f64` on 64-bit iOS),
    /// passed in `V0..V7` (`S0..S7` or `D0..D7`).
    Float32,
    /// Double-precision float (`f64`), passed in `D0..D7`.
    Float64,
    /// Homogeneous floating-point aggregate (`CGPoint`, `CGSize`, `CGRect`,
    /// `CGVector`, `UIEdgeInsets`), passed in consecutive `D` registers.
    FloatAggregate,
}

#[inline]
fn vreg_to_f32(raw: u64) -> f32 {
    if (raw >> 32) == 0 {
        f32::from_bits(raw as u32)
    } else {
        f64::from_bits(raw) as f32
    }
}

/// Address of an A32, T32, or A64 instruction, with the Thumb mode encoded
/// using the Thumb bit. This is also used as an (untyped) guest function pointer.
///
/// It is wrapped in a struct to prevent mixing with other pointers.
#[derive(Copy, Clone, Debug, Default, PartialEq)]
pub struct GuestFunction(ConstVoidPtr);

unsafe impl SafeRead for GuestFunction {}
impl GuestFunction {
    pub const THUMB_BIT: u32 = 0x1;

    pub fn from_addr_with_thumb_bit(addr: u32) -> Self {
        GuestFunction(Ptr::from_bits(addr))
    }

    pub fn from_addr_and_thumb_flag(pc: u32, thumb: bool) -> Self {
        GuestFunction(Ptr::from_bits(pc | ((thumb as u32) * Self::THUMB_BIT)))
    }

    /// Returns `true` if the function uses regular Thumb (T32) instructions,
    /// or `false` if it uses regular Arm (A32/A64) instructions.
    pub fn is_thumb(self) -> bool {
        self.0.to_bits() & Self::THUMB_BIT == Self::THUMB_BIT
    }

    pub fn addr_with_thumb_bit(self) -> u32 {
        self.0.to_bits()
    }

    /// Get the underlying address with the Thumb bit set to 0, i.e. the actual
    /// memory address of the first instruction.
    pub fn addr_without_thumb_bit(self) -> u32 {
        self.0.to_bits() & !Self::THUMB_BIT
    }

    pub fn to_ptr(self) -> ConstVoidPtr {
        self.0
    }

    /// Like [CallFromHost::call_from_host], but doesn't push a new guest stack
    /// frame. This is not a true tail call: PC and LR are still preserved.
    ///
    /// This is only needed in special applications where having a new stack
    /// frame would be troublesome, e.g. a tail call with stack argument
    /// pass-through.
    pub fn call_without_pushing_stack_frame(self, env: &mut Environment) {
        log_dbg!(
            "Begin call to guest function {:?} (no new stack frame)",
            self
        );

        let (old_pc, old_lr) = env
            .cpu
            .branch_with_link(self, env.dyld.return_to_host_routine());

        env.run_call();

        env.cpu.branch(old_pc);
        env.cpu.regs_mut()[Cpu::LR] = old_lr.addr_with_thumb_bit();
        if let Some(arm64) = env.cpu.arm64_mut() {
            arm64.x[30] = old_lr.addr_without_thumb_bit() as u64;
        }

        log_dbg!(
            "End call to guest function {:?} (no stack frame popped)",
            self
        );
    }

    /// A shorthand function for `GuestFunction::from_addr_with_thumb_bit(0)`
    pub fn null_ptr() -> Self {
        GuestFunction(Ptr::null())
    }
}

/// This trait represents a host function that can be called from guest code.
///
/// See also [CallFromHost] and
/// [GuestFunction::call_without_pushing_stack_frame].
pub trait CallFromGuest {
    fn call_from_guest(&self, env: &mut Environment);
}

fn read_next_arg_arm64<T: GuestArg>(
    gp_idx: &mut usize,
    fp_idx: &mut usize,
    stack_off: &mut u32,
    env: &Environment,
) -> T {
    let mut fake_regs = [0u32; 16];
    let sp = env.cpu.regs()[Cpu::SP];
    let Some(arm64) = env.cpu.arm64() else {
        return T::from_regs(&fake_regs[0..T::REG_COUNT]);
    };

    match T::ARM64_CLASS {
        Arm64ArgClass::Float32 => {
            let val = if *fp_idx < 8 {
                let raw = arm64.v[*fp_idx][0];
                *fp_idx += 1;
                vreg_to_f32(raw)
            } else {
                let raw: u64 = env.mem.read(ConstPtr::from_bits(sp + *stack_off));
                *stack_off += 8;
                vreg_to_f32(raw)
            };
            fake_regs[0] = val.to_bits();
        }
        Arm64ArgClass::Float64 => {
            let raw = if *fp_idx < 8 {
                let r = arm64.v[*fp_idx][0];
                *fp_idx += 1;
                r
            } else {
                let r: u64 = env.mem.read(ConstPtr::from_bits(sp + *stack_off));
                *stack_off += 8;
                r
            };
            let bits = if (raw >> 32) == 0 && raw != 0 {
                (f32::from_bits(raw as u32) as f64).to_bits()
            } else {
                raw
            };
            fake_regs[0] = bits as u32;
            fake_regs[1] = (bits >> 32) as u32;
        }
        Arm64ArgClass::FloatAggregate => {
            for i in 0..T::REG_COUNT {
                let val = if *fp_idx < 8 {
                    let raw = arm64.v[*fp_idx][0];
                    *fp_idx += 1;
                    vreg_to_f32(raw)
                } else {
                    let raw: u64 = env.mem.read(ConstPtr::from_bits(sp + *stack_off));
                    *stack_off += 8;
                    vreg_to_f32(raw)
                };
                fake_regs[i] = val.to_bits();
            }
        }
        Arm64ArgClass::Int64 => {
            let val = if *gp_idx < 8 {
                let r = arm64.x[*gp_idx];
                *gp_idx += 1;
                r
            } else {
                let r: u64 = env.mem.read(ConstPtr::from_bits(sp + *stack_off));
                *stack_off += 8;
                r
            };
            fake_regs[0] = val as u32;
            fake_regs[1] = (val >> 32) as u32;
        }
        Arm64ArgClass::General => {
            for i in 0..T::REG_COUNT {
                let val = if *gp_idx < 8 {
                    let r = arm64.x[*gp_idx] as u32;
                    *gp_idx += 1;
                    r
                } else {
                    let r: u64 = env.mem.read(ConstPtr::from_bits(sp + *stack_off));
                    *stack_off += 8;
                    r as u32
                };
                fake_regs[i] = val;
            }
        }
    }

    T::from_regs(&fake_regs[0..T::REG_COUNT])
}

fn write_next_arg_unified<T: GuestArg>(
    reg_offset: &mut usize,
    gp_idx: &mut usize,
    fp_idx: &mut usize,
    arg: T,
    env: &mut Environment,
) {
    let mut fake_regs = [0u32; 16];
    let slice = &mut fake_regs[0..T::REG_COUNT];
    arg.to_regs(slice);

    for &word in slice.iter() {
        if *reg_offset < 4 {
            env.cpu.regs_mut()[*reg_offset] = word;
        } else {
            let sp = env.cpu.regs()[Cpu::SP];
            let stack_ptr: MutPtr<u32> = Ptr::from_bits(sp);
            env.mem
                .write(stack_ptr + (*reg_offset - 4).try_into().unwrap(), word);
        }
        *reg_offset += 1;
    }

    let Some(arm64) = env.cpu.arm64_mut() else {
        return;
    };

    match T::ARM64_CLASS {
        Arm64ArgClass::Float32 => {
            if *fp_idx < 8 {
                let f = f32::from_bits(slice[0]);
                // Store as f64 (`CGFloat`) if non-zero, wait: most ObjC callbacks
                // on 64-bit iOS take `CGFloat` (`double`).
                arm64.v[*fp_idx][0] = (f as f64).to_bits();
                arm64.v[*fp_idx][1] = 0;
                *fp_idx += 1;
            }
        }
        Arm64ArgClass::Float64 => {
            if *fp_idx < 8 {
                let bits = (slice[0] as u64) | ((slice[1] as u64) << 32);
                arm64.v[*fp_idx][0] = bits;
                arm64.v[*fp_idx][1] = 0;
                *fp_idx += 1;
            }
        }
        Arm64ArgClass::FloatAggregate => {
            for &word in slice.iter() {
                if *fp_idx < 8 {
                    let f = f32::from_bits(word);
                    arm64.v[*fp_idx][0] = (f as f64).to_bits();
                    arm64.v[*fp_idx][1] = 0;
                    *fp_idx += 1;
                }
            }
        }
        Arm64ArgClass::Int64 => {
            if *gp_idx < 8 {
                let bits = (slice[0] as u64) | ((slice[1] as u64) << 32);
                arm64.x[*gp_idx] = bits;
                *gp_idx += 1;
            }
        }
        Arm64ArgClass::General => {
            for &word in slice.iter() {
                if *gp_idx < 8 {
                    arm64.x[*gp_idx] = word as u64;
                    *gp_idx += 1;
                }
            }
        }
    }
}

fn write_retval_arm64<R: GuestRet>(retval: R, env: &mut Environment) {
    if let Some(size) = R::SIZE_IN_MEM {
        let scratch_addr = {
            let arm64 = env.cpu.arm64().unwrap();
            arm64.stret_scratch_addr
        };
        let scratch_addr = if scratch_addr == 0 {
            let ptr = env.mem.alloc(128);
            env.cpu.arm64_mut().unwrap().stret_scratch_addr = ptr.to_bits();
            ptr.to_bits()
        } else {
            scratch_addr
        };
        let scratch_ptr: MutVoidPtr = Ptr::from_bits(scratch_addr);
        retval.to_mem(scratch_ptr, &mut env.mem);

        let word0: u32 = env.mem.read(ConstPtr::from_bits(scratch_addr));
        let word1: u32 = if size >= 8 {
            env.mem.read(ConstPtr::from_bits(scratch_addr + 4))
        } else {
            0
        };
        env.cpu.regs_mut()[0] = word0;
        env.cpu.regs_mut()[1] = word1;

        let float_count = ((size / 4) as usize).min(4);
        let mut floats = [0.0f64; 4];
        for (i, slot) in floats.iter_mut().enumerate().take(float_count) {
            let w: u32 = env.mem.read(ConstPtr::from_bits(scratch_addr + (i as u32) * 4));
            *slot = f32::from_bits(w) as f64;
        }

        let x8_addr = env.cpu.arm64().map_or(0, |a| a.x[8] as u32);
        if size > 16
            && x8_addr >= env.mem.null_segment_size()
            && env
                .mem
                .get_bytes_fallible(Ptr::from_bits(x8_addr), size * 2)
                .is_some()
        {
            let count = (size / 4) as usize;
            for i in 0..count {
                let w: u32 = env.mem.read(ConstPtr::from_bits(scratch_addr + (i as u32) * 4));
                let d = f32::from_bits(w) as f64;
                env.mem
                    .write(Ptr::<u64, true>::from_bits(x8_addr + (i as u32) * 8), d.to_bits());
            }
        }

        if let Some(arm64) = env.cpu.arm64_mut() {
            arm64.x[0] = word0 as u64;
            arm64.x[1] = word1 as u64;
            for (i, &d) in floats.iter().enumerate().take(float_count) {
                arm64.v[i][0] = d.to_bits();
                arm64.v[i][1] = 0;
            }
        }
    } else {
        retval.to_regs(env.cpu.regs_mut());
        let r0 = env.cpu.regs()[0];
        let r1 = env.cpu.regs()[1];
        if let Some(arm64) = env.cpu.arm64_mut() {
            match R::ARM64_CLASS {
                Arm64ArgClass::Float32 => {
                    let f = f32::from_bits(r0);
                    arm64.v[0][0] = (f as f64).to_bits();
                    arm64.v[0][1] = 0;
                    arm64.x[0] = r0 as u64;
                }
                Arm64ArgClass::Float64 => {
                    let bits = (r0 as u64) | ((r1 as u64) << 32);
                    arm64.v[0][0] = bits;
                    arm64.v[0][1] = 0;
                    arm64.x[0] = bits;
                }
                Arm64ArgClass::Int64 => {
                    let bits = (r0 as u64) | ((r1 as u64) << 32);
                    arm64.x[0] = bits;
                }
                _ => {
                    arm64.x[0] = r0 as u64;
                }
            }
        }
    }
}

macro_rules! impl_CallFromGuest {
    ( $($p:tt => $P:ident),* ) => {
        impl<R, $($P),*> CallFromGuest for fn(&mut Environment, $($P),*) -> R
            where R: GuestRet, $($P: GuestArg,)* {
            // ignore warnings for the zero-argument case
            #[allow(unused_variables, unused_mut, clippy::unused_unit)]
            fn call_from_guest(&self, env: &mut Environment) {
                if env.cpu.is_arm64() {
                    let mut gp_idx = 0;
                    let mut fp_idx = 0;
                    let mut stack_off = 0;
                    let args: ($($P,)*) = {
                        ($(read_next_arg_arm64::<$P>(&mut gp_idx, &mut fp_idx, &mut stack_off, env),)*)
                    };
                    let retval = self(env, $(args.$p),*);
                    write_retval_arm64(retval, env);
                    return;
                }

                let mut reg_offset = 0;
                let regs = env.cpu.regs();
                let retval_ptr = R::SIZE_IN_MEM.map(|_| {
                    read_next_arg(&mut reg_offset, regs, Ptr::from_bits(regs[Cpu::SP]), &env.mem)
                });
                let args: ($($P,)*) = {
                    ($(read_next_arg::<$P>(&mut reg_offset, regs, Ptr::from_bits(regs[Cpu::SP]), &env.mem),)*)
                };

                log_dbg!("CallFromGuest");

                let retval = self(env, $(args.$p),*);
                log_dbg!("CallFromGuest => {:?}", retval);
                if let Some(retval_ptr) = retval_ptr {
                    retval.to_mem(retval_ptr, &mut env.mem);
                } else {
                    retval.to_regs(env.cpu.regs_mut());
                }
            }
        }
        impl<R, $($P),*> CallFromGuest for fn(&mut Environment, $($P,)* DotDotDot) -> R
            where R: GuestRet, $($P: GuestArg,)* {
            // ignore warnings for the zero-argument case
            #[allow(unused_variables, unused_mut, clippy::unused_unit)]
            fn call_from_guest(&self, env: &mut Environment) {
                if env.cpu.is_arm64() {
                    let mut gp_idx = 0;
                    let mut fp_idx = 0;
                    let mut stack_off = 0;
                    let args: ($($P,)*) = {
                        ($(read_next_arg_arm64::<$P>(&mut gp_idx, &mut fp_idx, &mut stack_off, env),)*)
                    };
                    // On Apple Darwin ARM64 PCS, variadic arguments (`...`) are
                    // always placed on the stack starting at `[SP + stack_off]`.
                    let sp = env.cpu.regs()[Cpu::SP] + stack_off;
                    let va_list = DotDotDot(VaList {
                        reg_offset: 4,
                        stack_pointer: Ptr::from_bits(sp),
                    });
                    let retval = self(env, $(args.$p,)* va_list);
                    write_retval_arm64(retval, env);
                    return;
                }

                let mut reg_offset = 0;
                let regs = env.cpu.regs();
                let retval_ptr = R::SIZE_IN_MEM.map(|_| {
                    read_next_arg(&mut reg_offset, regs, Ptr::from_bits(regs[Cpu::SP]), &env.mem)
                });
                let args: ($($P,)*) = {
                    ($(read_next_arg::<$P>(&mut reg_offset, regs, Ptr::from_bits(regs[Cpu::SP]), &env.mem),)*)
                };
                let va_list = DotDotDot(VaList {
                    reg_offset,
                    stack_pointer: Ptr::from_bits(regs[Cpu::SP])
                });

                log_dbg!("CallFromGuest with va_list: {:?}", va_list);

                let retval = self(env, $(args.$p,)* va_list);
                log_dbg!("CallFromGuest => {:?}", retval);
                if let Some(retval_ptr) = retval_ptr {
                    retval.to_mem(retval_ptr, &mut env.mem);
                } else {
                    retval.to_regs(env.cpu.regs_mut());
                }
            }
        }
    }
}

impl_CallFromGuest!();
impl_CallFromGuest!(0 => P0);
impl_CallFromGuest!(0 => P0, 1 => P1);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10, 11 => P11);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10, 11 => P11, 12 => P12);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10, 11 => P11, 12 => P12, 13 => P13);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10, 11 => P11, 12 => P12, 13 => P13, 14 => P14);
impl_CallFromGuest!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10, 11 => P11, 12 => P12, 13 => P13, 14 => P14, 15 => P15);

/// This trait represents a guest or host function that can be called from host
/// code, but using the guest ABI. See [CallFromGuest], which this is the
/// inverse of.
pub trait CallFromHost<R, P> {
    fn call_from_host(&self, env: &mut Environment, args: P) -> R;
}

macro_rules! impl_CallFromHost {
    ( $($p:tt => $P:ident),* ) => {
        impl <T, R, $($P),*> CallFromHost<R, ($($P,)*)> for T
            where T: CallFromGuest, R: GuestRet, $($P: GuestArg,)* {
            // ignore warnings for the zero-argument case
            #[allow(unused_variables, unused_mut, clippy::unused_unit)]
            fn call_from_host(
                &self,
                env: &mut Environment,
                args: ($($P,)*),
            ) -> R {
                let mut reg_offset = 0;
                let mut gp_idx = 0;
                let mut fp_idx = 0;
                let retval_ptr = R::SIZE_IN_MEM.map(|size| {
                    let aligned_size = (size + 15) & !15;
                    let sp_bits = {
                        let regs = env.cpu.regs_mut();
                        regs[Cpu::SP] -= aligned_size;
                        regs[Cpu::SP]
                    };
                    let ptr: ConstVoidPtr = Ptr::from_bits(sp_bits);
                    env.mem
                        .bytes_at_mut(Ptr::from_bits(sp_bits), aligned_size)
                        .fill(0);
                    write_next_arg(&mut reg_offset, env.cpu.regs_mut(), &mut env.mem, ptr);
                    ptr
                });

                let old_sp = extend_stack_for_args(
                    0 $(+ <$P as GuestArg>::REG_COUNT)*,
                    env.cpu.regs_mut(),
                );

                $(write_next_arg_unified::<$P>(&mut reg_offset, &mut gp_idx, &mut fp_idx, args.$p, env);)*
                self.call_from_guest(env);

                env.cpu.regs_mut()[Cpu::SP] = old_sp;

                if let Some(retval_ptr) = retval_ptr {
                    let aligned_size = (R::SIZE_IN_MEM.unwrap() + 15) & !15;
                    env.cpu.regs_mut()[Cpu::SP] += aligned_size;
                    let scratch_addr = env.cpu.arm64().map_or(0, |a| a.stret_scratch_addr);
                    if scratch_addr != 0 {
                        let size = R::SIZE_IN_MEM.unwrap();
                        let src = env
                            .mem
                            .bytes_at(ConstPtr::from_bits(scratch_addr), size)
                            .to_vec();
                        env.mem
                            .bytes_at_mut(MutPtr::from_bits(retval_ptr.to_bits()), size)
                            .copy_from_slice(&src);
                    }
                    <R as GuestRet>::from_mem(retval_ptr, &env.mem)
                } else {
                    <R as GuestRet>::from_regs(env.cpu.regs())
                }
            }
        }

        impl <R, $($P),*> CallFromHost<R, ($($P,)*)> for GuestFunction
            where R: GuestRet, $($P: GuestArg,)* {
            // ignore warnings for the zero-argument case
            #[allow(unused_variables, unused_mut, clippy::unused_unit)]
            fn call_from_host(
                &self,
                env: &mut Environment,
                args: ($($P,)*),
            ) -> R {
                log_dbg!("Begin call to guest function {:?}", self);

                let (old_pc, old_lr) = env
                    .cpu
                    .branch_with_link(*self, env.dyld.return_to_host_routine());

                // Create a new guest stack frame (16-byte aligned so both AAPCS32
                // and AAPCS64 stack alignment invariants hold).
                let (old_sp, old_fp, host_to_guest_frame_pointer) = {
                    let regs = env.cpu.regs_mut();
                    let old_sp = regs[Cpu::SP];
                    let old_fp = regs[FRAME_POINTER];
                    regs[Cpu::SP] = (regs[Cpu::SP] - 16) & !15;
                    regs[FRAME_POINTER] = regs[Cpu::SP];

                    env.mem
                        .write(Ptr::from_bits(regs[Cpu::SP]), old_fp);
                    env.mem.write(Ptr::from_bits(regs[Cpu::SP] + 4), old_lr);
                    (old_sp, old_fp, regs[FRAME_POINTER])
                };

                assert!(R::SIZE_IN_MEM.is_none()); // pointer return TODO

                let _ = extend_stack_for_args(
                    0 $(+ <$P as GuestArg>::REG_COUNT)*,
                    env.cpu.regs_mut(),
                );

                if env.cpu.is_arm64() {
                    let sp_now = env.cpu.regs()[Cpu::SP];
                    if let Some(arm64) = env.cpu.arm64_mut() {
                        arm64.sp = sp_now as u64;
                    }
                }

                let mut reg_offset = 0;
                let mut gp_idx = 0;
                let mut fp_idx = 0;
                $(write_next_arg_unified::<$P>(&mut reg_offset, &mut gp_idx, &mut fp_idx, args.$p, env);)*

                let host_to_guest_stack_frame =
                    env.push_host_to_guest_stack_frame(host_to_guest_frame_pointer);
                let run_result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    env.run_call();
                }));
                env.pop_host_to_guest_stack_frame(host_to_guest_stack_frame);
                if let Err(error) = run_result {
                    std::panic::resume_unwind(error);
                }

                env.cpu.branch(old_pc);

                let regs = env.cpu.regs_mut();
                log_dbg!("End call to guest function {:?}", self);

                regs[Cpu::LR] = old_lr.addr_with_thumb_bit();
                regs[Cpu::SP] = old_sp;
                regs[FRAME_POINTER] = old_fp;
                if let Some(arm64) = env.cpu.arm64_mut() {
                    arm64.x[30] = old_lr.addr_without_thumb_bit() as u64;
                    arm64.sp = old_sp as u64;
                }
                <R as GuestRet>::from_regs(env.cpu.regs())
            }
        }

    }
}

impl_CallFromHost!();
impl_CallFromHost!(0 => P0);
impl_CallFromHost!(0 => P0, 1 => P1);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10, 11 => P11);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10, 11 => P11, 12 => P12);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10, 11 => P11, 12 => P12, 13 => P13);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10, 11 => P11, 12 => P12, 13 => P13, 14 => P14);
impl_CallFromHost!(0 => P0, 1 => P1, 2 => P2, 3 => P3, 4 => P4, 5 => P5, 6 => P6, 7 => P7, 8 => P8, 9 => P9, 10 => P10, 11 => P11, 12 => P12, 13 => P13, 14 => P14, 15 => P15);

/// Calling convention translation for a function argument type.
pub trait GuestArg: std::fmt::Debug + Sized {
    /// How many registers does this argument type consume?
    const REG_COUNT: usize;

    /// Register class on 64-bit `AAPCS64`.
    const ARM64_CLASS: Arm64ArgClass = Arm64ArgClass::General;

    /// Read the argument from registers. Only `&regs[0..Self::REG_COUNT]` may
    /// be accessed.
    fn from_regs(regs: &[u32]) -> Self;

    /// Write the argument to registers. Only `&mut regs[0..Self::REG_COUNT]`
    /// may be accessed.
    fn to_regs(self, regs: &mut [u32]);
}

/// Read a single argument from registers or the stack. Call this for each
/// argument in order.
fn read_next_arg<T: GuestArg>(
    reg_offset: &mut usize,
    regs: &[u32],
    stack_ptr: ConstPtr<u32>,
    mem: &Mem,
) -> T {
    let mut fake_regs = [0u32; 16];
    let fake_regs = &mut fake_regs[0..T::REG_COUNT];
    for fake_reg in fake_regs.iter_mut() {
        if *reg_offset < 4 {
            *fake_reg = regs[*reg_offset];
        } else {
            *fake_reg = mem.read(stack_ptr + (*reg_offset - 4).try_into().unwrap());
        }
        *reg_offset += 1;
    }

    T::from_regs(fake_regs)
}

/// Decrements the stack pointer to prepare for calling [write_next_arg].
pub fn extend_stack_for_args(reg_count_sum: usize, regs: &mut [u32]) -> u32 {
    let old = regs[Cpu::SP];
    if reg_count_sum > 4 {
        let old: ConstPtr<u32> = Ptr::from_bits(old);
        regs[Cpu::SP] = (old - (reg_count_sum - 4).try_into().unwrap()).to_bits()
    }
    old
}

/// Write a single argument to registers or the stack.
pub fn write_next_arg<T: GuestArg>(
    reg_offset: &mut usize,
    regs: &mut [u32],
    mem: &mut Mem,
    arg: T,
) {
    let mut fake_regs = [0u32; 16];
    let fake_regs = &mut fake_regs[0..T::REG_COUNT];
    arg.to_regs(fake_regs);

    for &mut fake_reg in fake_regs {
        if *reg_offset < 4 {
            regs[*reg_offset] = fake_reg;
        } else {
            let stack_ptr: MutPtr<u32> = Ptr::from_bits(regs[Cpu::SP]);
            mem.write(stack_ptr + (*reg_offset - 4).try_into().unwrap(), fake_reg);
        }
        *reg_offset += 1;
    }
}

/// Represents variable arguments in a [CallFromGuest] function signature,
/// like C `...`, e.g. in the signature of `printf()`. See also [VaList].
#[derive(Debug)]
pub struct DotDotDot(VaList);
impl DotDotDot {
    pub fn start(&self) -> VaList {
        self.0
    }
}

/// Calling convention translation for a variable arguments list (like C
/// `va_list`).
#[derive(Copy, Clone, Debug)]
pub struct VaList {
    reg_offset: usize,
    stack_pointer: ConstVoidPtr,
}
impl VaList {
    /// Get the next argument, like C's `va_arg()`.
    pub fn next<T: GuestArg>(&mut self, env: &mut Environment) -> T {
        if env.cpu.is_arm64() {
            let slot_idx = self.reg_offset.saturating_sub(4) as u32;
            let addr = self.stack_pointer.to_bits() + slot_idx * 8;
            let raw: u64 = env.mem.read(ConstPtr::from_bits(addr));
            self.reg_offset += 1;
            let mut fake_regs = [0u32; 16];
            if T::REG_COUNT >= 2 {
                fake_regs[0] = raw as u32;
                fake_regs[1] = (raw >> 32) as u32;
            } else if T::REG_COUNT == 1 {
                fake_regs[0] = raw as u32;
            }
            return T::from_regs(&fake_regs[0..T::REG_COUNT]);
        }
        let sp_reg = self.stack_pointer.cast();
        read_next_arg(&mut self.reg_offset, env.cpu.regs_mut(), sp_reg, &env.mem)
    }
}

macro_rules! impl_GuestArg_with {
    ($for:ty, $with:ty) => {
        impl GuestArg for $for {
            const REG_COUNT: usize = <$with as GuestArg>::REG_COUNT;
            const ARM64_CLASS: Arm64ArgClass = <$with as GuestArg>::ARM64_CLASS;
            fn from_regs(regs: &[u32]) -> Self {
                <$with as GuestArg>::from_regs(regs) as $for
            }
            fn to_regs(self, regs: &mut [u32]) {
                <$with as GuestArg>::to_regs(self as $with, regs)
            }
        }
    };
}

// GuestArg implementations for u32-like types

impl GuestArg for u32 {
    const REG_COUNT: usize = 1;
    fn from_regs(regs: &[u32]) -> Self {
        regs[0]
    }
    fn to_regs(self, regs: &mut [u32]) {
        regs[0] = self;
    }
}

impl_GuestArg_with!(i32, u32);
impl_GuestArg_with!(u16, u32);
impl_GuestArg_with!(i16, u32);
impl_GuestArg_with!(u8, u32);
impl_GuestArg_with!(i8, u32);
impl_GuestArg_with!(usize, u32);

impl GuestArg for bool {
    const REG_COUNT: usize = 1;
    fn from_regs(regs: &[u32]) -> Self {
        <u32 as GuestArg>::from_regs(regs) != 0
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestArg>::to_regs(self as u32, regs)
    }
}

impl GuestArg for f32 {
    const REG_COUNT: usize = <u32 as GuestArg>::REG_COUNT;
    const ARM64_CLASS: Arm64ArgClass = Arm64ArgClass::Float32;
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u32 as GuestArg>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestArg>::to_regs(self.to_bits(), regs)
    }
}

impl<T, const MUT: bool> GuestArg for Ptr<T, MUT> {
    const REG_COUNT: usize = <u32 as GuestArg>::REG_COUNT;
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u32 as GuestArg>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestArg>::to_regs(self.to_bits(), regs)
    }
}

impl GuestArg for GuestFunction {
    const REG_COUNT: usize = <ConstVoidPtr as GuestArg>::REG_COUNT;
    fn from_regs(regs: &[u32]) -> Self {
        GuestFunction(<ConstVoidPtr as GuestArg>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <ConstVoidPtr as GuestArg>::to_regs(self.0, regs)
    }
}

impl GuestArg for VaList {
    const REG_COUNT: usize = <ConstVoidPtr as GuestArg>::REG_COUNT;
    fn from_regs(regs: &[u32]) -> Self {
        VaList {
            reg_offset: 4,
            stack_pointer: <ConstVoidPtr as GuestArg>::from_regs(regs),
        }
    }
    fn to_regs(self, _regs: &mut [u32]) {
        log!(
            "Warning: VaList::to_regs called; var-args are read from the stack, never written back. Ignoring."
        );
    }
}

// GuestArg implementations for u64-like types

impl GuestArg for u64 {
    const REG_COUNT: usize = 2;
    const ARM64_CLASS: Arm64ArgClass = Arm64ArgClass::Int64;
    fn from_regs(regs: &[u32]) -> Self {
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&regs[0].to_le_bytes());
        bytes[4..8].copy_from_slice(&regs[1].to_le_bytes());
        u64::from_le_bytes(bytes)
    }
    fn to_regs(self, regs: &mut [u32]) {
        let bytes = self.to_le_bytes();
        regs[0] = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        regs[1] = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    }
}

impl_GuestArg_with!(i64, u64);

impl GuestArg for f64 {
    const REG_COUNT: usize = <u64 as GuestArg>::REG_COUNT;
    const ARM64_CLASS: Arm64ArgClass = Arm64ArgClass::Float64;
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u64 as GuestArg>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u64 as GuestArg>::to_regs(self.to_bits(), regs)
    }
}

/// Calling convention translation for a function return type.
pub trait GuestRet: std::fmt::Debug + Sized {
    const SIZE_IN_MEM: Option<GuestUSize> = None;
    const ARM64_CLASS: Arm64ArgClass = Arm64ArgClass::General;

    /// Read the return value from registers.
    fn from_regs(regs: &[u32]) -> Self {
        let _ = regs;
        log!(
            "Warning: GuestRet::from_regs called on a type that did not override it; this is a bug in the ABI implementation. Returning a default-constructed value would be UB; aborting this call."
        );
        panic!("GuestRet::from_regs: default impl invoked")
    }
    /// Write the return value to registers.
    fn to_regs(self, regs: &mut [u32]) {
        let _ = regs;
        log!(
            "Warning: GuestRet::to_regs called on a type that did not override it; this is a bug in the ABI implementation."
        );
    }

    /// Read the return value from memory.
    fn from_mem(ptr: ConstVoidPtr, mem: &Mem) -> Self {
        let _ = (ptr, mem);
        log!(
            "Warning: GuestRet::from_mem called on a type that did not override it; this is a bug in the ABI implementation."
        );
        panic!("GuestRet::from_mem: default impl invoked")
    }
    /// Write the return value to memory.
    fn to_mem(self, ptr: MutVoidPtr, mem: &mut Mem) {
        let _ = (ptr, mem);
        log!(
            "Warning: GuestRet::to_mem called on a type that did not override it; this is a bug in the ABI implementation."
        );
    }
}

macro_rules! impl_GuestRet_with {
    ($for:ty, $with:ty) => {
        impl GuestRet for $for {
            const ARM64_CLASS: Arm64ArgClass = <$with as GuestRet>::ARM64_CLASS;
            fn from_regs(regs: &[u32]) -> Self {
                <$with as GuestRet>::from_regs(regs) as $for
            }
            fn to_regs(self, regs: &mut [u32]) {
                <$with as GuestRet>::to_regs(self as $with, regs)
            }
        }
    };
}

/// Generates a trait implementation of [GuestRet] for a struct type that is
/// larger than 4 bytes.
#[macro_export]
macro_rules! impl_GuestRet_for_large_struct {
    ($for:ty) => {
        impl $crate::abi::GuestRet for $for {
            const SIZE_IN_MEM: Option<$crate::mem::GuestUSize> =
                Some($crate::mem::guest_size_of::<$for>());
            fn from_mem(ptr: $crate::mem::ConstVoidPtr, mem: &$crate::mem::Mem) -> Self {
                let ptr = ptr.cast::<Self>();
                mem.read(ptr)
            }
            fn to_mem(self, ptr: $crate::mem::MutVoidPtr, mem: &mut $crate::mem::Mem) {
                let ptr = ptr.cast::<Self>();
                mem.write(ptr, self)
            }
        }
    };
}
pub use crate::impl_GuestRet_for_large_struct;

impl GuestRet for () {
    fn to_regs(self, _regs: &mut [u32]) {}
    fn from_regs(_regs: &[u32]) -> Self {}
}

// GuestRet implementations for u32-like types

impl GuestRet for u32 {
    fn from_regs(regs: &[u32]) -> Self {
        regs[0]
    }
    fn to_regs(self, regs: &mut [u32]) {
        regs[0] = self;
    }
}

impl_GuestRet_with!(i32, u32);
impl_GuestRet_with!(u16, u32);
impl_GuestRet_with!(i16, u32);
impl_GuestRet_with!(u8, u32);
impl_GuestRet_with!(i8, u32);
impl_GuestRet_with!(usize, u32);

impl GuestRet for bool {
    fn from_regs(regs: &[u32]) -> Self {
        <u32 as GuestRet>::from_regs(regs) != 0
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestRet>::to_regs(self as u32, regs)
    }
}

impl GuestRet for f32 {
    const ARM64_CLASS: Arm64ArgClass = Arm64ArgClass::Float32;
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u32 as GuestRet>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestRet>::to_regs(self.to_bits(), regs)
    }
}

impl<T, const MUT: bool> GuestRet for Ptr<T, MUT> {
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u32 as GuestRet>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u32 as GuestRet>::to_regs(self.to_bits(), regs)
    }
}

// GuestRet implementations for u64-like types

impl GuestRet for u64 {
    const ARM64_CLASS: Arm64ArgClass = Arm64ArgClass::Int64;
    fn from_regs(regs: &[u32]) -> Self {
        let mut bytes = [0u8; 8];
        bytes[0..4].copy_from_slice(&regs[0].to_le_bytes());
        bytes[4..8].copy_from_slice(&regs[1].to_le_bytes());
        u64::from_le_bytes(bytes)
    }
    fn to_regs(self, regs: &mut [u32]) {
        let bytes = self.to_le_bytes();
        regs[0] = u32::from_le_bytes(bytes[0..4].try_into().unwrap());
        regs[1] = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
    }
}

impl_GuestRet_with!(i64, u64);

impl GuestRet for f64 {
    const ARM64_CLASS: Arm64ArgClass = Arm64ArgClass::Float64;
    fn from_regs(regs: &[u32]) -> Self {
        Self::from_bits(<u64 as GuestRet>::from_regs(regs))
    }
    fn to_regs(self, regs: &mut [u32]) {
        <u64 as GuestRet>::to_regs(self.to_bits(), regs)
    }
}
