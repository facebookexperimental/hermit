#!/usr/bin/env python3
"""Fill a hermit.private-static-producer-inputs.v1 config from real inputs.

Reads the producer-inputs.example.json schema beside this file, verifies every
code-pinned SHA256 the pipeline stages check, derives the three heads from the
supplied checkouts, computes the file/module digests, and writes the filled
config to --out. It never builds, fetches, or writes outside --out.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import sys

sys.dont_write_bytecode = True

SCHEMA = 'hermit.private-static-producer-inputs.v1'
PACKAGE_ROLE_SCHEMA = 'hermit.private-static-package-roles.source-owned.v1'

# Code-pinned SHA256 values, each also checked by the pipeline stage named in
# its citation. test_file_pins.py classifies these literals as EXCLUDED inputs
# outside this tree; a pin change is a reviewed change here and there together.
PRIVATE_INVENTORY_SHA256 = 'f825b66769fb55bee298b0a41277b38767970e5cad4beca8400f02dcdd811fcc'  # owned/prepare.py PRIVATE_INVENTORY_SHA256
INSPECTION_TOOLS_SHA256 = 'b5b75a7dbf58d0805bbfcfb8d1fc17e55b1c528a2d2d210d5def39ea559d97a0'  # owned/prepare.py inspection tool inventory
MUSL_ARCHIVE_SHA256 = 'a9a118bbe84d8764da0ea0d28b3ab3fae8477fc7e4085d90102b8596fc7c75e4'  # owned/prepare.py musl source archive
AUTHORITY_INVENTORY_SHA256 = '6aeea4469c8ba49c4c475a6694972e6a1ef3974954c9f986f99d7d1bce926275'  # owned/section/prepare.py authority inventory.json
MEMCPY_SOURCE_SHA256 = '790bdd75f7736c83ed7a21f8813b20454ead1ccfc49e806a88692d2a471024eb'  # owned/section/prepare.py memcpy_source
STACK_CHK_SOURCE_SHA256 = '2ea4d8cf6758daddecf8973913ac196ffa76fdb355cea391ca890aeafdc2ac6c'  # owned/section/prepare.py stack_chk_source
OPTIONAL_STD_WEAK_SHA256 = '0f9c4e63d9f15b2486e8c0a9af89c4c2ad88fcad7c1a5127a95d8031e13ec2d3'  # owned/prepare.py std_weak
OPTIONAL_STD_DESTRUCTORS_SHA256 = '5634b13e8faa9ead0e7dac104abf51a990bc14cbc39c5c6bae22a7b70811e2b3'  # owned/prepare.py std_destructors
OPTIONAL_MUSL_DL_ITERATE_SHA256 = '802a537924c900ff607b65bda465bf66899890b7b0707c0cd55c7b800db20327'  # owned/prepare.py musl_dl_iterate
LIBC_BYTES = 6406314
LIBC_SHA256 = 'e699c64b0c6b427d89ad5bf767bf78beb30c19930c5dedff635951636e8f9f70'  # owned/section/prepare.py self-contained libc.a
LIBUNWIND_BYTES = 201418
LIBUNWIND_SHA256 = 'eb16569e611a5d6cdda2e7604fd6402a3ad810817dd8d63ed50baa2ce8070aaa'  # owned/section/prepare.py self-contained libunwind.a
RUST_LLD_SHA256 = 'eb9b9bc681e57f7ff4166f8fb57fad7336d29a51c85e8c70ba1ca203600e392b'  # owned/section/prepare.py rust-lld
LLVM_OBJDUMP_SHA256 = '8f906de30ee713c953d1762f78901b3a5d4ccd99f89c65627b712618aeeddcf4'  # owned/prepare.py private llvm-objdump
HOST_OBJDUMP_PATH = '/usr/bin/objdump'
HOST_OBJDUMP_SHA256 = '0a1b558f444ed2bca7fb467dede7416a651bc490b1abc77705ab470d99c527c1'  # owned/prepare.py census GNU objdump 2.35.2

MODULE_FILES = {
    'initializer_module': 'reverie-liteinst-artifact/src/private_static/selected_initializers.rs',
    'tls_source_module': 'reverie-liteinst-artifact/src/private_static/tls_sources.rs',
    'native_leaf_module': 'reverie-liteinst-artifact/src/private_static/selected_initializers/native_leaves.rs',
    'ordinary_gate_module': 'reverie-liteinst-artifact/src/private_static/selected_initializers/ordinary_gate.rs',
    'terminal_module': 'reverie-liteinst-artifact/src/private_static/selected_initializers/terminal_observation.rs',
    'view_site_module': 'reverie-liteinst-artifact/src/private_static/selected_initializers/view_site.rs',
    'view_table_module': 'reverie-liteinst-artifact/src/private_static/selected_initializers/view_table.rs',
    'package_roles_module': 'reverie-liteinst-artifact/src/private_static/selected_initializers/package_roles.rs',
}

# Source-owned role coordinates are fixed by owned/publish.py validate_roles;
# only the operation-1 owner body hash is a free value (--owner-sha256).
SOURCE_OWNED_SITES = ((2, 218, '__init_tp', 44), (3, 7, '__init_libc', 302),
                      (4, 2, '__init_libc', 388))


def need(value, message):
    if not value:
        raise RuntimeError(message)


def sha(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(chunk)
    return digest.hexdigest()


def canonical(path, directory=False):
    path = Path(path)
    need(path.is_absolute() and path.resolve() == path,
         'canonical absolute path required: ' + str(path))
    need(path.is_dir() if directory else path.is_file(),
         'existing directory required: ' + str(path) if directory
         else 'existing regular file required: ' + str(path))
    return path


def check_pin(path, expected, label, size=None):
    path = canonical(path)
    observed = sha(path)
    need(observed == expected,
         label + ' sha256 differs: ' + str(path)
         + '; expected=' + expected + ', observed=' + observed)
    if size is not None:
        need(path.stat().st_size == size,
             label + ' size differs: ' + str(path)
             + '; expected=' + str(size) + ', observed=' + str(path.stat().st_size))
    return path


def git(source, *args):
    environment = dict(os.environ, GIT_OPTIONAL_LOCKS='0')
    return subprocess.check_output(
        ['git', '-c', 'diff.autoRefreshIndex=false', '-C', str(source), *args],
        env=environment, timeout=30)


def head_of(checkout, label):
    head = git(checkout, 'rev-parse', 'HEAD').decode().strip()
    need(re.fullmatch('[0-9a-f]{40}', head), label + ' HEAD is not a full commit: ' + str(checkout))
    return head


def gitlink_of(hermit):
    row = git(hermit, 'ls-tree', 'HEAD', 'reverie').decode().strip()
    parts = row.replace('\t', ' ').split()
    need(len(parts) >= 3 and parts[0] == '160000' and parts[1] == 'commit'
         and re.fullmatch('[0-9a-f]{40}', parts[2]),
         'Hermit HEAD has no reverie gitlink: ' + str(hermit))
    return parts[2]


def check_prefix(prefix):
    gnu_bin = prefix / 'lib/rustlib/x86_64-unknown-linux-gnu/bin'
    self_contained = prefix / 'lib/rustlib/x86_64-unknown-linux-musl/lib/self-contained'
    check_pin(self_contained / 'libc.a', LIBC_SHA256,
              'private self-contained libc.a (pin owned/section/prepare.py)', LIBC_BYTES)
    check_pin(self_contained / 'libunwind.a', LIBUNWIND_SHA256,
              'private self-contained libunwind.a (pin owned/section/prepare.py)', LIBUNWIND_BYTES)
    check_pin(gnu_bin / 'rust-lld', RUST_LLD_SHA256,
              'private rust-lld (pin owned/section/prepare.py)')
    check_pin(gnu_bin / 'llvm-objdump', LLVM_OBJDUMP_SHA256,
              'private llvm-objdump (pin owned/prepare.py)')
    check_pin(HOST_OBJDUMP_PATH, HOST_OBJDUMP_SHA256,
              'host census GNU objdump (pin owned/prepare.py)')


def check_native_inputs(path, prefix):
    reference = json.loads(Path(path).read_text())
    need(isinstance(reference, dict) and isinstance(reference.get('toolchain'), dict),
         'native-inputs manifest has no toolchain object: ' + str(path))
    toolchain = reference['toolchain']
    for key in ('prefix', 'input_inventory', 'cargo_sha256', 'rustc_sha256', 'rustdoc_sha256'):
        need(isinstance(toolchain.get(key), str) and toolchain[key],
             'native-inputs toolchain.' + key + ' missing: ' + str(path))
    need(Path(toolchain['prefix']) == prefix,
         'native-inputs toolchain.prefix differs from --private-prefix: '
         + toolchain['prefix'] + ' != ' + str(prefix))
    check_pin(toolchain['input_inventory'], PRIVATE_INVENTORY_SHA256,
              'private toolchain inventory (pin owned/prepare.py PRIVATE_INVENTORY_SHA256)')
    for name, key in (('cargo', 'cargo_sha256'), ('rustc', 'rustc_sha256'),
                      ('rustdoc', 'rustdoc_sha256')):
        binary = canonical(prefix / 'bin' / name)
        need(os.access(binary, os.X_OK), 'private compiler not executable: ' + str(binary))
        need(sha(binary) == toolchain[key],
             'private ' + name + ' sha256 differs from native-inputs toolchain.'
             + key + ': ' + str(binary))


def check_authority(directory, source_owned):
    inventory_path = check_pin(
        directory / 'inventory.json', AUTHORITY_INVENTORY_SHA256,
        'musl source authority inventory.json (pin owned/section/prepare.py)')
    inventory = json.loads(inventory_path.read_text())
    need(isinstance(inventory, dict) and isinstance(inventory.get('files'), list),
         'musl source authority inventory has no files list: ' + str(inventory_path))
    for row in inventory['files']:
        need(isinstance(row, dict) and set(row) == {'path', 'bytes', 'sha256'}
             and isinstance(row['path'], str) and type(row['bytes']) is int
             and isinstance(row['sha256'], str)
             and re.fullmatch('[0-9a-f]{64}', row['sha256']),
             'musl source authority row shape differs: ' + repr(row))
        relative = Path(row['path'])
        need(not relative.is_absolute() and all(p not in ('..', '.') for p in relative.parts),
             'musl source authority path escape: ' + row['path'])
        source = canonical(directory / relative)
        need(source.stat().st_size == row['bytes'] and sha(source) == row['sha256'],
             'musl source authority file differs: ' + str(source))
    if source_owned:
        for name in ('__libc_start_main.c', '__init_tls.c'):
            canonical(directory / 'musl-1.2.5-selected/src/env' / name)


def find_optional(directory):
    by_digest = {}
    for path in sorted(directory.iterdir()):
        if path.is_file():
            by_digest.setdefault(sha(path), []).append(path)
    found = {}
    for label, digest in (('optional_std_weak_source', OPTIONAL_STD_WEAK_SHA256),
                          ('optional_std_destructors_source', OPTIONAL_STD_DESTRUCTORS_SHA256),
                          ('optional_musl_dl_iterate_source', OPTIONAL_MUSL_DL_ITERATE_SHA256)):
        matches = by_digest.get(digest, [])
        need(len(matches) == 1,
             label + ' not uniquely identified by its pinned sha256 in --optional-source-dir '
             + str(directory) + ' (pin owned/prepare.py; digest=' + digest
             + ', matches=' + str(len(matches)) + ')')
        found[label] = canonical(matches[0])
    return found


def source_owned_roles(owner_sha256):
    need(isinstance(owner_sha256, str) and re.fullmatch('[0-9a-f]{64}', owner_sha256),
         '--owner-sha256 must be the 64-hex owner body sha256 of __set_thread_area')
    sites = [dict(operation=1, number=158,
                  owner=dict(symbol='__set_thread_area', offset=13,
                             owner_sha256=owner_sha256))]
    sites += [dict(operation=op, number=num, owner=dict(symbol=name, offset=off))
              for op, num, name, off in SOURCE_OWNED_SITES]
    return dict(schema=PACKAGE_ROLE_SCHEMA,
                fallback_entry=dict(symbol='fallback_entry', offset=0),
                completion_syscall=dict(symbol='fallback_entry', offset=44),
                sites=sites)


def legacy_roles(path):
    roles = json.loads(canonical(path).read_text())
    need(isinstance(roles, dict) and set(roles) == {'fallback_entry', 'completion_syscall', 'sites'},
         'legacy roles shape differs (owned/publish.py _validate_legacy_roles): ' + str(path))
    need(isinstance(roles['sites'], list) and len(roles['sites']) == 4,
         'legacy roles need exactly four native DATA rows: ' + str(path))
    owners = [roles['fallback_entry'], roles['completion_syscall']]
    kinds = []
    for row in roles['sites']:
        need(isinstance(row, dict) and set(row) == {'owner', 'number', 'operation'}
             and type(row['number']) is int and type(row['operation']) is int,
             'legacy roles row shape differs: ' + str(path))
        kinds.append((row['operation'], row['number']))
        owners.append(row['owner'])
    need(sorted(kinds) == [(1, 158), (2, 218), (3, 7), (4, 2)],
         'legacy roles source-contract rows differ: ' + str(path))
    for owner in owners:
        need(isinstance(owner, dict) and set(owner) == {'symbol', 'offset', 'owner_sha256'}
             and isinstance(owner['symbol'], str) and owner['symbol']
             and len(owner['symbol']) <= 16384
             and not any(v in owner['symbol'] for v in ('\0', '\n', '\r'))
             and type(owner['offset']) is int and 0 <= owner['offset'] < 256 * 1024
             and isinstance(owner['owner_sha256'], str)
             and re.fullmatch('[0-9a-f]{64}', owner['owner_sha256']),
             'legacy roles owner shape differs: ' + str(path))
    return roles


def fill(inputs):
    hermit = canonical(inputs.hermit, directory=True)
    runtime_reverie = canonical(inputs.runtime_reverie, directory=True)
    tool_reverie = canonical(inputs.tool_reverie, directory=True)
    prefix = canonical(inputs.private_prefix, directory=True)
    authority = canonical(inputs.musl_authority, directory=True)
    optional_dir = canonical(inputs.optional_source_dir, directory=True)
    cargo_home = canonical(inputs.cargo_home, directory=True)
    host_toolchain = canonical(inputs.host_toolchain, directory=True)
    target = canonical(hermit / 'target', directory=True)
    source_owned = inputs.legacy_roles is None

    heads = dict(H=head_of(hermit, 'Hermit'), runtime_R=gitlink_of(hermit),
                 tool_R=head_of(tool_reverie, 'tool Reverie'))
    need(head_of(runtime_reverie, 'runtime Reverie') == heads['runtime_R'],
         'runtime Reverie checkout HEAD differs from the Hermit reverie gitlink: '
         + str(runtime_reverie) + ' (gitlink=' + heads['runtime_R'] + ')')

    check_prefix(prefix)
    native_inputs = canonical(inputs.native_inputs_json)
    check_native_inputs(native_inputs, prefix)
    for name in ('cargo', 'rustc'):
        host = canonical(host_toolchain / 'bin' / name)
        need(os.access(host, os.X_OK), 'host compiler not executable: ' + str(host))
        need(host != prefix / 'bin' / name,
             'host ' + name + ' must differ from the private compiler (owned/prepare.py)')
    inspection = check_pin(inputs.inspection_tools_json, INSPECTION_TOOLS_SHA256,
                           'inspection-tools manifest (pin owned/prepare.py)')
    musl_archive = check_pin(inputs.musl_archive, MUSL_ARCHIVE_SHA256,
                             'musl source archive (pin owned/prepare.py)')
    memcpy_source = check_pin(inputs.memcpy_source, MEMCPY_SOURCE_SHA256,
                              'memcpy source (pin owned/section/prepare.py)')
    stack_chk_source = check_pin(inputs.stack_chk_source, STACK_CHK_SOURCE_SHA256,
                                 'stack_chk source (pin owned/section/prepare.py)')
    check_authority(authority, source_owned)
    optional = find_optional(optional_dir)
    runner = canonical(hermit / 'hermit-install/private-static/run_pipeline.py')

    digests = dict(native_inputs=sha(native_inputs),
                   inspection_tools=sha(inspection), runner=sha(runner))
    for name, relative in MODULE_FILES.items():
        if name == 'package_roles_module' and not source_owned:
            continue
        digests[name] = sha(canonical(tool_reverie / relative))
    roles = (source_owned_roles(inputs.owner_sha256) if source_owned
             else legacy_roles(inputs.legacy_roles))
    paths = dict(
        H=str(hermit), runtime_R=str(runtime_reverie), tool_R=str(tool_reverie),
        cargo=str(prefix / 'bin/cargo'), rustc=str(prefix / 'bin/rustc'),
        host_cargo=str(host_toolchain / 'bin/cargo'),
        host_rustc=str(host_toolchain / 'bin/rustc'),
        target=str(target), cargo_home=str(cargo_home), runner=str(runner),
        native_inputs_json=str(native_inputs), inspection_tools_json=str(inspection),
        musl_source_archive=str(musl_archive), memcpy_source=str(memcpy_source),
        musl_source_authority=str(authority), stack_chk_source=str(stack_chk_source),
        **{name: str(path) for name, path in optional.items()})
    return dict(schema=SCHEMA, heads=heads, paths=paths, digests=digests,
                data_roles=roles)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    for name in ('hermit', 'runtime-reverie', 'tool-reverie', 'private-prefix',
                 'native-inputs-json', 'inspection-tools-json', 'musl-archive',
                 'musl-authority', 'memcpy-source', 'stack-chk-source',
                 'optional-source-dir', 'cargo-home', 'host-toolchain', 'out'):
        parser.add_argument('--' + name, required=True)
    roles = parser.add_mutually_exclusive_group(required=True)
    roles.add_argument('--owner-sha256',
                       help='source-owned roles: 64-hex owner body sha256 of __set_thread_area')
    roles.add_argument('--legacy-roles',
                       help='legacy hashed-owner roles: path to a complete data_roles JSON object')
    inputs = parser.parse_args(argv)
    config = fill(inputs)
    out = Path(inputs.out)
    need(out.is_absolute() and out.resolve() == out and not out.exists()
         and out.parent.is_dir(), 'fresh canonical --out required: ' + str(out))
    with out.open('x') as stream:
        json.dump(config, stream, indent=2, sort_keys=True)
        stream.write('\n')
    print(json.dumps(dict(config=str(out), config_sha256=sha(out), heads=config['heads'])))
    return 0


if __name__ == '__main__':
    sys.exit(main())
