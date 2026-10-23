"""Publish closed private-static DATA, never source/entry/Ready authority."""
import hashlib
import importlib.util
import json
import os
import re
from pathlib import Path
import stat


def need(value, message):
    if not value:
        raise RuntimeError(message)


def validate_roles(roles):
    """Check closed conditional DATA expectations, not source authority."""
    need(type(roles) is dict and set(roles) == {'fallback_entry','completion_syscall','sites'},
         'unknown/missing role DATA')
    need(type(roles['sites']) is list and len(roles['sites']) == 4, 'exactly four native DATA rows')
    owners = [roles['fallback_entry'],roles['completion_syscall']]
    kinds = []
    for row in roles['sites']:
        need(type(row) is dict and set(row) == {'owner','number','operation'}, 'native DATA row shape')
        need(type(row['number']) is int and type(row['operation']) is int, 'native DATA integer types')
        kinds.append((row['operation'],row['number']))
        owners.append(row['owner'])
    need(sorted(kinds) == [(1,158),(2,218),(3,7),(4,2)], 'native DATA source-contract roles differ')
    for owner in owners:
        need(type(owner) is dict and set(owner) == {'symbol','offset','owner_sha256'}, 'owner DATA shape')
        need(type(owner['symbol']) is str and owner['symbol'] and len(owner['symbol']) <= 16384 and
             not any(v in owner['symbol'] for v in ['\0','\n','\r']), 'bounded exact owner symbol required')
        need(type(owner['offset']) is int and 0 <= owner['offset'] < 256*1024 and
             type(owner['owner_sha256']) is str and re.fullmatch('[0-9a-f]{64}',owner['owner_sha256']),
             'bounded exact owner offset/hash required')


def fixed_gate_data(symbol, span, file_offset, gate, reference, decoded, closed_digest):
    """Repeat existing closed structural page observations, not instruction proof.

    GNU objdump -d may render these OBJECT pages as data. Decoded ordinary
    instructions and the three fixed ABI locations are separate evidence.
    """
    executed_symbol = symbol('reverie_inguest_mapped_gate_page')
    reference_symbol = symbol('reverie_inguest_fixed_gate_template')
    need(executed_symbol['value'] == gate and reference_symbol['value'] == reference and
         all(v['type'] == 1 and v['size'] == 4096 for v in [executed_symbol,reference_symbol]),
         'fixed page object coordinates/extent differ')
    need(reference % 4096 == 0 and
         (gate + 4096 <= reference or reference + 4096 <= gate),
         'fixed page/reference address overlap')
    for name,address in [('reverie_inguest_mapped_gate_page_end',gate),
                         ('reverie_inguest_fixed_gate_template_end',reference)]:
        end = symbol(name)
        need(end['value'] == address + 4096 and end['size'] == 0, 'fixed page end alias differs')
    executed = span(gate,4096,5)
    expected = span(reference,4096,4)
    first,second = file_offset(gate,4096,5),file_offset(reference,4096,4)
    need(first+4096 <= second or second+4096 <= first, 'fixed page/reference file bytes overlap')
    need(executed == expected and hashlib.sha256(executed).hexdigest() == closed_digest,
         'fixed page complete bytes/closed structural digest differ')
    need(executed[0x7c:0x85] == bytes.fromhex('f30f1efa0f0541ffe4'), 'fixed native gate ABI differs')
    need(all(executed[offset:offset+2] == b'\x0f\x05' for offset in [0x80,0x200,0x309]),
         'fixed ABI syscall bytes differ')
    actual = {post for post in decoded if gate <= post-2 < gate+4096}
    expected_posts = {gate+offset for offset in [0x82,0x202,0x30b]}
    need(actual <= expected_posts, 'unexpected decoded fixed-page syscall')
    return expected_posts


def zero_set_thread_area_data(observed, owner, expectation, artifact, link_map,
                              runtime_archive, libc, span):
    """Only the existing exact native sixteen-byte section observation, DATA only."""
    need(observed['scope'] == 'SOURCE_MEMBER_MAP_RX_IDENTITIES_ONLY_NOT_SYSCALL_OR_ENTRY_AUTHORIZATION' and
         observed['artifact'] == artifact and observed['link_map'] == link_map and
         observed['runtime_archive'] == runtime_archive,
         'native observation belongs to another actual owner/image/map')
    leaf = observed['set_thread_area']
    need(expectation['symbol'] == owner['name'] == leaf['symbol'] == '__set_thread_area' and
         owner['type'] == 2 and owner['size'] == leaf['actual_symbol_size'] == 0 and
         owner['value'] == leaf['final_entry'] and owner['binding'] == leaf['final_binding'] and
         owner['visibility'] == leaf['final_visibility'],
         'exact zero-sized native symbol differs')
    need(leaf['archive'] == libc and leaf['member_ordinal'] == 1223 and
         leaf['member_header'] == 5904434 and
         leaf['member_sha256'] == '0f842b24b5425118e12f36fd97e7fd7e91b97728e49a7b52c26a84192754416c' and
         leaf['input_section'] == 1 and leaf['complete_section_bytes'] == 16,
         'exact native physical section/member differs')
    code = span(owner['value'],16,5)
    digest = hashlib.sha256(code).hexdigest()
    need(code == bytes.fromhex('4889febf02100000b89e0000000f05c3') and
         digest == leaf['code_sha256'] == expectation['owner_sha256'] and
         expectation['offset'] == 13,
         'exact native sixteen-byte body/digest/syscall offset differs')
    return owner['value'] + 13


def publish(m, c, result_path, coordinates_path, roles, destination, close_pipeline):
    """Use existing ELF readers and exact closed output identities only."""
    validate_roles(roles)
    result = json.loads(result_path.read_text())
    coordinates = json.loads(coordinates_path.read_text())
    need(result['state'] == 'PASS_OWNED_FINAL_LINK_INSPECTION_ONLY' and
         result['source_tools_closed'] is True and result['actual_lld_invocations'] == 1,
         'owned final link did not close successfully')
    need(coordinates['state'] == 'RECORDS_OBSERVED_NOT_QUALIFIED' and
         coordinates['source_tools_closed'] is True and
         coordinates['receipt'] == m.file_fact(result_path) and
         coordinates['ELF'] == result['elf'], 'coordinates are not this closed actual image')
    original = Path(result['elf']['path'])
    need(m.sha(m.LEGACY) == m.LEGACY_SHA, 'existing ELF inspection helper changed')
    spec = importlib.util.spec_from_file_location('package_existing_elf_helpers', m.LEGACY)
    existing = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(existing)
    flags = os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK
    pending_image = pending_data = None
    created_image = created_data = False
    original_error = None
    final_errors = []
    destination = Path(destination)
    need(destination.is_absolute() and destination.resolve() == destination and
         not destination.exists(), 'fresh canonical package directory required')
    destination.mkdir()
    fd = os.open(original, flags)
    image_path = destination / 'detcore_private_runtime.elf'
    data_path = destination / 'detcore_private_runtime.package.json'
    def identity(info):
        return (info.st_dev, info.st_ino, info.st_mode, info.st_nlink, info.st_uid,
                info.st_gid, info.st_size, info.st_mtime_ns, info.st_ctime_ns)
    try:
        before = os.fstat(fd)
        need(stat.S_ISREG(before.st_mode) and before.st_uid == os.geteuid() and
             0 < before.st_size <= 512 * 1024 * 1024, 'bounded owned image required')
        # Reuse the existing reader through this retained descriptor, so its
        # path open cannot race a replacement of the original output name.
        headers = existing.module('elf_metadata.py').elf_headers(Path('/proc/self/fd') / str(fd))
        chunks = []
        remaining = before.st_size
        while remaining:
            chunk = os.read(fd, min(1024 * 1024, remaining))
            need(chunk, 'truncated retained package image')
            chunks.append(chunk)
            remaining -= len(chunk)
        raw = b''.join(chunks)
        need(identity(os.fstat(fd)) == identity(before) and len(raw) == result['elf']['bytes'] and
             hashlib.sha256(raw).hexdigest() == result['elf']['sha256'],
             'actual opened image differs from closed output')
        elf = existing.ElfEvidence(raw)
        def file_offset(address, length, permission):
            need(type(address) is int and type(length) is int and address >= 0 and
                 0 < length <= 512 * 1024 * 1024 and address + length <= 2**64,
                 'invalid package coordinate span')
            candidates = [p for p in headers['segments'] if p['type'] == 1 and
                          p['vaddr'] <= address and address + length <= p['vaddr'] + p['filesz']]
            need(len(candidates) == 1, 'coordinate outside unique initialized PT_LOAD')
            p = candidates[0]
            need(p['flags'] == permission, 'coordinate actual RX/RO permission differs')
            at = p['offset'] + address - p['vaddr']
            return at
        def span(address, length, permission):
            return elf.span(file_offset(address,length,permission),length)
        def symbol(name):
            found = [s for s in elf.symbols if s['name'] == name and
                     0 < s['section_index'] < len(elf.sections)]
            need(len(found) == 1, 'package symbol absent/duplicate: ' + name)
            return found[0]
        def site(expectation, zero_native=False):
            need(set(expectation) == {'symbol','offset','owner_sha256'}, 'closed site DATA shape differs')
            owner = symbol(expectation['symbol'])
            if zero_native and owner['size'] == 0:
                ledger = result['native_leaf_retained_source_closed_files']
                matches = [fact for path,fact in ledger.items()
                           if Path(path).name == 'native-leaves.not-proven.json']
                need(len(matches) == 1, 'actual native JSON receipt absent/ambiguous')
                fact = matches[0]
                need(0 < fact['bytes'] <= 65536 and m.file_fact(fact['path']) == fact and
                     result['produced_outputs'].get(fact['path']) == fact,
                     'actual retained native JSON receipt changed/unclosed')
                native = json.loads(Path(fact['path']).read_text())
                libc_path = c['link_paths']['libc']
                libc = dict(path=libc_path,sha256=m.sha(libc_path))
                return zero_set_thread_area_data(native,owner,expectation,
                    dict(path=str(original),sha256=result['elf']['sha256']),
                    dict(path=coordinates['map']['path'],sha256=coordinates['map']['sha256']),
                    structural['observation']['runtime_archive'],libc,span)
            body = elf.function_bytes(owner)
            need(hashlib.sha256(body).hexdigest() == expectation['owner_sha256'],
                 'conditional site owner whole-body expectation differs')
            offset = expectation['offset']
            need(type(offset) is int and 0 <= offset < len(body), 'site offset outside actual whole owner')
            return owner['value'] + offset
        entry = symbol('detcore_liteinst_mapped_entry')['value']
        need(entry == headers['entry'], 'package entry differs from actual ELF entry')
        terminal = symbol('detcore_liteinst_mapped_entry_observed_int3')['value']
        need(span(terminal,1,5) == b'\xcc', 'actual initializer observer is not INT3')
        layout = symbol('detcore_liteinst_mapped_layout')['value']
        layout_bytes = span(layout,96,4)
        reference = symbol('reverie_inguest_fixed_gate_template')['value']
        span(reference,4096,4)
        # Existing layout ABI, not a new codec or acceptance predicate.
        import struct
        words = struct.unpack('<12Q',layout_bytes)
        need(words[:3] == (int.from_bytes(b'HLMLAY01','little'),1,96), 'layout descriptor ABI differs')
        gate = words[10]
        need(gate == 0x6000e1001000 and words[11] == gate + 0x7c,
             'fixed gate ABI coordinates differ')
        census_fact = result['startup_census_source_closed_identity']
        need(m.file_fact(census_fact['path']) == census_fact, 'actual source-closed census changed')
        census = json.loads(Path(census_fact['path']).read_text())
        need(census['artifact'] == {'path':str(original),'sha256':result['elf']['sha256']} and
             census['link_map'] == {'path':coordinates['map']['path'],'sha256':coordinates['map']['sha256']},
             'census/coordinate actual image-map association differs')
        handlers = [v for v in census['bodies'] if v['role'] == 'bootstrap_sigsys']
        need(len(handlers) == 1, 'bootstrap handler census absent/duplicate')
        handler = handlers[0]
        need(handler['complete_disassembly'] is True and 0 < handler['complete_bytes'] <= 65536 and
             handler['symbols'], 'bootstrap handler complete extent unavailable')
        addresses = {s['address'] for s in handler['symbols']}
        need(len(addresses) == 1 and all(s['size'] == handler['complete_bytes'] for s in handler['symbols']),
             'handler aliases/whole extent differ')
        handler_address = next(iter(addresses))
        handler_raw = span(handler_address,handler['complete_bytes'],5)
        need(hashlib.sha256(handler_raw).hexdigest() == handler['bytes_sha256'], 'handler complete bytes differ')
        literals = []
        for p in headers['segments']:
            if p['type'] != 1 or p['flags'] != 4: continue
            body = elf.span(p['offset'],p['filesz'])
            at = body.find(b'/dev/null\0')
            while at >= 0:
                literals.append(p['vaddr'] + at)
                at = body.find(b'/dev/null\0',at+1)
        need(len(literals) == 1, 'RO dev-null literal absent/ambiguous')
        decoded = {row['post_ip']: row for row in coordinates['decoded_syscalls']}
        need(len(decoded) == len(coordinates['decoded_syscalls']), 'duplicate decoded post-PC')
        for post,row in decoded.items():
            need(row['instruction'] + 2 == post and row['bytes'] == '0f05' and
                 span(post-2,2,5) == b'\x0f\x05', 'decoded syscall actual RX bytes differ')
        structural_fact = result['explicit_structural_receipt_file']
        need(m.file_fact(structural_fact['path']) == structural_fact,
             'closed structural receipt changed')
        structural = json.loads(Path(structural_fact['path']).read_text())
        need(structural == result['explicit_structural_source_closed_receipt'] and
             structural['source_factory_close'] == 'PASS' and
             structural['archive_close'] == 'PASS', 'actual structural close differs')
        inspection = structural['observation']['explicit_structural']
        need(inspection['artifact'] == {'path':str(original),'sha256':result['elf']['sha256']},
             'closed gate observation belongs to another artifact')
        expected_gates = fixed_gate_data(symbol,span,file_offset,gate,reference,decoded,
                                         inspection['gate_sha256'])
        ordinary = sorted(set(decoded) - expected_gates)
        need(0 < len(ordinary) <= 4096, 'ordinary decoded table bound')
        need(set(roles) == {'fallback_entry','completion_syscall','sites'}, 'unknown/missing role DATA')
        fallback = site(roles['fallback_entry'])
        completion = site(roles['completion_syscall'])
        need(fallback < completion and span(completion,4,5) == b'\x0f\x05\x0f\x0b' and
             completion+2 in ordinary, 'actual observer continuation coordinates differ')
        native = []
        need(len(roles['sites']) == 4, 'exactly four source-contract site expectations required')
        for row in roles['sites']:
            need(set(row) == {'owner','number','operation'}, 'native site expectation shape')
            ip = site(row['owner'], zero_native=(row['operation'],row['number']) == (1,158))
            need(ip+2 in decoded and span(ip,2,5) == b'\x0f\x05', 'native site is not decoded actual RX syscall')
            native.append([ip,ip+2,row['number'],row['operation']])
        need(sorted((v[3],v[2]) for v in native) == [(1,158),(2,218),(3,7),(4,2)] and
             len({v[0] for v in native}) == 4, 'finite source-contract operation roles differ')
        data = dict(schema='hermit.private-static-runtime-data.v1',
                    image=dict(bytes=len(raw),sha256=hashlib.sha256(raw).hexdigest()),
                    entry=entry,terminal_int3=terminal,fallback_entry=fallback,completion_syscall=completion,
                    observed=dict(layout_address=layout,reference_address=reference,
                                  handler_address=handler_address,handler_bytes=len(handler_raw),
                                  handler_sha256=hashlib.sha256(handler_raw).hexdigest(),
                                  dev_null_address=literals[0],sites=native,ordinary_post_ips=ordinary))
        encoded = (json.dumps(data,indent=2,sort_keys=True)+'\n').encode()
        need(len(encoded) <= 1024*1024, 'descriptor output exceeds consumer bound')
        pending_image = destination / '.detcore_private_runtime.elf.pending'
        pending_data = destination / '.detcore_private_runtime.package.json.pending'
        for path,body in [(pending_image,raw),(pending_data,encoded)]:
            with path.open('xb') as output:
                output.write(body); output.flush(); os.fsync(output.fileno())
            os.chmod(path,0o444)
        def close_inputs():
            need(identity(os.fstat(fd)) == identity(before) and
                 identity(original.lstat()) == identity(before) and m.file_fact(original) == result['elf'],
                 'retained opened/original package image changed')
            close_pipeline()
            need(pending_image.read_bytes() == raw and pending_data.read_bytes() == encoded,
                 'pending package output changed')
        # link is exclusive: existing packages are never overwritten. Descriptor
        # remains absent until both files and all actual producer owners close.
        os.link(pending_image,image_path)
        created_image = True
        close_inputs()
        need(image_path.read_bytes() == raw, 'published image bytes changed')
        os.link(pending_data,data_path)
        created_data = True
        directory = os.open(destination,os.O_RDONLY|os.O_DIRECTORY)
        try: os.fsync(directory)
        finally: os.close(directory)
        return dict(state='PUBLISHED_PRIVATE_STATIC_DATA_NOT_QUALIFIED',
                    image=m.file_fact(image_path),descriptor=m.file_fact(data_path),
                    owned_result=m.file_fact(result_path),coordinates=m.file_fact(coordinates_path))
    except BaseException as error:
        original_error = error
        raise
    finally:
        # Do not hide a body/publication failure behind a later cleanup failure.
        # Own only fresh paths this invocation created; no existing file removal.
        try:
            os.close(fd)
        except BaseException as error: final_errors.append(repr(error))
        for path in [pending_image,pending_data]:
            if path is not None:
                try: path.unlink(missing_ok=True)
                except BaseException as error: final_errors.append(repr(error))
        if original_error is not None or final_errors:
            for created,path in [(created_data,data_path),(created_image,image_path)]:
                if created:
                    try: path.unlink()
                    except BaseException as error: final_errors.append(repr(error))
        if final_errors:
            raise RuntimeError({'body_error':repr(original_error),'cleanup_errors':final_errors}) from original_error
