/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */
//! Utilities related to the stack.

use crate::cpu::Cpu;
use crate::mem::{GuestUSize, Mem, MutPtr, Ptr};

/// Set up the stack for the main thread, ready to execute the entry-point
/// of the application (aka `start` or `main`).
pub fn prep_stack_for_start(
    mem: &mut Mem,
    cpu: &mut Cpu,
    argv: &[&str],
    envp: &[&str],
    apple: &[&str],
    pass_args_in_registers: bool,
) {
    let argc: i32 = argv.len().try_into().unwrap();
    let is_arm64 = cpu.is_arm64();
    let ptr_size: u32 = if is_arm64 { 8 } else { 4 };

    let stack_base: usize =
        Mem::MAIN_THREAD_STACK_LOW_END as usize + Mem::MAIN_THREAD_STACK_SIZE as usize;

    let mut reversed_data = Vec::<u8>::new();
    let mut string_ptrs = Vec::<u32>::new();

    fn push_string(
        string: &str,
        reversed_data: &mut Vec<u8>,
        string_ptrs: &mut Vec<u32>,
        stack_base: usize,
    ) {
        reversed_data.reserve(string.len() + 1);
        reversed_data.push(b'\0'); // null terminator
        for &c in string.as_bytes().iter().rev() {
            reversed_data.push(c);
        }

        let ptr: u32 = (stack_base - reversed_data.len()).try_into().unwrap();
        string_ptrs.push(ptr);
    }

    string_ptrs.push(argc as u32);
    for arg in argv {
        push_string(arg, &mut reversed_data, &mut string_ptrs, stack_base);
    }
    string_ptrs.push(0); // terminator

    for arg in envp {
        push_string(arg, &mut reversed_data, &mut string_ptrs, stack_base);
    }
    string_ptrs.push(0); // terminator

    for arg in apple {
        push_string(arg, &mut reversed_data, &mut string_ptrs, stack_base);
    }
    string_ptrs.push(0); // terminator

    // Pad so the final stack pointer is 16-byte aligned on arm64 (4-byte on arm32).
    let total_ptr_bytes = string_ptrs.len() * (ptr_size as usize);
    let align = if is_arm64 { 16 } else { 4 };
    let misaligned_by = (reversed_data.len() + total_ptr_bytes) % align;
    let pad_by = if misaligned_by != 0 {
        align - misaligned_by
    } else {
        0
    };
    reversed_data.resize(reversed_data.len() + pad_by, 0);

    for &ptr in string_ptrs.iter().rev() {
        if is_arm64 {
            reversed_data.extend_from_slice((ptr as u64).to_be_bytes().as_slice());
        } else {
            reversed_data.extend_from_slice(ptr.to_be_bytes().as_slice());
        }
    }

    let stack_ptr: MutPtr<u8> =
        Ptr::from_bits((stack_base - reversed_data.len()).try_into().unwrap());
    let stack_height: GuestUSize = reversed_data.len().try_into().unwrap();

    assert!(stack_height < Mem::MAIN_THREAD_STACK_SIZE);

    let stack_region = mem.bytes_at_mut(stack_ptr, stack_height);

    for i in 0..stack_height {
        stack_region[i as usize] = reversed_data[(stack_height - i - 1) as usize];
    }

    assert!(stack_height.is_multiple_of(4));

    let sp = stack_ptr.to_bits();
    cpu.regs_mut()[Cpu::SP] = sp;
    if let Some(arm64) = cpu.arm64_mut() {
        arm64.sp = sp as u64;
    }

    if pass_args_in_registers {
        let argv_ptr = sp + ptr_size; // skip argc
        let envp_ptr = argv_ptr + ptr_size * (argv.len() as u32 + 1); // skip argv + NULL
        let apple_ptr = envp_ptr + ptr_size * (envp.len() as u32 + 1); // skip envp + NULL
        let regs = cpu.regs_mut();
        regs[0] = argc as u32;
        regs[1] = argv_ptr;
        regs[2] = envp_ptr;
        regs[3] = apple_ptr;
        if let Some(arm64) = cpu.arm64_mut() {
            arm64.x[0] = argc as u32 as u64;
            arm64.x[1] = argv_ptr as u64;
            arm64.x[2] = envp_ptr as u64;
            arm64.x[3] = apple_ptr as u64;
        }
    }
}
