#!/usr/bin/env python3
"""Pad a Mach-O dylib's string table to an 8-byte boundary.

The string table follows the indirect symbol table, whose entries are four
bytes each. When a dylib has an odd number of indirect symbols the linker
leaves the string pool 4-byte aligned and does not pad it, and dyld on iOS 26
and later refuses to load the result:

    mis-aligned LINKEDIT string pool, fileOffset=0x...

Whether it happens is pure luck of the symbol count, so it cannot be left to
chance: this inserts the missing padding, moves the string table, and fixes the
offsets that describe it. The dylib must be signed again afterwards.
"""

import shutil
import struct
import subprocess
import sys

MH_MAGIC_64 = 0xFEEDFACF
LC_SYMTAB = 0x02
LC_DYSYMTAB = 0x0B
LC_SEGMENT_64 = 0x19
LC_CODE_SIGNATURE = 0x1D
LC_FUNCTION_STARTS = 0x26
LC_DATA_IN_CODE = 0x29
LC_DYLD_INFO = 0x22
LC_DYLD_INFO_ONLY = 0x80000022
LC_DYLD_EXPORTS_TRIE = 0x80000033
LC_DYLD_CHAINED_FIXUPS = 0x80000034

# Load commands whose payload lives in __LINKEDIT and is described by a
# (dataoff, datasize) pair at a fixed place in the command.
LINKEDIT_DATA_COMMANDS = {
    LC_FUNCTION_STARTS: "function starts",
    LC_DATA_IN_CODE: "data in code",
    LC_DYLD_EXPORTS_TRIE: "exports trie",
    LC_DYLD_CHAINED_FIXUPS: "chained fixups",
}


class Unsupported(Exception):
    pass


def load_commands(data):
    magic, _, _, _, ncmds, _, _, _ = struct.unpack_from("<8I", data, 0)
    if magic != MH_MAGIC_64:
        raise Unsupported("not a 64-bit little-endian Mach-O")
    offset = 32
    for _ in range(ncmds):
        cmd, cmdsize = struct.unpack_from("<2I", data, offset)
        yield cmd, cmdsize, offset
        offset += cmdsize


def fix(path):
    data = bytearray(open(path, "rb").read())

    symtab = dysymtab = linkedit = signature = None
    trailing = []
    for cmd, cmdsize, offset in load_commands(data):
        if cmd == LC_SYMTAB:
            symtab = offset
        elif cmd == LC_DYSYMTAB:
            dysymtab = offset
        elif cmd == LC_CODE_SIGNATURE:
            signature = offset
        elif cmd == LC_SEGMENT_64:
            name = bytes(data[offset + 8:offset + 24]).rstrip(b"\0")
            if name == b"__LINKEDIT":
                linkedit = offset
        elif cmd in LINKEDIT_DATA_COMMANDS:
            dataoff, datasize = struct.unpack_from("<2I", data, offset + 8)
            trailing.append((LINKEDIT_DATA_COMMANDS[cmd], dataoff, datasize))
        elif cmd in (LC_DYLD_INFO, LC_DYLD_INFO_ONLY):
            for index, field in enumerate(
                ("rebase", "bind", "weak bind", "lazy bind", "export")
            ):
                dataoff, datasize = struct.unpack_from(
                    "<2I", data, offset + 8 + index * 8
                )
                trailing.append((field, dataoff, datasize))

    if symtab is None or linkedit is None:
        raise Unsupported("no LC_SYMTAB or __LINKEDIT segment")

    stroff, strsize = struct.unpack_from("<2I", data, symtab + 16)
    padding = -stroff % 8
    if padding == 0:
        return False

    # Everything except the code signature has to sit before the string table,
    # otherwise moving the table would move data described by offsets this
    # script does not know how to update.
    if dysymtab is not None:
        indirectsymoff, nindirectsyms = struct.unpack_from("<2I", data, dysymtab + 56)
        trailing.append(("indirect symbols", indirectsymoff, nindirectsyms * 4))
    symoff, nsyms = struct.unpack_from("<2I", data, symtab + 8)
    trailing.append(("symbol table", symoff, nsyms * 16))
    for name, dataoff, datasize in trailing:
        if datasize and dataoff + datasize > stroff:
            raise Unsupported(f"{name} lies after the string table")

    string_table_end = stroff + strsize
    if signature is not None:
        signature_off, signature_size = struct.unpack_from("<2I", data, signature + 8)
        if signature_off < string_table_end:
            raise Unsupported("code signature lies before the string table")
    elif len(data) > string_table_end:
        raise Unsupported("unknown data after the string table")

    data[stroff:stroff] = b"\0" * padding

    struct.pack_into("<I", data, symtab + 16, stroff + padding)
    if signature is not None:
        struct.pack_into("<I", data, signature + 8, signature_off + padding)

    fileoff, filesize = struct.unpack_from("<2Q", data, linkedit + 40)
    vmsize, = struct.unpack_from("<Q", data, linkedit + 32)
    struct.pack_into("<Q", data, linkedit + 48, filesize + padding)
    page = 0x4000
    struct.pack_into("<Q", data, linkedit + 32, -(-(filesize + padding) // page) * page)

    backup = path + ".unaligned"
    shutil.copyfile(path, backup)
    with open(path, "wb") as output:
        output.write(data)

    # The signature now covers stale contents, and an invalid signature is
    # worse than none. Ad-hoc sign; a real identity is applied when the dylib
    # is embedded in the app.
    subprocess.run(
        ["codesign", "--force", "--sign", "-", path],
        check=True,
        capture_output=True,
    )
    return True


def main(paths):
    status = 0
    for path in paths:
        try:
            if fix(path):
                print(f"Aligned the string pool in {path}")
        except Unsupported as error:
            print(f"error: cannot align {path}: {error}", file=sys.stderr)
            status = 1
    return status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
