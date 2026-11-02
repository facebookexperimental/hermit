"""Strict observation protocol, no source/leaf capability constructor."""
import hashlib
import json
from pathlib import Path
import re
import stat

CAP = 128 * 1024 * 1024
HANDOFF_SOURCE_SHA256 = '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'

def require(ok, message):
    if not ok:
        raise RuntimeError(message)

def fact(path):
    path = Path(path)
    before = path.lstat()
    require(stat.S_ISREG(before.st_mode) and before.st_size <= CAP,
            'view evidence not bounded regular file: ' + str(path))
    raw = path.read_bytes()
    after = path.lstat()
    require((before.st_dev, before.st_ino, before.st_mode, before.st_size) ==
            (after.st_dev, after.st_ino, after.st_mode, after.st_size) and len(raw) == before.st_size,
            'view evidence changed while reading: ' + str(path))
    return {'path': str(path), 'sha256': hashlib.sha256(raw).hexdigest()}, raw

def parse(diagnostic, logs, *, source_closed, tool_head, h_head, runtime_pin,
          site_sha, table_sha, helper_sha, archive_identity, elf_identity, map_identity):
    """Reconcile all evidence even when another owned consumer returned failure.

    A returned REFUSED is data, never execution authority. Malformed/incomplete
    protocol raises; the outer caller must retain that error and still inspect
    the actual ELF via its existing independent-final-inspection path.
    """
    lines = diagnostic.splitlines()
    def one(key):
        values = [line[len(key)+1:] for line in lines if line.startswith(key + '=')]
        require(len(values) == 1, 'missing/duplicate view marker: ' + key)
        return values[0]
    marker = one('VIEW_LEAF_CONTROLS')
    require(marker in {'ACTUAL_LEAF_FACTORY_CONTROLS_ONLY_NOT_QUALIFIED', 'REFUSED'},
            'invalid view aggregate marker')
    require(source_closed is True, 'view markers are not from a closed source transaction')
    count = one('VIEW_LEAF_CONTROL_ROWS')
    require(re.fullmatch('[0-8]', count) is not None, 'invalid view control count')
    role_markers = {role: one('VIEW_' + role.upper() + '_LEAF') for role in ('site', 'table')}
    require(all(v in {'OBSERVED_NOT_QUALIFIED', 'REFUSED', 'NOT_REACHED'} for v in role_markers.values()),
            'invalid view role marker')
    report_hash = one('VIEW_LEAF_REPORT_SHA256')
    require(re.fullmatch('[0-9a-f]{64}', report_hash) is not None, 'invalid view report hash')
    logs = Path(logs)
    identities = []
    def read(path):
        identity, raw = fact(path)
        identities.append(identity)
        return identity, raw
    identity, raw = read(logs / 'view-leaf-controls.not-qualified.json')
    require(identity['sha256'] == report_hash, 'view report differs from source-closed Rust identity')
    report = json.loads(raw)
    for key, value in {'schema': 1, 'tool_R_head': tool_head, 'runtime_H_head': h_head,
                       'runtime_R_pin': runtime_pin, 'site_module_sha256': site_sha,
                       'table_module_sha256': table_sha, 'helper_source_sha256': helper_sha,
                       'required_roles': 2, 'required_controls': 8,
                       'qualification': 'NOT_PROVEN', 'caller_and_async': 'NOT_PROVEN',
                       'readiness': 'NOT_PROVEN', 'guests_executed': 0}.items():
        require(report.get(key) == value and type(report.get(key)) is type(value),
                'view report binding differs: ' + key)
    require(report.get('restoration_failed') is False, 'view restoration not confirmed')
    roles = report.get('roles')
    require(isinstance(roles, list) and [r.get('role') for r in roles] == ['site', 'table'],
            'view roles missing, duplicated or reordered')
    row_count = 0
    all_pass = True
    script_path = str(logs / 'optional-inputs/selected-link-script.ld')
    for role in roles:
        name = role['role']
        rows = role.get('controls')
        require(isinstance(rows, list) and [r.get('case') for r in rows] ==
                ['positive', 'callback_error', 'producer_ledger_mutation_close', 'foreign_source_owner'],
                'view actual control identities differ')
        row_count += len(rows)
        for row in rows:
            require(row.get('state') in {'PASS', 'FAIL'} and type(row.get('callback_count')) is int,
                    'view control state/count invalid')
        passed = all(row['state'] == 'PASS' for row in rows)
        require(role.get('state') == role_markers[name] ==
                ('OBSERVED_NOT_QUALIFIED' if passed else 'REFUSED'), 'view role state mismatch')
        all_pass &= passed
        # These checks validate every positive assertion; FAIL rows remain raw
        # failure data, never accepted as alternative qualifying predicates.
        for row in rows:
            if row['state'] != 'PASS':
                continue
            require(row['callback_count'] == (0 if row['case'] == 'foreign_source_owner' else 1),
                    'view callback count differs for actual control')
            if row['case'] == 'positive':
                require(row['factory_result'] == row['post_close'] == 'Ok(())',
                        'view positive did not close')
                _, observed_raw = read(logs / ('view-' + name + '-leaf.not-qualified.json'))
                require(json.loads(observed_raw) == role.get('observation'), 'view actual observation changed')
                observation = role['observation']
                for field, expected in [('runtime_archive', archive_identity),
                                        ('artifact', elf_identity), ('link_map', map_identity)]:
                    require(observation.get(field) == expected, 'view leaf actual identity differs: ' + field)
                require(observation.get('source', {}).get('sha256') ==
                        HANDOFF_SOURCE_SHA256,
                        'view leaf actual handoff source differs')
                source_identity, _ = read(observation['source']['path'])
                require(source_identity == observation['source'], 'view actual resolved source changed')
            elif row['case'] == 'callback_error':
                sentinel = 'actual View::' + name + ' leaf callback error control'
                require(row['expected_exact_error'] == sentinel and row['post_close'] == 'Ok(())' and
                        row['factory_result'] == 'Err(' + json.dumps(sentinel) + ')',
                        'view callback error was not preserved exactly')
            elif row['case'] == 'producer_ledger_mutation_close':
                expected = 'static source transaction close: file identity changed: ' + script_path
                require(row['expected_exact_error'] == expected and row['restore'] == 'Ok(())' and
                        row['post_restore_close'] == 'Ok(())' and
                        row['factory_result'] == 'Err(' + json.dumps(expected) + ')',
                        'view mutation did not reach its exact producer-ledger close')
                for key, suffix in [('backup', 'original.bin'), ('journal', 'journal.json')]:
                    captured, _ = read(logs / ('view-' + name + '-script.' + suffix))
                    require(captured == row[key], 'view mutation recovery evidence differs: ' + key)
                backup = row['backup']
                script, _ = read(script_path)
                require(script['sha256'] == backup['sha256'], 'view selected script not restored to backup')
            else:
                expected = 'static archive belongs to another source transaction'
                require(row['callback_count'] == 0 and row['expected_exact_error'] == expected and
                        row['factory_result'] == 'Err(' + json.dumps(expected) + ')' and
                        row['nested_source_close'] == row['outer_input_close'] == 'Ok(())' and
                        row['no_additional_link'] is True and
                        re.fullmatch('[0-9a-f]{64}', row['outer_metadata_sha256']) is not None and
                        row['outer_metadata_sha256'] == row['nested_metadata_sha256'] == row['archive_metadata_sha256'],
                        'View foreign live owner not refused at exact API boundary')
    require(row_count == int(count) == 8, 'view eight actual controls not accounted')
    expected_state = 'ACTUAL_LEAF_FACTORY_CONTROLS_ONLY_NOT_QUALIFIED' if all_pass else 'REFUSED'
    require(marker == report['state'] == expected_state, 'view aggregate differs from individual outcomes')
    # Seal all reads again; this list is also retained by the outer wrapper's
    # produced-file ledger and checked at its normal final-close.
    for expected in identities:
        current, _ = fact(expected['path'])
        require(current == expected, 'view protocol file changed before parser close')
    return {'state': expected_state, 'controls_observed': row_count, 'roles': roles,
            'report': identity, 'files': identities, 'qualification': 'NOT_PROVEN',
            'readiness': 'NOT_PROVEN', 'guests_executed': 0}
