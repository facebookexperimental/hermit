"""Existing bounded metadata helpers; no standalone historical CLI."""
import argparse


import hashlib


import importlib.util


import json


import re


import os


import resource


import subprocess


import time


import struct


import sys


from pathlib import Path


ROOT = 'detcore_liteinst_exec_initialize'


BASE, END = 0x6000e1000000, 0x6000ff000000


LIMIT = 128 * 1024 * 1024


def need(ok, why):
    if not ok:
        raise RuntimeError(why)


def allocated_inventory(archive, members):
    """Read ar/ELF metadata only. No member extraction or executable loading."""
    length = archive.stat().st_size
    rows, names, objects, symbols = [], [], [], []
    with archive.open('rb') as stream:
        need(stream.read(8) == b'!<arch>\n', 'regular archive required')
        offset, longnames = 8, b''
        while offset < length:
            stream.seek(offset)
            header = stream.read(60)
            need(len(header) == 60 and header[58:] == b'`\n', 'invalid ar header')
            size = int(header[48:58].decode('ascii').strip())
            header_offset = offset
            start, name = offset + 60, header[:16].decode('ascii').rstrip()
            need(size >= 0 and start + size <= length, 'ar member outside archive')
            offset = start + size + size % 2
            if name == '//':
                need(size <= LIMIT, 'oversized ar name table')
                stream.seek(start)
                longnames = stream.read(size)
                continue
            if name in ('/', '/SYM64/'):
                continue
            if name.startswith('/'):
                index = int(name[1:])
                need(0 <= index < len(longnames), 'invalid ar long name')
                end = longnames.find(b'/\n', index)
                need(end >= index, 'unterminated ar long name')
                name = longnames[index:end].decode('utf-8')
            else:
                need(not name.startswith('#1/'), 'BSD extended names require explicit review')
                name = name.removesuffix('/')
            ordinal = len(names)
            names.append(name)
            identity = {'archive': str(archive), 'ordinal': ordinal, 'header_offset': header_offset, 'member': name}
            objects.append(identity)

            def read(relative, count):
                need(relative >= 0 and count >= 0 and relative + count <= size,
                     'ELF metadata outside member: ' + name)
                need(count <= LIMIT, 'oversized ELF metadata request')
                stream.seek(start + relative)
                value = stream.read(count)
                need(len(value) == count, 'short ELF metadata')
                return value

            elf = read(0, 64)
            need(elf[:7] == b'\x7fELF\x02\x01\x01', 'ELF64 little-endian member required')
            need(struct.unpack_from('<HH', elf, 16) == (1, 62), 'x86-64 ET_REL required')
            table = struct.unpack_from('<Q', elf, 40)[0]
            stride, count, strings = struct.unpack_from('<HHH', elf, 58)
            need(stride == 64, 'unexpected ELF section entry size')
            zero = struct.unpack('<IIQQQQIIQQ', read(table, 64))
            if count == 0:
                count = zero[5]
            if strings == 0xffff:
                strings = zero[6]
            need(0 < count <= 1000000 and 0 <= strings < count, 'invalid ELF section count/names')
            sections = [struct.unpack('<IIQQQQIIQQ', read(table + i * stride, 64)) for i in range(count)]
            name_table = sections[strings]
            name_bytes = read(name_table[4], name_table[5])
            # Physical member identity is independent of a repeated basename.
            # Read global/weak/unique records directly from this bound ELF object.
            for table_index, symbol_table in enumerate(sections):
                if symbol_table[1] != 2:  # SHT_SYMTAB
                    continue
                need(symbol_table[9] == 24 and symbol_table[5] % 24 == 0, 'invalid ELF64 symbol table stride/size')
                string_index = symbol_table[6]
                need(0 <= string_index < count and sections[string_index][1] == 3, 'symbol string table is not SHT_STRTAB')
                strings_section = sections[string_index]
                strings_data = read(strings_section[4], strings_section[5])
                sym_data = read(symbol_table[4], symbol_table[5])
                sym_count = len(sym_data) // 24
                extended = [section for section in sections if section[1] == 18 and section[6] == table_index]
                need(len(extended) <= 1, 'ambiguous extended symbol section indices')
                ext_data = None
                if extended:
                    need(extended[0][5] == sym_count * 4 and extended[0][9] == 4, 'invalid extended symbol section indices')
                    ext_data = read(extended[0][4], extended[0][5])
                for symbol_index in range(sym_count):
                    ni, info, other, section_index, value, symbol_size = struct.unpack_from('<IBBHQQ', sym_data, symbol_index * 24)
                    need(ni < len(strings_data), 'symbol name outside string table')
                    name_end = strings_data.find(b'\0', ni)
                    need(name_end >= ni, 'unterminated symbol name')
                    symbol_name = strings_data[ni:name_end].decode('utf-8')
                    binding, symbol_type = info >> 4, info & 15
                    if section_index == 0xffff:
                        need(ext_data is not None, 'missing extended symbol section index')
                        section_index = struct.unpack_from('<I', ext_data, symbol_index * 4)[0]
                        need(section_index < count, 'extended symbol section outside object')
                    else:
                        need(section_index < count or section_index in (0xfff1, 0xfff2), 'unsupported/reserved symbol section index')
                    need(binding in (0, 1, 2, 10), 'unsupported ELF symbol binding')
                    need(symbol_type in (0, 1, 2, 3, 4, 5, 6, 10), 'unsupported ELF symbol type')
                    need(other & ~3 == 0, 'unsupported ELF symbol visibility bits')
                    if binding == 0:
                        continue  # Locals cannot satisfy another object's undefined reference.
                    need(symbol_name and not any(character.isspace() for character in symbol_name), 'invalid global symbol name')
                    symbols.append({'object': identity, 'table_index': table_index, 'symbol_index': symbol_index,
                                    'name': symbol_name, 'binding': binding, 'type': symbol_type,
                                    'visibility': other, 'section_index': section_index, 'value': value, 'size': symbol_size})
            for index, section in enumerate(sections):
                ni, kind, flags, _, file_offset, byte_count, _, _, alignment, _ = section
                need(ni < len(name_bytes), 'invalid section name index')
                name_end = name_bytes.find(b'\0', ni)
                need(name_end >= ni, 'unterminated section name')
                section_name = name_bytes[ni:name_end].decode('utf-8')
                # Names become linker-script literals; refuse meta/shell/path syntax.
                need(not section_name or re.fullmatch(r'[A-Za-z0-9_.$+\-]+', section_name),
                     'section name requires explicit quoting review: ' + repr(section_name))
                need(alignment == 0 or alignment & (alignment - 1) == 0, 'non-power-of-two section alignment')
                if kind != 8:
                    need(file_offset + byte_count <= size, 'section outside actual object')
                rows.append({**identity, 'index': index, 'name': section_name, 'type': kind,
                             'flags': flags, 'size': byte_count, 'align': alignment})
        need(offset == length, 'trailing/truncated archive data')
    need(names == members, 'physical archive member order/names differ from inspector')
    return rows, objects, symbols


def symbol_observations(inputs):
    # Conservative dependency graph, never a claim about actual LLD extraction.
    def key(identity):
        return (identity['archive'], identity['ordinal'], identity['header_offset'])
    definitions, needs, root_definitions, identities = {}, {}, [], {}
    for archive, objects, symbols in inputs:
        for identity in objects:
            object_key = key(identity)
            need(object_key not in identities and identity['archive'] == str(archive), 'physical object identity collision')
            identities[object_key] = identity
            needs[object_key] = set()
        for record in symbols:
            object_key = key(record['object'])
            need(object_key in identities and identities[object_key] == record['object'], 'symbol physical identity mismatch')
            name, binding = record['name'], record['binding']
            if record['section_index'] != 0:
                definitions.setdefault(name, []).append((object_key, binding))
                if name == ROOT and binding == 1 and record['type'] == 2:
                    root_definitions.append(object_key)
            elif binding != 2:
                needs[object_key].add(name)
            # Undefined STB_WEAK stays in the retained symbol ledger, not a strong requirement.
    need(len(root_definitions) == 1, 'genuine initializer strong function definition not unique')
    visited, pending, missing, edges = set(), [root_definitions[0]], set(), []
    while pending:
        member = pending.pop()
        if member in visited:
            continue
        visited.add(member)
        for symbol in sorted(needs[member]):
            choices = definitions.get(symbol, [])
            strong = [item for item in choices if item[1] != 2]
            selected = strong or choices
            edges.append({'consumer': identities[member], 'symbol': symbol,
                          'candidate_providers': [{'object': identities[k], 'binding': binding} for k, binding in selected]})
            if selected:
                pending.extend(item[0] for item in selected)
            else:
                missing.add(symbol)
    boundaries = {'__ehdr_start', '__executable_start', '__init_array_start', '__init_array_end',
                  '__preinit_array_start', '__preinit_array_end', '__fini_array_start', '__fini_array_end',
                  '__bss_start', '_edata', 'edata', '_end', 'end'}
    return {'root_member': identities[root_definitions[0]],
            'candidate_dependency_members': [identities[k] for k in sorted(visited)],
            'symbol_edges': edges,
            'undefined_symbols_without_archive_definition': sorted(missing),
            'actual_script_boundary_references': sorted(missing & boundaries),
            'additional_native_inputs_required': sorted(missing - boundaries),
            'selected_native_input_archives': [str(item[0]) for item in inputs],
            'actual_LLD_selected_members': 'NOT_RUN; use final map/trace, not this conservative graph'}


def make_script(rows):
    allocated = [row for row in rows if row['flags'] & 2 and row['size']]
    need(allocated, 'no actual allocated sections')
    groups = {name: set() for name in ['text', 'ro', 'notes', 'data', 'bss', 'tdata', 'tbss', 'eh', 'gcc', 'preinit', 'init', 'fini']}
    other = set()
    for row in rows:
        name, kind, flags = row['name'], row['type'], row['flags']
        if flags & 2:
            need(kind not in (0, 2, 3, 4, 9, 17, 18), 'allocated linker metadata/relocations require explicit review: ' + name)
        if not name or kind in (0, 2, 3, 4, 9, 17, 18):
            continue  # Metadata/relocations are consumed by LLD; --emit-relocs retains resolved evidence.
        if not flags & 2:
            other.add(name)
            continue
        need(not (flags & 1 and flags & 4), 'actual object has writable executable section')
        # ELF x86-64 SHT_X86_64_UNWIND is the emitted read-only .eh_frame
        # form (0x70000001), not a generic processor-section admission.
        if kind == 0x70000001:
            need(name == '.eh_frame' and flags == 2,
                 'unexpected x86-64 unwind section shape: ' + name)
        else:
            need(kind in (1, 7, 8, 14, 15, 16), 'unhandled allocated section type: ' + name)
        if flags & 0x400:
            need(flags & 1 and not flags & 4 and kind in (1, 8), 'unhandled TLS section shape')
            group = 'tbss' if kind == 8 else 'tdata'
        elif kind in (14, 15, 16):
            group = {14: 'init', 15: 'fini', 16: 'preinit'}[kind]
        elif name == '.eh_frame' or name.startswith('.eh_frame.'):
            group = 'eh'
        elif name == '.gcc_except_table' or name.startswith('.gcc_except_table.'):
            group = 'gcc'
        elif kind == 7:
            need(not flags & (1 | 4), 'unexpected writable/executable note')
            group = 'notes'
        elif flags & 4:
            group = 'text'
        elif not flags & 1:
            group = 'ro'
        else:
            group = 'bss' if kind == 8 else 'data'
        groups[group].add(name)
    # Same input name cannot have contradictory segment/metadata attributes.
    flattened = [n for names in groups.values() for n in names]
    need(len(flattened) == len(set(flattened)) and not set(flattened) & other,
         'section name has contradictory attributes')
    def inputs(group, keep=False):
        names = sorted(groups[group])
        # Preserve normal constructor priority ordering; names with unusual semantics refuse below.
        if group in ('init', 'preinit', 'fini'):
            expected = '.' + {'init': 'init_array', 'preinit': 'preinit_array', 'fini': 'fini_array'}[group]
            need(all(n == expected or n.startswith(expected + '.') for n in names), 'nonstandard constructor table')
            return 'KEEP(*(SORT_BY_INIT_PRIORITY(' + expected + '.*))) KEEP(*(' + expected + '))'
        return ' '.join(('KEEP(*(' + n + '))') if keep else '*(' + n + ')' for n in names)
    tls_align = max([row['align'] for row in rows if row['flags'] & 0x400] + [1])
    # This is an inspection ELF: the first RX page contains headers/padding, no invented gate.
    lines = ['OUTPUT_FORMAT(elf64-x86-64)', 'OUTPUT_ARCH(i386:x86-64)',
             'ENTRY(' + ROOT + ')',
             'PHDRS { text PT_LOAD FILEHDR PHDRS FLAGS(5); ro PT_LOAD FLAGS(4); data PT_LOAD FLAGS(6); tls PT_TLS FLAGS(4); stack PT_GNU_STACK FLAGS(6); }',
             'SECTIONS {', '  . = 0x%x + SIZEOF_HEADERS;' % BASE,
             '  __ehdr_start = 0x%x; __executable_start = 0x%x;' % (BASE, BASE),
             '  . = ALIGN(4096);', '  .text : { ' + inputs('text') + ' } :text',
             '  ASSERT(ADDR(.text) >= 0x%x, "ordinary text intrudes into reserved first page")' % (BASE + 4096),
             '  . = ALIGN(4096);', '  .rodata : { ' + inputs('ro', True) + ' } :ro',
             '  .eh_frame : { ' + inputs('eh', True) + ' } :ro',
             '  .gcc_except_table : { ' + inputs('gcc', True) + ' } :ro',
             '  . = ALIGN(4096);', '  .data : { ' + inputs('data', True) + ' } :data']
    for group in ['preinit', 'init', 'fini']:
        name = {'preinit': 'preinit_array', 'init': 'init_array', 'fini': 'fini_array'}[group]
        lines.append('  .' + name + ' : { __' + name + '_start = .; ' + inputs(group) + ' __' + name + '_end = .; } :data')
    # NOTE sections retain their actual section type instead of being merged into PROGBITS.
    notes_at = lines.index('  . = ALIGN(4096);', lines.index('  .rodata : { ' + inputs('ro', True) + ' } :ro') + 1)
    for name in sorted(groups['notes']):
        lines.insert(notes_at, '  ' + name + ' : { KEEP(*(' + name + ')) } :ro')
        notes_at += 1
    lines.extend(['  . = ALIGN(%d);' % tls_align, '  .tdata : { ' + inputs('tdata', True) + ' } :data :tls',
                  '  .tbss (NOLOAD) : { ' + inputs('tbss', True) + ' } :data :tls',
                  '  _edata = .; edata = .;', '  .bss (NOLOAD) : { __bss_start = .; ' + inputs('bss', True) + ' *(COMMON) } :data',
                  '  _end = .; end = .;', '  ASSERT(ALIGN(.,4096) <= 0x%x, "runtime window overflow")' % END])
    for index, name in enumerate(sorted(other)):
        # Keep original nonallocated debug/attributes section names; no stripping/discard rules.
        lines.append('  ' + name + ' 0 : { *(' + name + ') } :NONE')
    lines.append('}')
    return '\n'.join(lines) + '\n', groups
