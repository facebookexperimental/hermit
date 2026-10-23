#!/usr/bin/env python3
"""Retain decoded DATA from the same closed owned link; never qualification."""
import argparse
from contextlib import ExitStack
import hashlib
import importlib.util
import json
from pathlib import Path
import re
import sys
sys.dont_write_bytecode = True
D = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location('owned_producer', D / 'prepare.py')
m = importlib.util.module_from_spec(spec)
spec.loader.exec_module(m)


def main():
    p = argparse.ArgumentParser(description=__doc__)
    for name in ['result', 'result-sha256', 'binding', 'binding-sha256', 'out']:
        p.add_argument('--' + name, required=True)
    a = p.parse_args()
    result_path, binding_path = Path(a.result), Path(a.binding)
    m.need(m.sha(result_path) == a.result_sha256 and
           m.sha(binding_path) == a.binding_sha256, 'closed receipt/binding changed')
    result = json.loads(result_path.read_text())
    c = json.loads(binding_path.read_text())
    m.need(result['state'] == 'PASS_OWNED_FINAL_LINK_INSPECTION_ONLY' and
           result['source_tools_closed'] is True and result['actual_lld_invocations'] == 1 and
           result['binding_sha256'] == a.binding_sha256 and
           result_path == Path(c['out']) / 'result.json', 'not this successful closed owned link')
    m.runtime_content_preflight(c)
    out = Path(a.out)
    m.need(out.is_absolute() and out.resolve() == out and
           out.is_relative_to(Path(c['evidence_root'])) and not out.exists(),
           'fresh canonical coordinate evidence directory required')
    elf = Path(c['link_paths']['output_directory']) / 'initializer-definitions.not-qualified.elf'
    link_map = elf.with_name('initializer-definitions.map')
    fixed = {str(v): m.file_fact(v) for v in
             [Path(__file__).resolve(), result_path, binding_path, elf, link_map]}
    m.need(result['elf'] == fixed[str(elf)], 'closed actual ELF differs')
    for v in [elf, link_map]:
        m.need(result['produced_outputs'][str(v)] == fixed[str(v)], 'closed link output differs')
    objdump = Path(c['census_objdump'])
    readelf = Path(c['inspection_tools']['readelf']['path'])
    for v in [objdump, readelf]:
        m.need(c['files'][str(v)] == m.sha(v), 'bound observation tool differs')
        fixed[str(v)] = m.file_fact(v)
    out.mkdir()
    record = dict(state='RUNNING', scope='DECODED_COORDINATES_NOT_QUALIFIED', commands=[],
                  receipt=m.file_fact(result_path), binding=m.file_fact(binding_path),
                  ELF=fixed[str(elf)], map=fixed[str(link_map)],
                  source_context={k:c[k] for k in ['H_head','runtime_R_pin','tool_R_head']})
    def close():
        m.files_close(c)
        m.need(m.snapshot(c) == c['source_before'], 'actual producer source changed')
        for v, fact in fixed.items():
            m.need(m.file_fact(v) == fact, 'coordinate source/tool/link input changed: ' + v)
        for v, fact in result['produced_outputs'].items():
            m.need(m.file_fact(v) == fact, 'closed producer output changed: ' + v)
    def run(name, argv):
        close()
        row = dict(name=name, argv=argv, cwd=c['H'])
        record['commands'].append(row)
        command_cancelled = None
        def close_stream(stream):
            nonlocal command_cancelled
            try:
                stream.close()
            except BaseException as error:
                command_cancelled = m.cancellation(command_cancelled, error)
                raise
        try:
            with ExitStack() as streams:
                try:
                    o = (out / (name + '.stdout')).open('xb')
                    streams.callback(close_stream, o)
                    e = (out / (name + '.stderr')).open('xb')
                    streams.callback(close_stream, e)
                    m.bounded_stream_command(argv, c['H'], m.declared_child_environment(c, host=True),
                                             o, e, 512 * 1024 * 1024, 300, row)
                except BaseException as error:
                    command_cancelled = m.cancellation(command_cancelled, error)
                    raise
        except BaseException as error:
            command_cancelled = m.cancellation(command_cancelled, error)
            raise
        finally:
            try:
                for stream in ['stdout','stderr']:
                    v = out / (name + '.' + stream)
                    if v.exists(): row[stream] = m.file_fact(v)
                close()
            finally:
                if command_cancelled is not None:
                    raise command_cancelled
        m.need(row.get('exit') == 0, name + ' failed')
    cancelled = None
    try:
        close()
        record['private_before'] = m.private_close(c)
        run('symbols', [str(readelf), '-sW', str(elf)])
        run('program-headers', [str(readelf), '-lW', str(elf)])
        run('sections', [str(readelf), '-SW', str(elf)])
        run('decoded-text', [str(objdump), '-d', '--wide', '--insn-width=16', str(elf)])
        sites = []
        pattern = re.compile(r'^\s*([0-9a-f]+):\s+((?:[0-9a-f]{2}\s+)+)\s*(\S+)(?:\s+(.*))?$')
        with (out / 'decoded-text.stdout').open() as stream:
            for number, line in enumerate(stream, 1):
                row = pattern.match(line.rstrip())
                if row and row.group(3) == 'syscall':
                    m.need(bytes.fromhex(row.group(2)) == b'\x0f\x05', 'unexpected decoded syscall encoding')
                    sites.append(dict(instruction=int(row.group(1),16),
                                      post_ip=int(row.group(1),16)+2,line=number,bytes='0f05'))
        m.need(0 < len(sites) <= 4096 and len(sites) == len({v['post_ip'] for v in sites}),
               'missing/duplicate/excess decoded syscall rows')
        record['decoded_syscalls'] = sites
        record['state'] = 'RECORDS_OBSERVED_NOT_QUALIFIED'
    except Exception as error:
        record.update(state='FAIL', error=repr(error))
    except BaseException as error:
        cancelled = m.cancellation(cancelled, error)
        record.update(state='FAIL', error=repr(error))
        raise
    finally:
        errors = []
        def cleanup(action, *args):
            nonlocal cancelled
            try:
                action(*args)
            except Exception as error:
                errors.append(repr(error))
            except BaseException as error:
                cancelled = m.cancellation(cancelled, error)
                errors.append(repr(error))
                raise

        def close_stream(fact):
            m.need(m.file_fact(fact['path']) == fact, 'coordinate stream changed')

        def close_private():
            record['private_after'] = m.private_close(c)
            m.need(record.get('private_before') == record['private_after'], 'private inventory changed')

        try:
            with ExitStack() as pending:
                pending.callback(cleanup, close_private)
                pending.callback(cleanup, close)
                for row in reversed(record['commands']):
                    for stream in ['stderr', 'stdout']:
                        if stream in row:
                            pending.callback(cleanup, close_stream, row[stream])
        finally:
            record['source_tools_closed'] = not errors
            if errors: record.update(state='FAIL', close_errors=errors)
            try:
                m.save(out / 'result.json', record)
            finally:
                if cancelled is not None:
                    raise cancelled
    m.need(record['state'] == 'RECORDS_OBSERVED_NOT_QUALIFIED', 'coordinate observation failed')


if __name__ == '__main__':
    main()
