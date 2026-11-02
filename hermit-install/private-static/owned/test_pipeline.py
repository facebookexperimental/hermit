"""Data-only controls of actual pipeline/role guards; no producer or guest."""
import copy
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

D = Path(__file__).resolve().parent

def load(name):
    spec = importlib.util.spec_from_file_location('test_'+name,D/(name+'.py'))
    value = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(value)
    return value

pipeline = load('pipeline')
publisher = load('publish')
section_prepare = load('section/prepare')


class OrdinaryPlacement(unittest.TestCase):
    # Synthetic archive metadata controls placement selection only. These
    # records cannot establish actual code bytes, source custody or qualification.
    SECTION = '.text.reverie_inguest_trusted_syscall'

    def fixture(self, name=SECTION, size=44, offset=0):
        archive = '/data/runtime.a'
        obj = dict(archive=archive, ordinal=7, header_offset=4096,
                   member='reverie_inguest-structural.o')
        section = dict(obj, index=2, name=name, type=1, flags=6, align=16, size=size)
        symbols = [dict(name=name, object=dict(obj), section_index=2,
                        binding=1, type=2 if index == 0 else 0, visibility=2,
                        size=44 if index == 0 else 0, value=offset + delta)
                   for index, (name, delta) in enumerate(zip(
                       section_prepare.ORDINARY_ROLES, (0, 32, 34)))]
        return [section], [dict(obj)], [(archive, [dict(obj)], symbols)], Path(archive)

    def test_exact_dedicated_and_both_historical_selectors(self):
        for name, size, offset in [(self.SECTION, 44, 0), ('.text', 177, 0),
                                   ('.text', 289, 112)]:
            fixture = self.fixture(name, size, offset)
            with self.subTest(name=name, size=size):
                selected = section_prepare.ordinary_selector(*fixture)
                self.assertEqual(selected['input_description'],
                                 '"/data/runtime.a:reverie_inguest-structural.o"(' + name + ')')
                self.assertEqual(selected['owner_offset'], offset)
                self.assertEqual(selected['section'], fixture[0][0])
                self.assertEqual(selected['roles'], fixture[2][0][2])
                self.assertEqual(selected['scope'],
                                 'PREPARATION_PLACEMENT_COORDINATES_NOT_QUALIFICATION')

    def test_dedicated_extent_name_and_offset_are_exact(self):
        for name, size, offset in [
            (self.SECTION, size, 0) for size in (0, 43, 45, 177, 289)
        ] + [
            (name, 44, 0) for name in ('.text', self.SECTION + '.extra', '.text.*')
        ] + [(self.SECTION, 44, offset) for offset in (True, -1, 1, 112)]:
            fixture = self.fixture(name, size, offset)
            if offset is True:
                fixture[2][0][2][0]['value'] = True
            with self.subTest(name=name, size=size, offset=offset), self.assertRaises(RuntimeError):
                section_prepare.ordinary_selector(*fixture)

    def test_dedicated_section_type_permissions_and_alignment_are_exact(self):
        for key, value in [('type', 8), ('flags', 2), ('flags', 7), ('align', 8), ('align', 32)]:
            fixture = self.fixture(); fixture[0][0][key] = value
            with self.subTest(key=key, value=value), self.assertRaises(RuntimeError):
                section_prepare.ordinary_selector(*fixture)

    def test_dedicated_role_typed_declarations_are_not_optional(self):
        for index in range(3):
            for key, value in [('binding', 0), ('visibility', 0),
                               ('type', 0 if index == 0 else 2), ('size', 43 if index == 0 else 1),
                               ('value', (0, 32, 34)[index] + 1)]:
                fixture = self.fixture(); fixture[2][0][2][index][key] = value
                with self.subTest(index=index, key=key), self.assertRaises(RuntimeError):
                    section_prepare.ordinary_selector(*fixture)

    def test_dedicated_role_names_must_be_unique_and_present(self):
        for index in range(3):
            for change in ('missing', 'duplicate'):
                fixture = self.fixture(); symbols = fixture[2][0][2]
                if change == 'missing': symbols.pop(index)
                else: symbols.append(copy.deepcopy(symbols[index]))
                with self.subTest(index=index, change=change), self.assertRaises(RuntimeError):
                    section_prepare.ordinary_selector(*fixture)

    def test_dedicated_foreign_definition_and_split_owner_are_refused(self):
        for index in range(3):
            fixture = self.fixture(); symbol = copy.deepcopy(fixture[2][0][2][index])
            symbol['object']['archive'] = '/data/foreign.a'
            fixture[2].append(('/data/foreign.a', [symbol['object']], [symbol]))
            with self.subTest(index=index, change='foreign'), self.assertRaises(RuntimeError):
                section_prepare.ordinary_selector(*fixture)
        for key, value in [('ordinal', 8), ('header_offset', 8192), ('member', 'other.o')]:
            fixture = self.fixture(); fixture[2][0][2][1]['object'][key] = value
            with self.subTest(key=key, change='split-owner'), self.assertRaises(RuntimeError):
                section_prepare.ordinary_selector(*fixture)
        fixture = self.fixture(); fixture[2][0][2][1]['section_index'] = 3
        with self.assertRaises(RuntimeError):
            section_prepare.ordinary_selector(*fixture)

    def test_dedicated_physical_selector_cannot_be_ambiguous(self):
        for change in ('object', 'ledger-object', 'ledger', 'section', 'same-name-section', 'member'):
            fixture = self.fixture(); rows, objects, inputs, _ = fixture
            if change == 'object': objects.append(copy.deepcopy(objects[0]))
            elif change == 'ledger-object': inputs[0][1].append(copy.deepcopy(objects[0]))
            elif change == 'ledger': inputs.append(copy.deepcopy(inputs[0]))
            elif change == 'section': rows.append(copy.deepcopy(rows[0]))
            elif change == 'same-name-section': rows.append(dict(rows[0], index=3))
            else: objects.append(dict(objects[0], ordinal=8, header_offset=8192))
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                section_prepare.ordinary_selector(*fixture)


class DataRoles(unittest.TestCase):
    def fixture(self):
        def owner(): return dict(symbol='actual_owner',offset=1,owner_sha256='a'*64)
        return dict(fallback_entry=owner(),completion_syscall=owner(),
                    sites=[dict(owner=owner(),number=number,operation=operation)
                           for operation,number in [(1,158),(2,218),(3,7),(4,2)]])
    def test_closed_conditional_data(self):
        publisher.validate_roles(self.fixture())
    def test_wrong_or_duplicate_operation_refused(self):
        for change in [('operation',3),('number',7)]:
            data=self.fixture();data['sites'][0][change[0]]=change[1]
            with self.assertRaisesRegex(RuntimeError,'source-contract roles differ'):
                publisher.validate_roles(data)
    def test_extra_coordinate_or_authority_field_refused(self):
        for value in ['entry','ready','absolute_address']:
            data=self.fixture();data[value]=True
            with self.assertRaisesRegex(RuntimeError,'unknown/missing'):
                publisher.validate_roles(data)
    def test_offset_bool_negative_or_oversized_refused(self):
        for value in [True,-1,256*1024]:
            data=self.fixture();data['fallback_entry']['offset']=value
            with self.assertRaisesRegex(RuntimeError,'offset/hash'):
                publisher.validate_roles(data)
    def test_unknown_hash_or_symbol_refused(self):
        for key,value in [('owner_sha256','A'*64),('owner_sha256','a'*63),('symbol','')]:
            data=self.fixture();data['completion_syscall'][key]=value
            with self.assertRaises(RuntimeError): publisher.validate_roles(data)
    def test_removed_native_row_refused(self):
        data=self.fixture();data['sites'].pop()
        with self.assertRaisesRegex(RuntimeError,'exactly four'):
            publisher.validate_roles(data)


class SourceOwnedPackageRoles(unittest.TestCase):
    # Synthetic DATA at the existing publisher edge. These controls do not
    # create a Rust proof token, reconstruct relocations, or qualify an image.
    SCHEMA = 'hermit.private-static-package-roles.source-owned.v1'

    def projection_fixture(self):
        # Deliberately small synthetic bodies and transaction identities, not
        # copied A04 output hashes and not serialized Rust authority.
        module = 'a' * 64
        identities = {name: dict(path='/data/attempt/' + name, sha256=digit * 64)
                      for name, digit in [('runtime_archive', 'b'), ('libc_archive', 'c'),
                                          ('artifact', 'd'), ('link_map', 'e')]}
        identities['artifact']['path'] = '/data/attempt/initializer-definitions.not-qualified.elf'
        identities['link_map']['path'] = '/data/attempt/initializer-definitions.map'
        sources = [dict(path=path, sha256=digit * 64) for path, digit in [
            ('/source/reverie/reverie-inguest/src/guest/continuation.rs', 'f'),
            ('/source/musl/src/env/__libc_start_main.c', '1'),
            ('/source/musl/src/env/__init_tls.c', '2'),
        ]]
        rows, bodies, symbols, decoded = [], {}, {}, set()
        specifications = [
            ('fallback_entry', 48, 0x1000, [6, 14, 19], [(44, 39, 0)]),
            ('__init_libc', 413, 0x2000,
             [31, 60, 106, 126, 138, 150, 157, 184, 222, 235, 330, 400],
             [(302, 7, 3), (388, 2, 4)]),
            ('__init_tp', 107, 0x3000, [8, 20, 40, 52, 80], [(44, 218, 2)]),
        ]
        for index, (name, length, entry, offsets, sites) in enumerate(specifications):
            code = bytearray(b'\x90' * length)
            for offset, number, operation in sites:
                code[offset:offset + 2] = b'\x0f\x05'
                decoded.add(entry + offset + 2)
            if name == 'fallback_entry':
                code[46:48] = b'\x0f\x0b'
            bodies[name] = code
            symbols[name] = dict(name=name, type=2, size=length, value=entry,
                                 binding=0, visibility=2, section_index=3)
            relocations = []
            for ordinal, offset in enumerate(offsets):
                row = dict(offset=offset, kind=2, addend=-4,
                           source_symbol_index=ordinal + 1, source_symbol='source_target_' + str(ordinal),
                           selected_target=0x8000 + ordinal * 16,
                           selection='map_selected_input', final_symbol_index=200 + ordinal,
                           final_symbol_value=0x8000 + ordinal * 16, final_addend=-4)
                if name == 'fallback_entry' and offset in [14, 19]:
                    row['kind'] = 4
                if name == '__init_libc' and offset in [184, 330]:
                    literal_index = 0 if offset == 184 else 1
                    row.update(source_symbol_index=18 + literal_index,
                               source_symbol='.LC' + str(literal_index),
                               selected_target=0x7000 + literal_index * 64,
                               selection='anchored_merged_literal',
                               final_symbol_index=101 + literal_index,
                               final_symbol_value=0x7000 + literal_index * 64)
                if name == '__init_libc' and offset == 235:
                    row.update(kind=4, source_symbol_index=21, source_symbol='__init_ssp',
                               selected_target=0x6000, selection='selected_strong_ssp',
                               final_symbol_index=400, final_symbol_value=0x6000)
                relocations.append(row)
            aliases = dict(fallback_completion_syscall=44, fallback_completion_return=46) if index == 0 else {}
            rows.append(dict(symbol=name, member_name='synthetic-member-' + str(index) + '.o',
                             member_ordinal=index + 1, member_header=1000 + index * 1000,
                             member_sha256=str(index + 3) * 64, input_section=index + 1,
                             input_offset=0, complete_bytes=length, input_body_sha256='6' * 64,
                             final_entry=entry, final_body_sha256=hashlib.sha256(code).hexdigest(),
                             relocations=relocations,
                             sites=[dict(offset=o, number=n, operation=k) for o, n, k in sites],
                             aliases=aliases))
        for name, offset in [('fallback_completion_syscall', 44), ('fallback_completion_return', 46)]:
            symbols[name] = dict(name=name, type=0, size=0, value=0x1000 + offset,
                                 binding=0, visibility=2, section_index=3)
        literals = []
        for index, code in enumerate([b'\0', b'/dev/null\0']):
            literals.append(dict(source_symbol='.LC' + str(index), input_symbol_index=18 + index,
                                 input_section=6, input_offset=index, bytes=len(code),
                                 sha256=hashlib.sha256(code).hexdigest(), file_symbol_index=100,
                                 final_symbol_index=101 + index, final_address=0x7000 + index * 64,
                                 final_section=7, final_file_offset=0x9000 + index * 64,
                                 anchor_symbol_index=103, anchor_address=0x2000))
        value = dict(schema='reverie.private-static-package-role-observation.v1',
                     scope='THREE_COMPLETE_SOURCE_MEMBER_RELOCATED_BODIES_AND_FINITE_SITE_IDENTITIES_ONLY',
                     implementation_sha256=module, **copy.deepcopy(identities),
                     sources=copy.deepcopy(sources), roles=rows, merged_literals=literals,
                     ssp_override=dict(source_symbol_index=21, source_section=5,
                                       selected_member_name='synthetic-selected-ssp.o',
                                       selected_member_ordinal=7, selected_member_header=7000,
                                       selected_member_sha256='7' * 64, final_entry=0x6000,
                                       complete_bytes=64))
        return value, module, identities, sources, bodies, symbols, decoded

    def projection(self, fixture):
        value, module, identities, sources = fixture[:4]
        return publisher.validate_package_role_projection(value, module, identities, sources)

    def receipt_fixture(self):
        value, module, identities, sources = self.projection_fixture()[:4]
        module_path = '/source/reverie/reverie-liteinst-artifact/src/private_static/selected_initializers/package_roles.rs'
        config = dict(package_roles=dict(schema=self.SCHEMA, module_path=module_path,
                                         module_sha256=module, sources=copy.deepcopy(sources)),
                      files={module_path: module, **{row['path']: row['sha256'] for row in sources}},
                      prepared_archive=copy.deepcopy(identities['runtime_archive']),
                      link_paths=dict(libc=identities['libc_archive']['path'],
                                      output_directory='/data/attempt'))
        receipt = dict(source_factory_close='PASS', archive_close='PASS',
                       observation={key: copy.deepcopy(identities[key])
                                    for key in ['runtime_archive', 'artifact', 'link_map']},
                       package_roles=value)
        current = {row['path']: copy.deepcopy(row) for row in list(identities.values()) + sources}
        current[module_path] = dict(path=module_path, sha256=module)
        return config, receipt, current

    def receipt(self, fixture):
        config, receipt, current = fixture
        def file_identity(path):
            if str(path) not in current:
                raise RuntimeError('DATA identity absent: ' + str(path))
            return copy.deepcopy(current[str(path)])
        return publisher.package_role_receipt(config, receipt, file_identity)

    def actual_bodies(self, fixture, permissions=None):
        value, module, identities, sources, bodies, symbols, decoded = fixture
        rows = self.projection(fixture)
        reads = []
        def symbol(name):
            if name not in symbols:
                raise RuntimeError('DATA symbol absent: ' + name)
            return symbols[name]
        def span(address, length, permission):
            reads.append((address, length, permission))
            matches = [(name, owner) for name, owner in symbols.items() if name in bodies and
                       owner['value'] <= address and address + length <= owner['value'] + len(bodies[name])]
            if len(matches) != 1 or permission != (permissions or {}).get(matches[0][0], 5):
                raise RuntimeError('DATA span lacks unique actual RX bytes')
            name, owner = matches[0]
            at = address - owner['value']
            return bytes(bodies[name][at:at + length])
        entries = publisher.package_role_bodies(rows, symbol, span, decoded)
        return entries, reads

    def roles_fixture(self, selected=True):
        def owner(symbol, offset):
            return dict(symbol=symbol, offset=offset)
        roles = dict(
            fallback_entry=owner('fallback_entry', 0),
            completion_syscall=owner('fallback_entry', 44),
            sites=[
                dict(owner=dict(owner('__set_thread_area', 13), owner_sha256=
                     '31877545d0b87f1550ee15672428f43434a76fca4134aaa12bd83afd66086470'),
                     number=158, operation=1),
                dict(owner=owner('__init_libc', 302), number=7, operation=3),
                dict(owner=owner('__init_libc', 388), number=2, operation=4),
                dict(owner=owner('__init_tp', 44), number=218, operation=2),
            ])
        if selected:
            roles['schema'] = self.SCHEMA
        else:
            digests = dict(
                fallback_entry='b4e1333f20f5b7734e660145558a10585bb24318ada5282df7ed76ec80d8fc86',
                __init_libc='51827ac09470dbb584616f66a12ce7b6a549590f72376b195b48923bcc3031c4',
                __init_tp='e47e1814d32a39e44748e3407cbc1ba6c418a07dfa98dac2efaf29b69f93a649')
            for record in [roles['fallback_entry'], roles['completion_syscall']] + [
                    row['owner'] for row in roles['sites'][1:]]:
                record['owner_sha256'] = digests[record['symbol']]
        return roles

    def test_explicit_and_legacy_role_shapes_are_disjoint_and_unchanged(self):
        for selected in [False, True]:
            roles = self.roles_fixture(selected)
            before = copy.deepcopy(roles)
            publisher.validate_roles(roles)
            self.assertIs(publisher.source_owned_roles(roles), selected)
            self.assertEqual(roles, before)
        legacy = self.roles_fixture(False)
        selected = self.roles_fixture()
        # Receipt presence cannot change selection or supply a replacement hash.
        legacy['package_roles'] = dict(schema=self.SCHEMA)
        selected['fallback_entry']['owner_sha256'] = 'a' * 64
        for roles in [legacy, selected]:
            with self.subTest(roles=roles), self.assertRaises(RuntimeError):
                publisher.source_owned_roles(roles)

    def test_selected_marker_cannot_fall_back_to_legacy_owners(self):
        cases = []
        value = self.roles_fixture(False); value['schema'] = self.SCHEMA; cases.append(value)
        value = self.roles_fixture(); value.pop('schema'); cases.append(value)
        for schema in [None, True, '', self.SCHEMA + '.unknown']:
            value = self.roles_fixture(); value['schema'] = schema; cases.append(value)
        for value in cases:
            with self.subTest(roles=value), self.assertRaises(RuntimeError):
                publisher.source_owned_roles(value)

    def test_selected_owner_names_offsets_and_operations_are_exact(self):
        for index in range(5):
            for key, value in [('symbol', 'another_owner'), ('offset', 1),
                               ('offset', True), ('offset', -1)]:
                roles = self.roles_fixture()
                owners = [roles['fallback_entry'], roles['completion_syscall']] + [
                    row['owner'] for row in roles['sites'][1:]]
                owners[index][key] = value
                with self.subTest(index=index, key=key, value=value), self.assertRaises(RuntimeError):
                    publisher.validate_roles(roles)
        for key, value in [('operation', 3), ('number', 2)]:
            roles = self.roles_fixture(); roles['sites'][3][key] = value
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                publisher.validate_roles(roles)

    def test_selected_roles_do_not_absorb_native_leaf_or_optional_child(self):
        for selected in [False, True]:
            roles = self.roles_fixture(selected)
            publisher.validate_roles(roles)
            self.assertIs(publisher.source_owned_roles(roles), selected)
            for change in ['remove_hash', 'wrong_hash_shape']:
                changed = copy.deepcopy(roles)
                owner = changed['sites'][0]['owner']
                if change == 'remove_hash':
                    owner.pop('owner_sha256')
                else:
                    owner['owner_sha256'] = 'not a digest'
                with self.subTest(selected=selected, change=change), self.assertRaises(RuntimeError):
                    publisher.validate_roles(changed)
            # Main's singleton package has exactly four native sites. A fifth
            # child-settid row remains refused even with a well-formed hash.
            roles['sites'].append(dict(owner=dict(symbol='__post_Fork', offset=29,
                                                  owner_sha256='b' * 64),
                                       number=218, operation=5))
            with self.subTest(selected=selected), self.assertRaisesRegex(RuntimeError, 'exactly four'):
                publisher.validate_roles(roles)
        roles = self.roles_fixture()
        roles['sites'][0]['owner']['symbol'] = '__init_tp'
        with self.assertRaises(RuntimeError):
            publisher.validate_roles(roles)
    def test_complete_projection_and_actual_three_bodies(self):
        fixture = self.projection_fixture()
        value = fixture[0]
        before = copy.deepcopy(value)
        rows = self.projection(fixture)
        self.assertEqual(list(rows), ['fallback_entry', '__init_libc', '__init_tp'])
        self.assertEqual(rows, {row['symbol']: row for row in value['roles']})
        # Other ordinary syscalls are still owned by the existing whole-image
        # decoder; the three-role comparison is exactly these three extents.
        fixture[6].add(0x9002)
        entries, reads = self.actual_bodies(fixture)
        self.assertEqual(entries, dict(fallback_entry=0x1000, __init_libc=0x2000, __init_tp=0x3000))
        for read in [(0x1000, 48, 5), (0x2000, 413, 5), (0x3000, 107, 5)]:
            self.assertIn(read, reads)
        self.assertEqual(value, before)

    def test_projection_missing_malformed_or_unknown_cannot_supply_authority(self):
        for replacement in [None, {}, [], True, {'schema': self.SCHEMA}]:
            fixture = list(self.projection_fixture()); fixture[0] = replacement
            with self.subTest(value=replacement), self.assertRaises(RuntimeError):
                self.projection(fixture)
        for key, replacement in [('schema', self.SCHEMA), ('scope', 'READY'),
                                 ('implementation_sha256', 'not-a-hash')]:
            fixture = self.projection_fixture(); fixture[0][key] = replacement
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                self.projection(fixture)

    def test_original_receipt_edge_requires_explicit_selection_and_projection(self):
        fixture = self.receipt_fixture()
        self.assertEqual(list(self.receipt(fixture)), ['fallback_entry', '__init_libc', '__init_tp'])
        legacy = copy.deepcopy(fixture)
        legacy[0].pop('package_roles'); legacy[1].pop('package_roles')
        self.assertIsNone(self.receipt(legacy))
        selected_without_projection = self.receipt_fixture()
        selected_without_projection[1].pop('package_roles')
        legacy_with_projection = self.receipt_fixture()
        legacy_with_projection[0].pop('package_roles')
        for value in [selected_without_projection, legacy_with_projection]:
            with self.assertRaises(RuntimeError):
                self.receipt(value)

    def test_original_receipt_edge_rechecks_sources_transactions_and_both_closes(self):
        fixture = self.receipt_fixture()
        checked_paths = [fixture[0]['package_roles']['module_path']] + [
            row['path'] for row in fixture[0]['package_roles']['sources']] + [
            fixture[0]['link_paths']['libc'],
            '/data/attempt/initializer-definitions.not-qualified.elf',
            '/data/attempt/initializer-definitions.map']
        for path in checked_paths:
            changed = self.receipt_fixture(); changed[2][path]['sha256'] = '0' * 64
            with self.subTest(path=path), self.assertRaises(RuntimeError):
                self.receipt(changed)
        for key in ['source_factory_close', 'archive_close']:
            changed = self.receipt_fixture(); changed[1][key] = 'FAIL'
            with self.subTest(close=key), self.assertRaises(RuntimeError):
                self.receipt(changed)
        for key in ['runtime_archive', 'artifact', 'link_map']:
            changed = self.receipt_fixture(); changed[1]['observation'][key]['sha256'] = '0' * 64
            with self.subTest(observation=key), self.assertRaises(RuntimeError):
                self.receipt(changed)

    def test_projection_serialized_size_bound_is_not_a_per_field_allowance(self):
        fixture = self.projection_fixture()
        # Every changed string individually fits the bounded-name contract;
        # the complete observation exceeds the separate 64 KiB receipt bound.
        for row in fixture[0]['roles'][1]['relocations'][:5]:
            row['source_symbol'] = 'x' * 16384
        self.assertGreater(len(json.dumps(fixture[0]).encode()), 65536)
        with self.assertRaisesRegex(RuntimeError, 'bound'):
            self.projection(fixture)

    def test_projection_requires_exact_order_and_complete_unique_role_set(self):
        for change in ['missing', 'extra', 'duplicate', 'reordered', 'renamed']:
            fixture = self.projection_fixture(); rows = fixture[0]['roles']
            if change == 'missing': rows.pop()
            elif change == 'extra': rows.append(copy.deepcopy(rows[0]))
            elif change == 'duplicate': rows[2] = copy.deepcopy(rows[1])
            elif change == 'reordered': rows.reverse()
            else: rows[0]['symbol'] = 'another_function'
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                self.projection(fixture)

    def test_projection_module_and_all_transaction_identities_are_independent(self):
        fixture = list(self.projection_fixture()); fixture[1] = '0' * 64
        with self.assertRaises(RuntimeError):
            self.projection(fixture)
        for name in ['runtime_archive', 'libc_archive', 'artifact', 'link_map']:
            for key, replacement in [('path', '/data/other-attempt/' + name), ('sha256', '0' * 64)]:
                fixture = self.projection_fixture(); fixture[0][name][key] = replacement
                with self.subTest(name=name, key=key), self.assertRaises(RuntimeError):
                    self.projection(fixture)

    def test_projection_sources_require_exact_paths_hashes_count_and_order(self):
        for index in range(3):
            for key, replacement in [('path', '/source/foreign.c'), ('sha256', '0' * 64)]:
                fixture = self.projection_fixture(); fixture[0]['sources'][index][key] = replacement
                with self.subTest(index=index, key=key), self.assertRaises(RuntimeError):
                    self.projection(fixture)
        for change in ['missing', 'extra', 'reordered']:
            fixture = self.projection_fixture(); rows = fixture[0]['sources']
            if change == 'missing': rows.pop()
            elif change == 'extra': rows.append(copy.deepcopy(rows[0]))
            else: rows.reverse()
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                self.projection(fixture)

    def test_projection_all_nested_records_have_closed_shapes(self):
        selectors = [lambda v: v, lambda v: v['artifact'], lambda v: v['sources'][0],
                     lambda v: v['roles'][0], lambda v: v['roles'][0]['relocations'][0],
                     lambda v: v['roles'][0]['sites'][0], lambda v: v['merged_literals'][0],
                     lambda v: v['ssp_override']]
        for index, select in enumerate(selectors):
            fixture = self.projection_fixture(); select(fixture[0])['ready'] = True
            with self.subTest(record=index), self.assertRaises(RuntimeError):
                self.projection(fixture)

    def test_projection_extents_sites_aliases_and_integer_types_are_exact(self):
        changes = [
            (lambda v: v['roles'][0], 'complete_bytes', 47),
            (lambda v: v['roles'][1], 'complete_bytes', 414),
            (lambda v: v['roles'][2], 'complete_bytes', 106),
            (lambda v: v['roles'][0], 'input_offset', 1),
            (lambda v: v['roles'][0], 'final_entry', True),
            (lambda v: v['roles'][0], 'member_ordinal', True),
            (lambda v: v['roles'][0], 'member_header', -1),
            (lambda v: v['roles'][0], 'final_body_sha256', 'g' * 64),
            (lambda v: v['roles'][0]['aliases'], 'fallback_completion_return', 47),
            (lambda v: v['roles'][1]['aliases'], 'unexpected_alias', 0),
        ]
        for index, (select, key, replacement) in enumerate(changes):
            fixture = self.projection_fixture(); select(fixture[0])[key] = replacement
            with self.subTest(change=index), self.assertRaises(RuntimeError):
                self.projection(fixture)
        for role in range(3):
            for key, replacement in [('offset', 1), ('number', 1), ('operation', 99)]:
                fixture = self.projection_fixture()
                fixture[0]['roles'][role]['sites'][0][key] = replacement
                with self.subTest(role=role, key=key), self.assertRaises(RuntimeError):
                    self.projection(fixture)

    def test_projection_relocation_inventory_and_fallback_contract_are_exact(self):
        for role in range(3):
            for change in ['missing', 'extra', 'reordered', 'offset']:
                fixture = self.projection_fixture(); rows = fixture[0]['roles'][role]['relocations']
                if change == 'missing': rows.pop()
                elif change == 'extra': rows.append(copy.deepcopy(rows[0]))
                elif change == 'reordered': rows.reverse()
                else: rows[0]['offset'] += 1
                with self.subTest(role=role, change=change), self.assertRaises(RuntimeError):
                    self.projection(fixture)
        for key, replacement in [('kind', 4), ('addend', 0), ('source_symbol_index', True),
                                 ('selection', 'unqualified_diagnostic'), ('selected_target', -1)]:
            fixture = self.projection_fixture(); fixture[0]['roles'][0]['relocations'][0][key] = replacement
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                self.projection(fixture)

    def test_projection_literal_and_ssp_shapes_cannot_be_relabelled(self):
        for key, replacement in [('source_symbol', '.LC2'), ('input_section', 7),
                                 ('input_offset', 1), ('bytes', 2), ('sha256', '0' * 64),
                                 ('final_symbol_index', True), ('final_address', -1)]:
            fixture = self.projection_fixture(); fixture[0]['merged_literals'][0][key] = replacement
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                self.projection(fixture)
        for change in ['missing', 'extra', 'reordered']:
            fixture = self.projection_fixture(); rows = fixture[0]['merged_literals']
            if change == 'missing': rows.pop()
            elif change == 'extra': rows.append(copy.deepcopy(rows[0]))
            else: rows.reverse()
            with self.subTest(change=change), self.assertRaises(RuntimeError):
                self.projection(fixture)
        for key, replacement in [('source_section', 4), ('selected_member_header', -1),
                                 ('selected_member_sha256', 'bad'), ('complete_bytes', 0)]:
            fixture = self.projection_fixture(); fixture[0]['ssp_override'][key] = replacement
            with self.subTest(key=key), self.assertRaises(RuntimeError):
                self.projection(fixture)

    def test_complete_body_comparison_refuses_non_site_byte_mutations(self):
        for name in ['fallback_entry', '__init_libc', '__init_tp']:
            for offset in [0, -1]:
                fixture = self.projection_fixture(); fixture[4][name][offset] ^= 1
                with self.subTest(name=name, offset=offset), self.assertRaises(RuntimeError):
                    self.actual_bodies(fixture)

    def test_actual_body_name_type_binding_visibility_extent_and_address_are_bound(self):
        for name in ['fallback_entry', '__init_libc', '__init_tp']:
            for key, replacement in [('name', 'foreign_function'), ('type', 1), ('binding', 1),
                                     ('visibility', 0), ('size', 0), ('size', True), ('value', 1)]:
                fixture = self.projection_fixture(); fixture[5][name][key] = replacement
                with self.subTest(name=name, key=key), self.assertRaises(RuntimeError):
                    self.actual_bodies(fixture)

    def test_complete_decoder_set_refuses_missing_extra_and_moved_syscalls(self):
        for name, offset in [('fallback_entry', 44), ('__init_libc', 302), ('__init_tp', 44)]:
            for change in ['missing', 'extra', 'moved']:
                fixture = self.projection_fixture(); entry = fixture[5][name]['value']
                if change in ['missing', 'moved']:
                    fixture[6].remove(entry + offset + 2)
                if change in ['extra', 'moved']:
                    # The added location contains real syscall bytes; updating
                    # the body observation cannot excuse an unrequested site.
                    fixture[4][name][24:26] = b'\x0f\x05'
                    fixture[6].add(entry + 26)
                    row = next(row for row in fixture[0]['roles'] if row['symbol'] == name)
                    row['final_body_sha256'] = hashlib.sha256(fixture[4][name]).hexdigest()
                with self.subTest(name=name, change=change), self.assertRaises(RuntimeError):
                    self.actual_bodies(fixture)

    def test_actual_fallback_alias_and_rx_mapping_cannot_be_substituted(self):
        for name in ['fallback_completion_syscall', 'fallback_completion_return']:
            for change in ['missing', 'moved']:
                fixture = self.projection_fixture()
                if change == 'missing': fixture[5].pop(name)
                else: fixture[5][name]['value'] += 1
                with self.subTest(name=name, change=change), self.assertRaises(RuntimeError):
                    self.actual_bodies(fixture)
        for name in ['fallback_entry', '__init_libc', '__init_tp']:
            with self.subTest(name=name), self.assertRaises(RuntimeError):
                self.actual_bodies(self.projection_fixture(), {name: 6})



class SourceCustody(unittest.TestCase):
    def fixture(self,directory):
        root=Path(directory);module=root/'unit.py';module.write_text('retained original bytes\n')
        manifest=root/'source-files.json'
        manifest.write_text(json.dumps([dict(path='unit.py',sha256=pipeline.sha(module))]))
        return root,module,manifest
    def test_actual_producer_source_guard_changed_module(self):
        with tempfile.TemporaryDirectory() as directory:
            root,module,manifest=self.fixture(directory);previous=pipeline.D
            try:
                pipeline.D=root;expected=pipeline.sha(manifest)
                pipeline.source_files(expected)
                module.write_text('changed bytes\n')
                with self.assertRaisesRegex(RuntimeError,'producer source changed'):
                    pipeline.source_files(expected)
            finally: pipeline.D=previous
    def test_actual_producer_source_guard_changed_manifest(self):
        with tempfile.TemporaryDirectory() as directory:
            root,module,manifest=self.fixture(directory);previous=pipeline.D
            try:
                pipeline.D=root;expected=pipeline.sha(manifest)
                manifest.write_text('[]')
                with self.assertRaisesRegex(RuntimeError,'manifest differs'):
                    pipeline.source_files(expected)
            finally: pipeline.D=previous
    def test_path_escape_refused(self):
        with tempfile.TemporaryDirectory() as directory:
            root,module,manifest=self.fixture(directory);previous=pipeline.D
            try:
                pipeline.D=root
                manifest.write_text(json.dumps([dict(path='../unit.py',sha256='a'*64)]))
                with self.assertRaisesRegex(RuntimeError,'path escape'):
                    pipeline.source_files(pipeline.sha(manifest))
            finally: pipeline.D=previous
    def test_unknown_config_shape_refused(self):
        with self.assertRaisesRegex(RuntimeError,'config shape'):
            pipeline.validate_config(dict(schema='hermit.private-static-producer-inputs.v1',ready=True))


class AttemptIsolation(unittest.TestCase):
    def test_stage_helper_names_never_reuse_another_attempt(self):
        roots = [Path('/data/run-a'), Path('/data/run-b'), Path('/other/run-a')]
        names = [pipeline.stage_output(root, stage).name
                 for root in roots for stage in ('archive', 'owned')]
        self.assertEqual(len(set(names)), 6)
        for root in roots:
            for stage in ('archive', 'owned'):
                self.assertEqual(pipeline.stage_output(root, stage).parent, root)
    def test_unknown_stage_refuses(self):
        with self.assertRaisesRegex(RuntimeError, 'unknown pipeline stage'):
            pipeline.stage_output(Path('/data/run-a'), '../escape')


if __name__ == '__main__':
    unittest.main()
