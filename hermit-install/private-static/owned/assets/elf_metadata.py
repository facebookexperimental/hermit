"""Existing bounded metadata helpers; no standalone historical CLI."""
import argparse, hashlib, importlib.util, json, os, re, resource, struct, subprocess, sys, time


from pathlib import Path


LOW, HIGH = 0x6000e1000000, 0x6000ff000000


def need(ok, why):
    if not ok:
        raise RuntimeError(why)


def elf_headers(path):
    """Read actual ELF headers; this is not the runtime's opcode/entry qualifier."""
    length = path.stat().st_size
    with path.open('rb') as f:
        h = f.read(64)
        need(h[:7] == b'\x7fELF\x02\x01\x01', 'not ELF64 little endian')
        need(struct.unpack_from('<HHI', h, 16) == (2, 62, 1), 'not x86-64 ET_EXEC')
        entry, phoff = struct.unpack_from('<QQ', h, 24)
        entsize, count = struct.unpack_from('<HH', h, 54)
        need(entsize == 56 and 0 < count < 128 and phoff + count * 56 <= length, 'bad PHDR table')
        f.seek(phoff)
        segments = []
        for index in range(count):
            kind, flags, offset, vaddr, paddr, filesz, memsz, align = struct.unpack('<IIQQQQQQ', f.read(56))
            need(offset + filesz <= length and filesz <= memsz, 'segment outside actual file/memory')
            need(kind not in (2, 3), 'dynamic/interpreter dependency in static inspection image')
            if kind == 1:
                need(LOW <= vaddr and vaddr + memsz <= HIGH and memsz > 0, 'PT_LOAD outside fixed runtime window')
                need(not (flags & 1 and flags & 2), 'writable executable PT_LOAD')
                need(align == 4096 and vaddr % align == offset % align, 'unexpected PT_LOAD alignment')
            segments.append(dict(index=index, type=kind, flags=flags, offset=offset, vaddr=vaddr,
                                 filesz=filesz, memsz=memsz, align=align))
        loads = [s for s in segments if s['type'] == 1]
        need(loads and any(s['flags'] & 1 and s['vaddr'] <= entry < s['vaddr'] + s['memsz'] for s in loads), 'entry not executable')
        occupied = sorted((s['vaddr'] // 4096 * 4096, (s['vaddr'] + s['memsz'] + 4095) // 4096 * 4096) for s in loads)
        need(all(a[1] <= b[0] for a, b in zip(occupied, occupied[1:])), 'overlapping load pages')
        tls = [s for s in segments if s['type'] == 7]
        need(len(tls) <= 1, 'multiple PT_TLS')
        return dict(entry=entry, segments=segments, tls=tls)
