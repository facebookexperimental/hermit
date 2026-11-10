"""Source association derived from the chosen heads' Git objects only.

The admitted rows are read from H_head and the Reverie commit its gitlink
records, never from the tree being built; the tree is then compared with them.
Full chosen-head custody and Rust proofs remain mandatory.
"""
from contextlib import ExitStack
import hashlib
import os
from pathlib import Path
import re
import stat
import subprocess

# Historical reviewed cohorts, retained as fixtures. They were projected from
# accepted source objects; since dh-gygs2r none of them admits a tree.
HISTORICAL = (('H', 'detcore-liteinst/build.rs', 2881, '9a2552ea4d2ba0190a9fc06244102ff95fd1902830369b90c0e891d7373c9633'), ('H', 'detcore-liteinst/src/exec_initial.rs', 27550, '5bcc17e7fc3eeb7d7131dce4e5e2df688e94d2b7ce51d20b619796f40e1624e2'), ('H', 'detcore-liteinst/src/exec_initial/mapped_entry.rs', 6707, '34e21f21a41f2346889c242c869bc32c74821c625dd5b9cf9bd2181ca2d8d40e'), ('H', 'detcore-liteinst/src/exec_initial/mapped_entry/bounds.rs', 1439, '4ab8a19e17b52905fea0176b45c63c48e5fc581c9977c927b8817c4de129f1e0'), ('H', 'detcore-liteinst/src/exec_native/catalog.rs', 10932, '3ef894a3325cd56a172fb67809d3b39109db839a916a6f3c6fcf927ec25c7ff7'), ('H', 'detcore-liteinst/src/exec_native/consumer.rs', 8232, '292f0a0df98d2dc6588f2af6976c79932423f44df84166ae158e9b4a7fb36b4d'), ('H', 'detcore-liteinst/src/exec_native/consumer_tests.rs', 21835, 'e3f0f107556e7261e5e5f80a8a2b4d1fbec38572de95df5df13363c479299fab'), ('H', 'detcore-liteinst/src/exec_native/driver.rs', 32420, 'bc0626efe96659f01fb6bb921cc61ef8bb76833522b45ba114d97888e8bb2975'), ('H', 'detcore-liteinst/src/exec_native/entry.rs', 17706, 'c9fe2808730baba77dd3710403d6f7f0eab0892b9bf6ae98fb9ea817eda94ca0'), ('H', 'detcore-liteinst/src/exec_native/mod.rs', 296, '25d35c840dd19367fe4ff54553972dc949b76dc4c6dfdb64c41e9657552724d1'), ('H', 'detcore-liteinst/src/exec_native/provider.rs', 19017, '12a6f762a00d460c06d03f143587124c8cc9c615725f8dce152282433f02ff1e'), ('H', 'detcore-liteinst/src/lib.rs', 29830, 'f4f4cc82a8565db4e7b4c8c13b7199ac97995925623ffa32d0ee5b3e1ac1b016'), ('R', 'reverie-inguest/Cargo.toml', 5569, '0c19352a33f158b91d1b37de71d5d8ef814a29be4599b95cee30ff0116e51176'), ('R', 'reverie-inguest/src/guest/exec_private.rs', 180910, '96005c0878ea34fd94109465db84ad0c66dd2c855c235c4160ce5a76afa86948'), ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial.rs', 37162, 'a28b8e9b8a8b3efdc70f39e11253a5525aff36c315d33cd3fbec99f0489845ad'), ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay.rs', 41384, 'b3101215bb483aff4b82e80063b8e0a5419ce3f0fc3ba39c3e84675b8feafc26'), ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay/frontend.rs', 16799, 'e1afa8d4a09a04efd342526652690a120644747782eeee968c23dbf1a8e5cc8d'), ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/mapped_scalar.rs', 20465, '285f7f75bf9f388a771d685607ce65f7097c27d21869952d46b3a7ce70897612'), ('R', 'reverie-inguest/src/lib.rs', 11734, '0fbf94503eac27b65e52967c11c675e23e7a2f50338ca32150002ff4c64fabbf'), ('R', 'reverie-inguest/src/private_runtime/mod.rs', 2093, 'f6764939d2016bd3c9414b17b35f21f8cf7217f128135b83cfb5459aa3ae5e1e'), ('R', 'reverie-inguest/src/private_runtime/tls/abi.rs', 12171, 'f7de3cd3a0653785f8aacc01e0fabdeb89172b2660f07ea0df9d393078299aab'), ('R', 'reverie-inguest/src/private_runtime/tls/layout.rs', 499, '8fcceedb234c081b5125d95d73eaafa083fadc2d1be41698b724487ab7cfae36'), ('R', 'reverie-inguest/src/private_runtime/tls/mod.rs', 4853, '369e35bcafda51a271720507dcf7a7c9072fb5db7486e110e14f15e4df91f0dd'), ('R', 'reverie-inguest/src/trap.rs', 49799, '5c9249419574f00c540dec767359181c8ac03a47aa2df0f19e5cef0a59b5755b'), ('R', 'reverie-inguest/src/trap/fixed_gates.rs', 36554, 'ff3f6f6c46e872734ae921e869dbccc0ce1aeefe4bc3af8f29363cf276501e54'), ('R', 'reverie-inguest/src/trap/frame.rs', 29421, '9fa094343c6a19142da3cc1622c1b390d0ece26eb0f8eb4902d1ba9f9cd69aba'), ('R', 'reverie-loader/Cargo.toml', 343, 'f407fe1dcf3aa4829fe6b0257546cdf413e2917e5161281a14d4477f5f4c48d9'), ('R', 'reverie-loader/src/handoff.rs', 76132, '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'), ('R', 'reverie-loader/src/lib.rs', 1510, 'b3318334635e632a23283c1f51633b090eb809de9710ab0c98038b684abbb5b8'), ('R', 'reverie-loader/src/mapped_inputs.rs', 17803, '81cd7474215ff617fb1bc16b236461d4deb33697c7d559130f4bdb98b0f0e2c2'), ('R', 'reverie-loader/src/musl_tls.rs', 7478, '84011bb8505fa3357908e7445b1305f54303bae32ddb9d393546da8e309ee1ea'))
# Complete candidate content tuple, independently reviewed with all historical rows retained.
CURRENT_MERGED = (
    ('H', 'detcore-liteinst/build.rs', 2881, '9a2552ea4d2ba0190a9fc06244102ff95fd1902830369b90c0e891d7373c9633'),
    ('H', 'detcore-liteinst/src/exec_initial.rs', 30450, '693667e8ae6efe87f9fa5810a61dbeffe3306ad89157aea9111b4b653a2236f8'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry.rs', 6707, '34e21f21a41f2346889c242c869bc32c74821c625dd5b9cf9bd2181ca2d8d40e'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry/bounds.rs', 1439, '4ab8a19e17b52905fea0176b45c63c48e5fc581c9977c927b8817c4de129f1e0'),
    ('H', 'detcore-liteinst/src/exec_native/catalog.rs', 10932, '3ef894a3325cd56a172fb67809d3b39109db839a916a6f3c6fcf927ec25c7ff7'),
    ('H', 'detcore-liteinst/src/exec_native/consumer.rs', 8232, '292f0a0df98d2dc6588f2af6976c79932423f44df84166ae158e9b4a7fb36b4d'),
    ('H', 'detcore-liteinst/src/exec_native/consumer_tests.rs', 21835, 'e3f0f107556e7261e5e5f80a8a2b4d1fbec38572de95df5df13363c479299fab'),
    ('H', 'detcore-liteinst/src/exec_native/driver.rs', 32420, 'bc0626efe96659f01fb6bb921cc61ef8bb76833522b45ba114d97888e8bb2975'),
    ('H', 'detcore-liteinst/src/exec_native/entry.rs', 17706, 'c9fe2808730baba77dd3710403d6f7f0eab0892b9bf6ae98fb9ea817eda94ca0'),
    ('H', 'detcore-liteinst/src/exec_native/mod.rs', 296, '25d35c840dd19367fe4ff54553972dc949b76dc4c6dfdb64c41e9657552724d1'),
    ('H', 'detcore-liteinst/src/exec_native/provider.rs', 19017, '12a6f762a00d460c06d03f143587124c8cc9c615725f8dce152282433f02ff1e'),
    ('H', 'detcore-liteinst/src/lib.rs', 30423, 'd2f4aadb0f86c70ee0c4f60204f5a877f501a0683657d7fc466cbc3aba19bd72'),
    ('R', 'reverie-inguest/Cargo.toml', 5569, '0c19352a33f158b91d1b37de71d5d8ef814a29be4599b95cee30ff0116e51176'),
    ('R', 'reverie-inguest/src/guest/exec_private.rs', 182321, '026cecaff9850b3bf916bc0ecb56d0e9021ca5130e7bfc1f64e21a48bc0dd4a9'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial.rs', 37162, 'a28b8e9b8a8b3efdc70f39e11253a5525aff36c315d33cd3fbec99f0489845ad'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay.rs', 50822, 'a7a4df88f1fd44855a0ccf7c60ca03dedb8c7e336c6239549484ee038fbc9547'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay/frontend.rs', 18922, '28a785bd1945b7aae1e72bc688da1918368ab5e2fb1a4d9927bf00be5e01fcb4'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/mapped_scalar.rs', 20465, '285f7f75bf9f388a771d685607ce65f7097c27d21869952d46b3a7ce70897612'),
    ('R', 'reverie-inguest/src/lib.rs', 11734, '0fbf94503eac27b65e52967c11c675e23e7a2f50338ca32150002ff4c64fabbf'),
    ('R', 'reverie-inguest/src/private_runtime/mod.rs', 2093, 'f6764939d2016bd3c9414b17b35f21f8cf7217f128135b83cfb5459aa3ae5e1e'),
    ('R', 'reverie-inguest/src/private_runtime/tls/abi.rs', 12171, 'f7de3cd3a0653785f8aacc01e0fabdeb89172b2660f07ea0df9d393078299aab'),
    ('R', 'reverie-inguest/src/private_runtime/tls/layout.rs', 499, '8fcceedb234c081b5125d95d73eaafa083fadc2d1be41698b724487ab7cfae36'),
    ('R', 'reverie-inguest/src/private_runtime/tls/mod.rs', 4853, '369e35bcafda51a271720507dcf7a7c9072fb5db7486e110e14f15e4df91f0dd'),
    ('R', 'reverie-inguest/src/trap.rs', 49799, '5c9249419574f00c540dec767359181c8ac03a47aa2df0f19e5cef0a59b5755b'),
    ('R', 'reverie-inguest/src/trap/fixed_gates.rs', 36554, 'ff3f6f6c46e872734ae921e869dbccc0ce1aeefe4bc3af8f29363cf276501e54'),
    ('R', 'reverie-inguest/src/trap/frame.rs', 35075, '357d2d884f7fdcc43b8d4acde8bc04e43023c85e6a77c569ac60c64dbb6030a0'),
    ('R', 'reverie-loader/Cargo.toml', 343, 'f407fe1dcf3aa4829fe6b0257546cdf413e2917e5161281a14d4477f5f4c48d9'),
    ('R', 'reverie-loader/src/handoff.rs', 76132, '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'),
    ('R', 'reverie-loader/src/lib.rs', 1510, 'b3318334635e632a23283c1f51633b090eb809de9710ab0c98038b684abbb5b8'),
    ('R', 'reverie-loader/src/mapped_inputs.rs', 17803, '81cd7474215ff617fb1bc16b236461d4deb33697c7d559130f4bdb98b0f0e2c2'),
    ('R', 'reverie-loader/src/musl_tls.rs', 7478, '84011bb8505fa3357908e7445b1305f54303bae32ddb9d393546da8e309ee1ea'),
)
# Complete declared final-source DATA tuple (H63dada / R30db); source association only.
FINAL_CANDIDATE_V1 = (
    ('H', 'detcore-liteinst/build.rs', 2881, '9a2552ea4d2ba0190a9fc06244102ff95fd1902830369b90c0e891d7373c9633'),
    ('H', 'detcore-liteinst/src/exec_initial.rs', 30450, '693667e8ae6efe87f9fa5810a61dbeffe3306ad89157aea9111b4b653a2236f8'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry.rs', 6707, '34e21f21a41f2346889c242c869bc32c74821c625dd5b9cf9bd2181ca2d8d40e'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry/bounds.rs', 1439, '4ab8a19e17b52905fea0176b45c63c48e5fc581c9977c927b8817c4de129f1e0'),
    ('H', 'detcore-liteinst/src/exec_native/catalog.rs', 10932, '3ef894a3325cd56a172fb67809d3b39109db839a916a6f3c6fcf927ec25c7ff7'),
    ('H', 'detcore-liteinst/src/exec_native/consumer.rs', 8232, '292f0a0df98d2dc6588f2af6976c79932423f44df84166ae158e9b4a7fb36b4d'),
    ('H', 'detcore-liteinst/src/exec_native/consumer_tests.rs', 21835, 'e3f0f107556e7261e5e5f80a8a2b4d1fbec38572de95df5df13363c479299fab'),
    ('H', 'detcore-liteinst/src/exec_native/driver.rs', 32420, 'bc0626efe96659f01fb6bb921cc61ef8bb76833522b45ba114d97888e8bb2975'),
    ('H', 'detcore-liteinst/src/exec_native/entry.rs', 17706, 'c9fe2808730baba77dd3710403d6f7f0eab0892b9bf6ae98fb9ea817eda94ca0'),
    ('H', 'detcore-liteinst/src/exec_native/mod.rs', 296, '25d35c840dd19367fe4ff54553972dc949b76dc4c6dfdb64c41e9657552724d1'),
    ('H', 'detcore-liteinst/src/exec_native/provider.rs', 19017, '12a6f762a00d460c06d03f143587124c8cc9c615725f8dce152282433f02ff1e'),
    ('H', 'detcore-liteinst/src/lib.rs', 30461, '61fa5f040e0188ec076c10f3a3260acb722257aa60a595ed815a158365fd5af3'),
    ('R', 'reverie-inguest/Cargo.toml', 5569, '0c19352a33f158b91d1b37de71d5d8ef814a29be4599b95cee30ff0116e51176'),
    ('R', 'reverie-inguest/src/guest/exec_private.rs', 182321, '026cecaff9850b3bf916bc0ecb56d0e9021ca5130e7bfc1f64e21a48bc0dd4a9'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial.rs', 37162, 'a28b8e9b8a8b3efdc70f39e11253a5525aff36c315d33cd3fbec99f0489845ad'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay.rs', 50822, 'a7a4df88f1fd44855a0ccf7c60ca03dedb8c7e336c6239549484ee038fbc9547'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay/frontend.rs', 18922, '28a785bd1945b7aae1e72bc688da1918368ab5e2fb1a4d9927bf00be5e01fcb4'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/mapped_scalar.rs', 20465, '285f7f75bf9f388a771d685607ce65f7097c27d21869952d46b3a7ce70897612'),
    ('R', 'reverie-inguest/src/lib.rs', 11734, '0fbf94503eac27b65e52967c11c675e23e7a2f50338ca32150002ff4c64fabbf'),
    ('R', 'reverie-inguest/src/private_runtime/mod.rs', 2093, 'f6764939d2016bd3c9414b17b35f21f8cf7217f128135b83cfb5459aa3ae5e1e'),
    ('R', 'reverie-inguest/src/private_runtime/tls/abi.rs', 12171, 'f7de3cd3a0653785f8aacc01e0fabdeb89172b2660f07ea0df9d393078299aab'),
    ('R', 'reverie-inguest/src/private_runtime/tls/layout.rs', 499, '8fcceedb234c081b5125d95d73eaafa083fadc2d1be41698b724487ab7cfae36'),
    ('R', 'reverie-inguest/src/private_runtime/tls/mod.rs', 4853, '369e35bcafda51a271720507dcf7a7c9072fb5db7486e110e14f15e4df91f0dd'),
    ('R', 'reverie-inguest/src/trap.rs', 49799, '5c9249419574f00c540dec767359181c8ac03a47aa2df0f19e5cef0a59b5755b'),
    ('R', 'reverie-inguest/src/trap/fixed_gates.rs', 36554, 'ff3f6f6c46e872734ae921e869dbccc0ce1aeefe4bc3af8f29363cf276501e54'),
    ('R', 'reverie-inguest/src/trap/frame.rs', 35075, '357d2d884f7fdcc43b8d4acde8bc04e43023c85e6a77c569ac60c64dbb6030a0'),
    ('R', 'reverie-loader/Cargo.toml', 343, 'f407fe1dcf3aa4829fe6b0257546cdf413e2917e5161281a14d4477f5f4c48d9'),
    ('R', 'reverie-loader/src/handoff.rs', 76132, '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'),
    ('R', 'reverie-loader/src/lib.rs', 1510, 'b3318334635e632a23283c1f51633b090eb809de9710ab0c98038b684abbb5b8'),
    ('R', 'reverie-loader/src/mapped_inputs.rs', 17803, '81cd7474215ff617fb1bc16b236461d4deb33697c7d559130f4bdb98b0f0e2c2'),
    ('R', 'reverie-loader/src/musl_tls.rs', 7478, '84011bb8505fa3357908e7445b1305f54303bae32ddb9d393546da8e309ee1ea'),
)
# Exact 0.5.1 manifest union; source association only. The three older tuples
# remain unchanged. Actual linked-image and guest evidence is required separately.
FINAL_DEVELOPMENT_051 = (
    ('H', 'detcore-liteinst/build.rs', 2881, '9a2552ea4d2ba0190a9fc06244102ff95fd1902830369b90c0e891d7373c9633'),
    ('H', 'detcore-liteinst/src/exec_initial.rs', 30450, '693667e8ae6efe87f9fa5810a61dbeffe3306ad89157aea9111b4b653a2236f8'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry.rs', 6707, '34e21f21a41f2346889c242c869bc32c74821c625dd5b9cf9bd2181ca2d8d40e'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry/bounds.rs', 1439, '4ab8a19e17b52905fea0176b45c63c48e5fc581c9977c927b8817c4de129f1e0'),
    ('H', 'detcore-liteinst/src/exec_native/catalog.rs', 10932, '3ef894a3325cd56a172fb67809d3b39109db839a916a6f3c6fcf927ec25c7ff7'),
    ('H', 'detcore-liteinst/src/exec_native/consumer.rs', 8232, '292f0a0df98d2dc6588f2af6976c79932423f44df84166ae158e9b4a7fb36b4d'),
    ('H', 'detcore-liteinst/src/exec_native/consumer_tests.rs', 21835, 'e3f0f107556e7261e5e5f80a8a2b4d1fbec38572de95df5df13363c479299fab'),
    ('H', 'detcore-liteinst/src/exec_native/driver.rs', 32420, 'bc0626efe96659f01fb6bb921cc61ef8bb76833522b45ba114d97888e8bb2975'),
    ('H', 'detcore-liteinst/src/exec_native/entry.rs', 17706, 'c9fe2808730baba77dd3710403d6f7f0eab0892b9bf6ae98fb9ea817eda94ca0'),
    ('H', 'detcore-liteinst/src/exec_native/mod.rs', 296, '25d35c840dd19367fe4ff54553972dc949b76dc4c6dfdb64c41e9657552724d1'),
    ('H', 'detcore-liteinst/src/exec_native/provider.rs', 19017, '12a6f762a00d460c06d03f143587124c8cc9c615725f8dce152282433f02ff1e'),
    ('H', 'detcore-liteinst/src/lib.rs', 30461, '61fa5f040e0188ec076c10f3a3260acb722257aa60a595ed815a158365fd5af3'),
    ('R', 'reverie-inguest/Cargo.toml', 5569, '71e2a295a5f59e3c129247c91b9beb84d19a07b0148153c884449af37d082aa7'),
    ('R', 'reverie-inguest/src/guest/exec_private.rs', 182321, '026cecaff9850b3bf916bc0ecb56d0e9021ca5130e7bfc1f64e21a48bc0dd4a9'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial.rs', 37162, 'a28b8e9b8a8b3efdc70f39e11253a5525aff36c315d33cd3fbec99f0489845ad'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay.rs', 50822, 'a7a4df88f1fd44855a0ccf7c60ca03dedb8c7e336c6239549484ee038fbc9547'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay/frontend.rs', 18922, '28a785bd1945b7aae1e72bc688da1918368ab5e2fb1a4d9927bf00be5e01fcb4'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/mapped_scalar.rs', 20465, '285f7f75bf9f388a771d685607ce65f7097c27d21869952d46b3a7ce70897612'),
    ('R', 'reverie-inguest/src/lib.rs', 11734, '0fbf94503eac27b65e52967c11c675e23e7a2f50338ca32150002ff4c64fabbf'),
    ('R', 'reverie-inguest/src/private_runtime/mod.rs', 2093, 'f6764939d2016bd3c9414b17b35f21f8cf7217f128135b83cfb5459aa3ae5e1e'),
    ('R', 'reverie-inguest/src/private_runtime/tls/abi.rs', 12171, 'f7de3cd3a0653785f8aacc01e0fabdeb89172b2660f07ea0df9d393078299aab'),
    ('R', 'reverie-inguest/src/private_runtime/tls/layout.rs', 499, '8fcceedb234c081b5125d95d73eaafa083fadc2d1be41698b724487ab7cfae36'),
    ('R', 'reverie-inguest/src/private_runtime/tls/mod.rs', 4853, '369e35bcafda51a271720507dcf7a7c9072fb5db7486e110e14f15e4df91f0dd'),
    ('R', 'reverie-inguest/src/trap.rs', 49799, '5c9249419574f00c540dec767359181c8ac03a47aa2df0f19e5cef0a59b5755b'),
    ('R', 'reverie-inguest/src/trap/fixed_gates.rs', 36554, 'ff3f6f6c46e872734ae921e869dbccc0ce1aeefe4bc3af8f29363cf276501e54'),
    ('R', 'reverie-inguest/src/trap/frame.rs', 35075, '357d2d884f7fdcc43b8d4acde8bc04e43023c85e6a77c569ac60c64dbb6030a0'),
    ('R', 'reverie-loader/Cargo.toml', 343, '781863e2fb39d08ba64776020dc859abe6b68024f4c4219dc6fb96f4b814b203'),
    ('R', 'reverie-loader/src/handoff.rs', 76132, '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'),
    ('R', 'reverie-loader/src/lib.rs', 1510, 'b3318334635e632a23283c1f51633b090eb809de9710ab0c98038b684abbb5b8'),
    ('R', 'reverie-loader/src/mapped_inputs.rs', 17803, '81cd7474215ff617fb1bc16b236461d4deb33697c7d559130f4bdb98b0f0e2c2'),
    ('R', 'reverie-loader/src/musl_tls.rs', 7478, '84011bb8505fa3357908e7445b1305f54303bae32ddb9d393546da8e309ee1ea'),
)
# Exact 0.5.4 manifest union; source association only. All older tuples
# and complete-file guards remain unchanged; runtime evidence is separate.
FINAL_DEVELOPMENT_054 = (
    ('H', 'detcore-liteinst/build.rs', 2881, '9a2552ea4d2ba0190a9fc06244102ff95fd1902830369b90c0e891d7373c9633'),
    ('H', 'detcore-liteinst/src/exec_initial.rs', 30450, '693667e8ae6efe87f9fa5810a61dbeffe3306ad89157aea9111b4b653a2236f8'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry.rs', 6707, '34e21f21a41f2346889c242c869bc32c74821c625dd5b9cf9bd2181ca2d8d40e'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry/bounds.rs', 1439, '4ab8a19e17b52905fea0176b45c63c48e5fc581c9977c927b8817c4de129f1e0'),
    ('H', 'detcore-liteinst/src/exec_native/catalog.rs', 10932, '3ef894a3325cd56a172fb67809d3b39109db839a916a6f3c6fcf927ec25c7ff7'),
    ('H', 'detcore-liteinst/src/exec_native/consumer.rs', 8232, '292f0a0df98d2dc6588f2af6976c79932423f44df84166ae158e9b4a7fb36b4d'),
    ('H', 'detcore-liteinst/src/exec_native/consumer_tests.rs', 21835, 'e3f0f107556e7261e5e5f80a8a2b4d1fbec38572de95df5df13363c479299fab'),
    ('H', 'detcore-liteinst/src/exec_native/driver.rs', 32420, 'bc0626efe96659f01fb6bb921cc61ef8bb76833522b45ba114d97888e8bb2975'),
    ('H', 'detcore-liteinst/src/exec_native/entry.rs', 17706, 'c9fe2808730baba77dd3710403d6f7f0eab0892b9bf6ae98fb9ea817eda94ca0'),
    ('H', 'detcore-liteinst/src/exec_native/mod.rs', 296, '25d35c840dd19367fe4ff54553972dc949b76dc4c6dfdb64c41e9657552724d1'),
    ('H', 'detcore-liteinst/src/exec_native/provider.rs', 19017, '12a6f762a00d460c06d03f143587124c8cc9c615725f8dce152282433f02ff1e'),
    ('H', 'detcore-liteinst/src/lib.rs', 30461, '61fa5f040e0188ec076c10f3a3260acb722257aa60a595ed815a158365fd5af3'),
    ('R', 'reverie-inguest/Cargo.toml', 5569, '0050a4e99fec9666cd297123bf298e19db712734e97cd97bd7b227114b826c50'),
    ('R', 'reverie-inguest/src/guest/exec_private.rs', 182321, '026cecaff9850b3bf916bc0ecb56d0e9021ca5130e7bfc1f64e21a48bc0dd4a9'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial.rs', 37162, 'a28b8e9b8a8b3efdc70f39e11253a5525aff36c315d33cd3fbec99f0489845ad'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay.rs', 50822, 'a7a4df88f1fd44855a0ccf7c60ca03dedb8c7e336c6239549484ee038fbc9547'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay/frontend.rs', 18922, '28a785bd1945b7aae1e72bc688da1918368ab5e2fb1a4d9927bf00be5e01fcb4'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/mapped_scalar.rs', 20465, '285f7f75bf9f388a771d685607ce65f7097c27d21869952d46b3a7ce70897612'),
    ('R', 'reverie-inguest/src/lib.rs', 11734, '0fbf94503eac27b65e52967c11c675e23e7a2f50338ca32150002ff4c64fabbf'),
    ('R', 'reverie-inguest/src/private_runtime/mod.rs', 2093, 'f6764939d2016bd3c9414b17b35f21f8cf7217f128135b83cfb5459aa3ae5e1e'),
    ('R', 'reverie-inguest/src/private_runtime/tls/abi.rs', 12171, 'f7de3cd3a0653785f8aacc01e0fabdeb89172b2660f07ea0df9d393078299aab'),
    ('R', 'reverie-inguest/src/private_runtime/tls/layout.rs', 499, '8fcceedb234c081b5125d95d73eaafa083fadc2d1be41698b724487ab7cfae36'),
    ('R', 'reverie-inguest/src/private_runtime/tls/mod.rs', 4853, '369e35bcafda51a271720507dcf7a7c9072fb5db7486e110e14f15e4df91f0dd'),
    ('R', 'reverie-inguest/src/trap.rs', 49799, '5c9249419574f00c540dec767359181c8ac03a47aa2df0f19e5cef0a59b5755b'),
    ('R', 'reverie-inguest/src/trap/fixed_gates.rs', 36554, 'ff3f6f6c46e872734ae921e869dbccc0ce1aeefe4bc3af8f29363cf276501e54'),
    ('R', 'reverie-inguest/src/trap/frame.rs', 35075, '357d2d884f7fdcc43b8d4acde8bc04e43023c85e6a77c569ac60c64dbb6030a0'),
    ('R', 'reverie-loader/Cargo.toml', 343, '0ebb8a376d4c8b6637d5058d76ad7bf9b3fcecea3deb474065e41a1707f7cc23'),
    ('R', 'reverie-loader/src/handoff.rs', 76132, '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'),
    ('R', 'reverie-loader/src/lib.rs', 1510, 'b3318334635e632a23283c1f51633b090eb809de9710ab0c98038b684abbb5b8'),
    ('R', 'reverie-loader/src/mapped_inputs.rs', 17803, '81cd7474215ff617fb1bc16b236461d4deb33697c7d559130f4bdb98b0f0e2c2'),
    ('R', 'reverie-loader/src/musl_tls.rs', 7478, '84011bb8505fa3357908e7445b1305f54303bae32ddb9d393546da8e309ee1ea'),
)
# Neutral core-support relocation; source association only. Full Git custody
# includes common/inguest/glibc_compat.rs; compiled/runtime proof remains separate.
NEUTRAL_CORE_SUPPORT = (
    ('H', 'detcore-liteinst/build.rs', 2878, '1c47b5a5440d9ecbbc0c10ada7e482867306a240aec9e365cb830438ea0d02c1'),
    ('H', 'detcore-liteinst/src/exec_initial.rs', 30450, '693667e8ae6efe87f9fa5810a61dbeffe3306ad89157aea9111b4b653a2236f8'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry.rs', 6707, '34e21f21a41f2346889c242c869bc32c74821c625dd5b9cf9bd2181ca2d8d40e'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry/bounds.rs', 1439, '4ab8a19e17b52905fea0176b45c63c48e5fc581c9977c927b8817c4de129f1e0'),
    ('H', 'detcore-liteinst/src/exec_native/catalog.rs', 10932, '3ef894a3325cd56a172fb67809d3b39109db839a916a6f3c6fcf927ec25c7ff7'),
    ('H', 'detcore-liteinst/src/exec_native/consumer.rs', 8232, '292f0a0df98d2dc6588f2af6976c79932423f44df84166ae158e9b4a7fb36b4d'),
    ('H', 'detcore-liteinst/src/exec_native/consumer_tests.rs', 21835, 'e3f0f107556e7261e5e5f80a8a2b4d1fbec38572de95df5df13363c479299fab'),
    ('H', 'detcore-liteinst/src/exec_native/driver.rs', 32420, 'bc0626efe96659f01fb6bb921cc61ef8bb76833522b45ba114d97888e8bb2975'),
    ('H', 'detcore-liteinst/src/exec_native/entry.rs', 17706, 'c9fe2808730baba77dd3710403d6f7f0eab0892b9bf6ae98fb9ea817eda94ca0'),
    ('H', 'detcore-liteinst/src/exec_native/mod.rs', 296, '25d35c840dd19367fe4ff54553972dc949b76dc4c6dfdb64c41e9657552724d1'),
    ('H', 'detcore-liteinst/src/exec_native/provider.rs', 19017, '12a6f762a00d460c06d03f143587124c8cc9c615725f8dce152282433f02ff1e'),
    ('H', 'detcore-liteinst/src/lib.rs', 30459, 'a0a3295844cf71c7dac508fc386da0e4e9459437a92b1741d7e2dc98f2955068'),
    ('R', 'reverie-inguest/Cargo.toml', 5569, '0050a4e99fec9666cd297123bf298e19db712734e97cd97bd7b227114b826c50'),
    ('R', 'reverie-inguest/src/guest/exec_private.rs', 182321, '026cecaff9850b3bf916bc0ecb56d0e9021ca5130e7bfc1f64e21a48bc0dd4a9'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial.rs', 37162, 'a28b8e9b8a8b3efdc70f39e11253a5525aff36c315d33cd3fbec99f0489845ad'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay.rs', 50822, 'a7a4df88f1fd44855a0ccf7c60ca03dedb8c7e336c6239549484ee038fbc9547'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay/frontend.rs', 18922, '28a785bd1945b7aae1e72bc688da1918368ab5e2fb1a4d9927bf00be5e01fcb4'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/mapped_scalar.rs', 20465, '285f7f75bf9f388a771d685607ce65f7097c27d21869952d46b3a7ce70897612'),
    ('R', 'reverie-inguest/src/lib.rs', 11734, '0fbf94503eac27b65e52967c11c675e23e7a2f50338ca32150002ff4c64fabbf'),
    ('R', 'reverie-inguest/src/private_runtime/mod.rs', 2093, 'f6764939d2016bd3c9414b17b35f21f8cf7217f128135b83cfb5459aa3ae5e1e'),
    ('R', 'reverie-inguest/src/private_runtime/tls/abi.rs', 12171, 'f7de3cd3a0653785f8aacc01e0fabdeb89172b2660f07ea0df9d393078299aab'),
    ('R', 'reverie-inguest/src/private_runtime/tls/layout.rs', 499, '8fcceedb234c081b5125d95d73eaafa083fadc2d1be41698b724487ab7cfae36'),
    ('R', 'reverie-inguest/src/private_runtime/tls/mod.rs', 4853, '369e35bcafda51a271720507dcf7a7c9072fb5db7486e110e14f15e4df91f0dd'),
    ('R', 'reverie-inguest/src/trap.rs', 49799, '5c9249419574f00c540dec767359181c8ac03a47aa2df0f19e5cef0a59b5755b'),
    ('R', 'reverie-inguest/src/trap/fixed_gates.rs', 36554, 'ff3f6f6c46e872734ae921e869dbccc0ce1aeefe4bc3af8f29363cf276501e54'),
    ('R', 'reverie-inguest/src/trap/frame.rs', 35075, '357d2d884f7fdcc43b8d4acde8bc04e43023c85e6a77c569ac60c64dbb6030a0'),
    ('R', 'reverie-loader/Cargo.toml', 343, '0ebb8a376d4c8b6637d5058d76ad7bf9b3fcecea3deb474065e41a1707f7cc23'),
    ('R', 'reverie-loader/src/handoff.rs', 76132, '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'),
    ('R', 'reverie-loader/src/lib.rs', 1510, 'b3318334635e632a23283c1f51633b090eb809de9710ab0c98038b684abbb5b8'),
    ('R', 'reverie-loader/src/mapped_inputs.rs', 17803, '81cd7474215ff617fb1bc16b236461d4deb33697c7d559130f4bdb98b0f0e2c2'),
    ('R', 'reverie-loader/src/musl_tls.rs', 7478, '84011bb8505fa3357908e7445b1305f54303bae32ddb9d393546da8e309ee1ea'),
)

# Same complete 0.5.4 source cohort, with the ordinary assembly function in
# its own exact section. This literal identity is reviewed source data, never
# a hash inferred from the source tree during a build.
FINAL_FUNCTION_SECTIONS_054 = tuple(
    ('R', 'reverie-inguest/src/trap.rs', 49885, '91c07be60ab719c9d353991a0230c058b22a457676d05f3866aab943ea55d6eb')
    if (row[0], row[1]) == ('R', 'reverie-inguest/src/trap.rs') else row
    for row in FINAL_DEVELOPMENT_054
)
# Complete source-only association for the composed split runtime.
# These are explicit reviewed candidate source facts, never derived in a build.
# All historical associations remain above; compilation/runtime proof is separate.
CURRENT_SPLIT_RUNTIME = (
    ('H', 'detcore-liteinst/build.rs', 2878, '1c47b5a5440d9ecbbc0c10ada7e482867306a240aec9e365cb830438ea0d02c1'),
    ('H', 'detcore-liteinst/src/exec_initial.rs', 30450, '693667e8ae6efe87f9fa5810a61dbeffe3306ad89157aea9111b4b653a2236f8'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry.rs', 6707, '34e21f21a41f2346889c242c869bc32c74821c625dd5b9cf9bd2181ca2d8d40e'),
    ('H', 'detcore-liteinst/src/exec_initial/mapped_entry/bounds.rs', 1439, '4ab8a19e17b52905fea0176b45c63c48e5fc581c9977c927b8817c4de129f1e0'),
    ('H', 'detcore-liteinst/src/exec_native/catalog.rs', 10932, '3ef894a3325cd56a172fb67809d3b39109db839a916a6f3c6fcf927ec25c7ff7'),
    ('H', 'detcore-liteinst/src/exec_native/consumer.rs', 8232, '292f0a0df98d2dc6588f2af6976c79932423f44df84166ae158e9b4a7fb36b4d'),
    ('H', 'detcore-liteinst/src/exec_native/consumer_tests.rs', 21835, 'e3f0f107556e7261e5e5f80a8a2b4d1fbec38572de95df5df13363c479299fab'),
    ('H', 'detcore-liteinst/src/exec_native/driver.rs', 32420, 'bc0626efe96659f01fb6bb921cc61ef8bb76833522b45ba114d97888e8bb2975'),
    ('H', 'detcore-liteinst/src/exec_native/entry.rs', 17706, 'c9fe2808730baba77dd3710403d6f7f0eab0892b9bf6ae98fb9ea817eda94ca0'),
    ('H', 'detcore-liteinst/src/exec_native/mod.rs', 296, '25d35c840dd19367fe4ff54553972dc949b76dc4c6dfdb64c41e9657552724d1'),
    ('H', 'detcore-liteinst/src/exec_native/provider.rs', 19017, '12a6f762a00d460c06d03f143587124c8cc9c615725f8dce152282433f02ff1e'),
    ('H', 'detcore-liteinst/src/lib.rs', 30459, 'a0a3295844cf71c7dac508fc386da0e4e9459437a92b1741d7e2dc98f2955068'),
    ('R', 'reverie-inguest/Cargo.toml', 5690, '2e268bb97c34408b56258876df631bcacfcb302bff02d61f16c7a27f0dd97309'),
    ('R', 'reverie-inguest/src/guest/exec_private.rs', 182321, '026cecaff9850b3bf916bc0ecb56d0e9021ca5130e7bfc1f64e21a48bc0dd4a9'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial.rs', 37162, 'a28b8e9b8a8b3efdc70f39e11253a5525aff36c315d33cd3fbec99f0489845ad'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay.rs', 50822, 'a7a4df88f1fd44855a0ccf7c60ca03dedb8c7e336c6239549484ee038fbc9547'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/libc_relay/frontend.rs', 18922, '28a785bd1945b7aae1e72bc688da1918368ab5e2fb1a4d9927bf00be5e01fcb4'),
    ('R', 'reverie-inguest/src/guest/exec_private/mapped_initial/mapped_scalar.rs', 20465, '285f7f75bf9f388a771d685607ce65f7097c27d21869952d46b3a7ce70897612'),
    ('R', 'reverie-inguest/src/lib.rs', 11734, '0fbf94503eac27b65e52967c11c675e23e7a2f50338ca32150002ff4c64fabbf'),
    ('R', 'reverie-inguest/src/private_runtime/mod.rs', 2093, 'f6764939d2016bd3c9414b17b35f21f8cf7217f128135b83cfb5459aa3ae5e1e'),
    ('R', 'reverie-inguest/src/private_runtime/tls/abi.rs', 12171, 'f7de3cd3a0653785f8aacc01e0fabdeb89172b2660f07ea0df9d393078299aab'),
    ('R', 'reverie-inguest/src/private_runtime/tls/layout.rs', 499, '8fcceedb234c081b5125d95d73eaafa083fadc2d1be41698b724487ab7cfae36'),
    ('R', 'reverie-inguest/src/private_runtime/tls/mod.rs', 4853, '369e35bcafda51a271720507dcf7a7c9072fb5db7486e110e14f15e4df91f0dd'),
    ('R', 'reverie-inguest/src/trap.rs', 49885, '91c07be60ab719c9d353991a0230c058b22a457676d05f3866aab943ea55d6eb'),
    ('R', 'reverie-inguest/src/trap/fixed_gates.rs', 36554, 'ff3f6f6c46e872734ae921e869dbccc0ce1aeefe4bc3af8f29363cf276501e54'),
    ('R', 'reverie-inguest/src/trap/frame.rs', 35075, '357d2d884f7fdcc43b8d4acde8bc04e43023c85e6a77c569ac60c64dbb6030a0'),
    ('R', 'reverie-loader/Cargo.toml', 343, '0ebb8a376d4c8b6637d5058d76ad7bf9b3fcecea3deb474065e41a1707f7cc23'),
    ('R', 'reverie-loader/src/handoff.rs', 76132, '85abd905256f3bc5b526840cf3cd5e090238e4aa770f03d320c2ae907ee7ae75'),
    ('R', 'reverie-loader/src/lib.rs', 1510, 'b3318334635e632a23283c1f51633b090eb809de9710ab0c98038b684abbb5b8'),
    ('R', 'reverie-loader/src/mapped_inputs.rs', 17803, '81cd7474215ff617fb1bc16b236461d4deb33697c7d559130f4bdb98b0f0e2c2'),
    ('R', 'reverie-loader/src/musl_tls.rs', 7478, '84011bb8505fa3357908e7445b1305f54303bae32ddb9d393546da8e309ee1ea'),
)
# The derived cohort reads exactly these rows from Git objects. Same paths as
# CURRENT_SPLIT_RUNTIME except the #[cfg(test)] consumer_tests.rs module
# (detcore-liteinst/src/exec_native/consumer.rs), which builds into no artifact.
DERIVED_COHORT = 'derived-from-pinned-objects-v1'
DERIVED_ROWS = tuple((label, path) for label, path, _, _ in CURRENT_SPLIT_RUNTIME
                     if path != 'detcore-liteinst/src/exec_native/consumer_tests.rs')
TRAP_SOURCE = 'reverie-inguest/src/trap.rs'
MAPPED_ENTRY_SOURCE = 'detcore-liteinst/src/exec_initial/mapped_entry.rs'
TERMINAL_SOURCE = 'reverie-liteinst-artifact/src/private_static/selected_initializers/terminal_observation.rs'
ENTRY_SOURCE_NAMES = ('ENTRY_SOURCE', 'BOOTSTRAP_ENTRY_SOURCE')
LOCK_REVERIE = 'github.com/rrnewton/reverie'
# The gate shape the ordinary provider's DedicatedFunction cohort depends on.
# ordinary_gate.rs compiles the other half: a 44-byte input section of exactly
# this name (DEDICATED_SECTION, Cohort::text_size). The x86_64 global_asm!
# block that defines the entry is the reviewed identity; its whole-line bytes
# are the same at Reverie e39f78dd, add6180b and 44ef088f. A new value needs
# review of that block, not a regenerated literal.
TRUSTED_SYSCALL_SECTION = '.text.reverie_inguest_trusted_syscall'
TRUSTED_SYSCALL_ENTRY = 'reverie_inguest_trusted_syscall'
TRUSTED_SYSCALL_X86_64_BLOCK_SHA256 = '095374039289c0c2063acdc8eabbd7bb1cbaf6a07e44d50db32ee9ab796fd4e2'
GATE_ARCHES = ('aarch64', 'x86_64')

COHORTS = (("historical-bounded-bootstrap", HISTORICAL),
           ("merged-runtime-content-v1", CURRENT_MERGED),
           ("final-candidate-content-v1", FINAL_CANDIDATE_V1),
           ("development-051-content-v1", FINAL_DEVELOPMENT_051),
           ("development-054-content-v1", FINAL_DEVELOPMENT_054),
           ("function-sections-054-content-v1", FINAL_FUNCTION_SECTIONS_054),
           ("neutral-core-support-content-v1", NEUTRAL_CORE_SUPPORT),
           ("current-split-runtime-content-v1", CURRENT_SPLIT_RUNTIME))


def need(ok, message):
    if not ok:
        raise RuntimeError(message)


def observed_file(path):
    return observed_source(path)[0]


def observed_source(path):
    """One bounded no-follow read: the identity fact and the bytes it covers."""
    need(path.is_absolute() and path.resolve() == path, 'runtime cohort indirect path')
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(descriptor)
        need(stat.S_ISREG(before.st_mode) and 0 < before.st_size <= 2 * 1024 * 1024,
             'runtime cohort nonregular/oversized source')
        chunks = []
        remaining = before.st_size
        while remaining:
            data = os.read(descriptor, min(remaining, 1024 * 1024))
            need(data, 'runtime cohort source truncated')
            chunks.append(data)
            remaining -= len(data)
        after = os.fstat(descriptor)
        def identity(info):
            return (info.st_dev, info.st_ino, info.st_mode, info.st_nlink,
                    info.st_size, info.st_mtime_ns, info.st_ctime_ns)
        need(identity(before) == identity(after) == identity(path.lstat()),
             'runtime cohort source changed while reading')
        raw = b''.join(chunks)
        return dict(path=str(path), bytes=before.st_size, sha256=hashlib.sha256(raw).hexdigest(),
                    identity=list(identity(before))), raw
    finally:
        os.close(descriptor)


GIT_ENV = dict(GIT_OPTIONAL_LOCKS='0', GIT_NO_LAZY_FETCH='1', GIT_NO_REPLACE_OBJECTS='1',
               GIT_TERMINAL_PROMPT='0')


def git(repo, *args, data=None):
    """Read-only Git query; every failure is a refusal, never a fallback."""
    description = 'runtime cohort derivation: git ' + ' '.join(args[:2]) + ' in ' + str(repo)
    try:
        child = subprocess.run(['git', '--no-replace-objects', '-C', str(repo), *args], input=data,
                               capture_output=True, timeout=60, env=dict(os.environ, **GIT_ENV))
    except (OSError, subprocess.SubprocessError) as failure:
        raise RuntimeError(description + ' failed: ' + repr(failure)) from failure
    need(child.returncode == 0,
         description + ' failed: ' + child.stderr.decode(errors='replace').strip()[:400])
    need(len(child.stdout) <= 64 * 1024 * 1024, description + ' output exceeds bound')
    return child.stdout


def objects(repo, label, specs, limit=2 * 1024 * 1024):
    """Blob bytes for each REV:PATH, refusing any missing object by name."""
    raw = git(repo, 'cat-file', '--batch', data=''.join(spec + '\n' for spec in specs).encode())
    result = {}
    at = 0
    for spec in specs:
        end = raw.find(b'\n', at)
        need(end >= 0, 'runtime cohort derivation: truncated object stream ' + label + ' ' + spec)
        fields = raw[at:end].split()
        at = end + 1
        need(len(fields) == 3, 'runtime cohort derivation: missing object ' + label + ' ' + spec)
        need(fields[1] == b'blob', 'runtime cohort derivation: not a blob ' + label + ' ' + spec)
        size = int(fields[2])
        need(size <= limit and raw[at + size:at + size + 1] == b'\n',
             'runtime cohort derivation: oversized/truncated object ' + label + ' ' + spec)
        result[spec] = raw[at:at + size]
        at += size + 1
    need(at == len(raw), 'runtime cohort derivation: unexpected object stream data')
    return result


def lock_pins(workspace, h_head, rpin):
    """Every Cargo.lock reverie rev in H_head's objects must equal the gitlink."""
    names = [os.fsdecode(name) for name in
             git(workspace, 'ls-tree', '-rz', '--full-tree', '--name-only', h_head).split(b'\0')
             if name == b'Cargo.lock' or name.endswith(b'/Cargo.lock')]
    blobs = objects(workspace, 'H', [h_head + ':' + name for name in names], limit=16 * 1024 * 1024)
    rows = []
    for name in names:
        text = blobs[h_head + ':' + name].decode('utf-8', errors='replace')
        for number, line in enumerate(text.splitlines(), 1):
            if not re.search(r'github\.com/rrnewton/reverie(?![-\w])', line):
                continue
            where = 'runtime cohort derivation: ' + name + ':' + str(number)
            pins = re.findall(r'(?:rev=|#)([0-9a-f]{40})', line)
            need(pins, where + ' names reverie without a 40-hex pin')
            for pin in pins:
                need(pin == rpin, where + ' reverie pin ' + pin + ' differs from the H_head gitlink ' + rpin)
            rows.append([name, number])
    need(any(name == 'Cargo.lock' for name, _ in rows),
         'runtime cohort derivation: root Cargo.lock carries no reverie pin')
    return rows


def entry_source_guard(mapped, terminal, rpin):
    """Compare H's entry blob with the two literal declarations in pinned R.

    This only recognizes the literal const form the terminal guard uses. Skip
    comments and string/character literals so examples cannot supply a missing
    declaration; every other entry-constant form requires an explicit update.
    """
    where = 'runtime cohort derivation: Reverie ' + rpin + ' ' + TERMINAL_SOURCE
    refusal = where + ' requires exactly one literal each for ' + ', '.join(ENTRY_SOURCE_NAMES)
    token = re.compile(rb'//[^\n]*|(?P<comment>/\*)|(?:br|cr|r)(?P<hashes>#{0,255})"|'
                       rb'"(?:\\.|[^"\\])*"|(?:b)?\'(?:\\.|[^\'\\])\'|'
                       rb'(?P<identifier>(?:r\#)?[A-Za-z_][A-Za-z0-9_]*)')
    at, pins, after_const = 0, [], False
    while match := token.search(terminal, at):
        at = match.end()
        if match['comment']:
            depth = 1
            while depth:
                end = re.search(rb'/\*|\*/', terminal[at:])
                need(end is not None, refusal + ': unterminated comment')
                depth += 1 if end[0] == b'/*' else -1
                at += end.end()
        elif match['hashes'] is not None:
            close = b'"' + match['hashes']
            end = terminal.find(close, at)
            need(end >= 0, refusal + ': unterminated raw string')
            at = end + len(close)
        elif match['identifier']:
            name = match['identifier'].decode()
            declaration, after_const = after_const, name == 'const'
            plain = name.removeprefix('r#')
            if not declaration or not (plain == 'ENTRY_SOURCE' or plain.endswith('_ENTRY_SOURCE')):
                continue
            need(name == plain, refusal + ': unsupported raw identifier ' + name)
            literal = re.match(rb'\s*:\s*&str\s*=\s*"([0-9a-f]{64})"\s*;', terminal[at:])
            need(literal is not None, refusal + ': malformed ' + name)
            pins.append((name, literal[1].decode()))
            at += literal.end()
    need(sorted(name for name, _ in pins) == sorted(ENTRY_SOURCE_NAMES),
         refusal + ': found ' + ', '.join(name for name, _ in pins))
    accepted = dict(pins)
    digest = hashlib.sha256(mapped).hexdigest()
    need(digest in accepted.values(),
         'runtime cohort derivation: Hermit ' + MAPPED_ENTRY_SOURCE + ' sha256 ' + digest +
         ' is not accepted by Reverie ' + rpin + ' ' + TERMINAL_SOURCE + ' (' +
         ', '.join(name + ' ' + accepted[name] for name in ENTRY_SOURCE_NAMES) + ')')


def derive(workspace, h_head, runtime_r_pin=None):
    """Rows read from H_head's objects and the Reverie commit its gitlink names.

    The working tree is never read here. A missing object, a disagreeing pin
    or a dirty or moved submodule is refused by name.
    """
    workspace = Path(workspace)
    need(isinstance(h_head, str) and re.fullmatch('[0-9a-f]{40}', h_head),
         'runtime cohort derivation: H_head must be 40 lowercase hex')
    need(Path(git(workspace, 'rev-parse', '--show-toplevel').decode().strip()).resolve() == workspace.resolve(),
         'runtime cohort derivation: H is not its own checkout: ' + str(workspace))
    head = git(workspace, 'rev-parse', '--verify', 'HEAD^{commit}').decode().strip()
    need(head == h_head, 'runtime cohort derivation: H checkout HEAD ' + head + ' is not H_head ' + h_head)
    entries = [e for e in git(workspace, 'ls-tree', '-z', '--full-tree', h_head, '--', 'reverie').split(b'\0') if e]
    header = entries[0].split(b'\t', 1)[0].split() if len(entries) == 1 and entries[0].endswith(b'\treverie') else []
    need(header[:2] == [b'160000', b'commit'],
         'runtime cohort derivation: H_head ' + h_head + ' has no reverie gitlink')
    rpin = header[2].decode()
    need(runtime_r_pin is None or runtime_r_pin == rpin,
         'runtime cohort derivation: declared runtime_R_pin ' + str(runtime_r_pin) +
         ' differs from the H_head gitlink ' + rpin)
    pins = lock_pins(workspace, h_head, rpin)
    reverie = workspace / 'reverie'
    need(reverie.is_dir() and not reverie.is_symlink() and
         Path(git(reverie, 'rev-parse', '--show-toplevel').decode().strip()).resolve() == reverie.resolve(),
         'runtime cohort derivation: reverie submodule is not an initialized checkout')
    moved = git(reverie, 'rev-parse', '--verify', 'HEAD^{commit}').decode().strip()
    need(moved == rpin, 'runtime cohort derivation: reverie submodule HEAD ' + moved +
         ' is not the H_head gitlink ' + rpin)
    dirty = [os.fsdecode(e) for e in git(reverie, 'status', '--porcelain=v1', '-z', '--untracked-files=no',
                                         '--ignore-submodules=none').split(b'\0') if e]
    need(not dirty, 'runtime cohort derivation: reverie submodule is dirty: ' + ', '.join(dirty[:8]))
    blobs = {}
    for label, repo, rev in [('H', workspace, h_head), ('R', reverie, rpin)]:
        paths = [path for owner, path in DERIVED_ROWS if owner == label]
        found = objects(repo, label, [rev + ':' + path for path in paths])
        blobs.update({(label, path): found[rev + ':' + path] for path in paths})
    rows = tuple((label, path, len(blobs[label, path]), hashlib.sha256(blobs[label, path]).hexdigest())
                 for label, path in DERIVED_ROWS)
    terminal = rpin + ':' + TERMINAL_SOURCE
    entry_source_guard(blobs['H', MAPPED_ENTRY_SOURCE], objects(reverie, 'R', [terminal])[terminal], rpin)
    return dict(h_head=h_head, runtime_R_pin=rpin, rows=rows, lock_pins=pins,
                gate=gate_guard(blobs['R', TRAP_SOURCE]))


def bind(workspace, h_head, runtime_r_pin=None):
    """Match the tree to the rows its chosen heads' objects declare."""
    derived = derive(workspace, h_head, runtime_r_pin)
    result = select(workspace, ((DERIVED_COHORT, derived['rows']),),
                    ' at H_head ' + derived['h_head'] + ', Reverie ' + derived['runtime_R_pin'])
    # Lists, not tuples: this result is saved in the prepared binding's JSON
    # and compared with a fresh one in run mode. derive() keeps its tuples.
    return dict(result, h_head=derived['h_head'], runtime_R_pin=derived['runtime_R_pin'],
                rows=[list(row) for row in derived['rows']], lock_pins=derived['lock_pins'],
                gate=derived['gate'])


MACRO = re.compile(rb'(?m)^[ \t]*(?:(?:core|std)::arch::)?global_asm![ \t]*\(')
TARGET_ARCH = re.compile(rb'#\[cfg\(target_arch = "([a-z0-9_]+)"\)\]')
SECTION = re.compile(re.escape(TRUSTED_SYSCALL_SECTION.encode()) + rb'(?![A-Za-z0-9_$])')
ENTRY = re.compile(rb'(?m)^[ \t]*' + re.escape(TRUSTED_SYSCALL_ENTRY.encode()) + rb':')


def macro_end(raw, at):
    """Offset of the bracket closing a macro opened before AT, past literals and comments."""
    depth = 1
    while at < len(raw):
        char = raw[at:at + 1]
        literal = (re.match(rb'r(#*)"', raw[at:at + 300])
                   if char == b'r' and not re.match(rb'[A-Za-z0-9_]', raw[at - 1:at]) else None)
        if literal:
            close = raw.find(b'"' + literal.group(1), at + literal.end())
            need(close >= 0, 'trusted syscall gate shape: unterminated raw string')
            at = close + 1 + len(literal.group(1))
        elif char == b'"':
            at += 1
            while at < len(raw) and raw[at:at + 1] != b'"':
                at += 2 if raw[at:at + 1] == b'\\' else 1
            need(at < len(raw), 'trusted syscall gate shape: unterminated string')
            at += 1
        elif raw.startswith(b'//', at):
            stop = raw.find(b'\n', at)
            at = len(raw) if stop < 0 else stop
        elif raw.startswith(b'/*', at):
            stop = raw.find(b'*/', at + 2)
            need(stop >= 0, 'trusted syscall gate shape: unterminated comment')
            at = stop + 2
        else:
            if char in (b'(', b'[', b'{'):
                depth += 1
            elif char in (b')', b']', b'}'):
                depth -= 1
                if depth == 0:
                    return at
            at += 1
    raise RuntimeError('trusted syscall gate shape: unterminated global_asm! block')


def gate_shape(raw):
    """Hash every global_asm! block that fills the gate section, one per arch.

    A block is its whole lines, from the macro line through the line holding
    the closing bracket. Exactly one x86_64 and one aarch64 block, each gated
    by a single target_arch cfg and each defining the entry, may name the
    section; nothing else in the file may.
    """
    need(isinstance(raw, bytes) and len(raw) <= 2 * 1024 * 1024, 'trusted syscall gate shape: source bound')
    blocks = []
    for match in MACRO.finditer(raw):
        start = match.start()
        line = raw.count(b'\n', 0, start) + 1
        need(not blocks or start >= blocks[-1]['end'],
             'trusted syscall gate shape: overlapping global_asm! block at line %d' % line)
        close = macro_end(raw, match.end())
        need(raw[close + 1:close + 2] == b';',
             'trusted syscall gate shape: global_asm! at line %d is not a statement' % line)
        stop = raw.find(b'\n', close)
        stop = len(raw) if stop < 0 else stop + 1
        attributes = []
        above = start
        while above > 0:
            previous = raw.rfind(b'\n', 0, above - 1) + 1
            text = raw[previous:above].strip()
            if not text.startswith(b'#['):
                break
            attributes.append(text)
            above = previous
        cfgs = [text for text in attributes if text.startswith(b'#[cfg')]
        arch = (TARGET_ARCH.fullmatch(cfgs[0]).group(1).decode()
                if len(cfgs) == 1 and TARGET_ARCH.fullmatch(cfgs[0]) else None)
        body = raw[start:stop]
        blocks.append(dict(line=line, start=start, end=stop, arch=arch,
                           sha256=hashlib.sha256(body).hexdigest(),
                           contributes=bool(SECTION.search(body)), defines_entry=bool(ENTRY.search(body))))
    for match in SECTION.finditer(raw):
        need(any(block['start'] <= match.start() < block['end'] for block in blocks),
             'trusted syscall gate shape: section named outside a global_asm! block at line %d'
             % (raw.count(b'\n', 0, match.start()) + 1))
    contributing = [block for block in blocks if block['contributes']]
    for block in contributing:
        need(block['arch'] is not None,
             'trusted syscall gate shape: contributing block at line %d has no single target_arch cfg' % block['line'])
    need(sorted(block['arch'] for block in contributing) == sorted(GATE_ARCHES),
         'trusted syscall gate shape: exactly one contributing block per target_arch x86_64 and aarch64 '
         'required, found ' + repr([(block['arch'], block['line']) for block in contributing]))
    for block in blocks:
        need(block['contributes'] or not block['defines_entry'],
             'trusted syscall gate shape: block at line %d defines %s outside the section'
             % (block['line'], TRUSTED_SYSCALL_ENTRY))
        need(not block['contributes'] or block['defines_entry'],
             'trusted syscall gate shape: contributing %s block at line %d does not define %s'
             % (block['arch'], block['line'], TRUSTED_SYSCALL_ENTRY))
    return dict({block['arch']: block['sha256'] for block in contributing}, blocks=blocks)


def gate_guard(raw):
    """The x86_64 block is the guard; the aarch64 block is recorded."""
    shape = gate_shape(raw)
    need(shape['x86_64'] == TRUSTED_SYSCALL_X86_64_BLOCK_SHA256,
         'trusted syscall gate shape: x86_64 block sha256 ' + shape['x86_64'] +
         ' differs from reviewed ' + TRUSTED_SYSCALL_X86_64_BLOCK_SHA256)
    return shape


def pin_bump_report(reverie_git, old, new):
    """Every derived R row whose content differs between two Reverie revisions."""
    for rev in (old, new):
        need(isinstance(rev, str) and re.fullmatch('[0-9a-f]{40}', rev),
             'runtime cohort derivation: report revisions must be 40 lowercase hex')
    paths = [path for label, path in DERIVED_ROWS if label == 'R']
    before = objects(reverie_git, 'R', [old + ':' + path for path in paths])
    after = objects(reverie_git, 'R', [new + ':' + path for path in paths])
    rows = []
    for path in paths:
        a, b = before[old + ':' + path], after[new + ':' + path]
        if a == b:
            continue
        numstat = git(reverie_git, 'diff', '--no-ext-diff', '--no-renames', '--numstat',
                      old, new, '--', path).decode().split()
        row = dict(path=path, old_bytes=len(a), old_sha256=hashlib.sha256(a).hexdigest(),
                   new_bytes=len(b), new_sha256=hashlib.sha256(b).hexdigest(),
                   stat='+%s -%s' % tuple(numstat[:2]) if len(numstat) >= 2 else 'binary')
        if path == TRAP_SOURCE:
            def shape(raw):
                try:
                    found = gate_shape(raw)
                except RuntimeError as refusal:
                    return dict(refused=str(refusal))
                return {arch: found[arch] for arch in GATE_ARCHES}
            row['gate'] = dict(old=shape(a), new=shape(b))
        rows.append(row)
    return rows


def format_pin_bump_report(old, new, rows):
    lines = ['Reverie pin bump %s -> %s: %d reviewed runtime source row(s) changed' % (old, new, len(rows))]
    for row in rows:
        lines.append('R %s %d:%s -> %d:%s %s' % (row['path'], row['old_bytes'], row['old_sha256'],
                                                 row['new_bytes'], row['new_sha256'], row['stat']))
        for arch in ('x86_64', 'aarch64') if 'gate' in row else ():
            before, after = row['gate']['old'].get(arch), row['gate']['new'].get(arch)
            verdict = 'unchanged' if before == after and before else 'CHANGED, review the gate block'
            if arch == 'x86_64' and after != TRUSTED_SYSCALL_X86_64_BLOCK_SHA256:
                verdict += '; differs from the reviewed guard, the build refuses'
            lines.append('    gate %s %s -> %s %s' % (arch, before or row['gate']['old'].get('refused'),
                                                       after or row['gate']['new'].get('refused'), verdict))
    return '\n'.join(lines) + '\n'


def select(workspace, cohorts, context=''):
    """Match ONE complete declared source tuple and close every successful read."""
    workspace = Path(workspace)
    roots = {'H': workspace, 'R': workspace / 'reverie'}
    keys = sorted({(row[0],row[1]) for _,rows in cohorts for row in rows})
    need(keys and len(keys) <= 128, 'runtime cohort declaration bound')
    opened = []
    error = None
    result = None
    try:
        for label,relative in keys:
            need(label in roots and not Path(relative).is_absolute() and
                 all(v not in ('.','..') for v in Path(relative).parts),
                 'runtime cohort declaration path')
            fact = observed_file(roots[label] / relative)
            opened.append((label,relative,fact))
        need(sum(fact['bytes'] for _,_,fact in opened) <= 64 * 1024 * 1024,
             'runtime cohort aggregate bound')
        observed = {(label,relative):(fact['bytes'],fact['sha256']) for label,relative,fact in opened}
        matches = [name for name,rows in cohorts if len(rows) == len(observed) and
                   len({(v[0],v[1]) for v in rows}) == len(rows) and
                   all(observed.get((label,relative)) == (size,digest)
                       for label,relative,size,digest in rows)]
        # A single declared tuple names each differing row with both identities.
        differing = ['%s %s observed %d:%s expected %d:%s' % (label, relative, *observed[label, relative], size, digest)
                     for _,rows in cohorts[:1] if len(cohorts) == 1
                     for label,relative,size,digest in rows
                     if (label, relative) in observed and observed[label, relative] != (size, digest)]
        need(len(matches) == 1, 'runtime sources are not one complete reviewed content cohort' + context +
             (': ' + '; '.join(differing) if differing else ''))
        result = dict(cohort=matches[0], files=[fact for _,_,fact in opened])
    except Exception as failure:
        error = failure
    except BaseException as failure:
        error = failure
        raise
    finally:
        # Close every successful read, including after a directly rethrown
        # cancellation. ExitStack keeps running the remaining close callbacks.
        cancelled = error if error is not None and not isinstance(error, Exception) else None
        closes = []

        def close(fact):
            nonlocal cancelled
            try:
                need(observed_file(Path(fact['path'])) == fact, 'runtime cohort source changed at close')
            except Exception as failure:
                closes.append(repr(failure))
            except BaseException as failure:
                if cancelled is None:
                    cancelled = failure
                closes.append(repr(failure))
                raise

        try:
            with ExitStack() as pending:
                for _,_,fact in reversed(opened):
                    pending.callback(close, fact)
        finally:
            if cancelled is not None:
                if error is cancelled and not closes:
                    raise cancelled
                raise cancelled from RuntimeError(dict(body_error=repr(error),close_errors=closes))
            if error is not None or closes:
                raise RuntimeError(dict(body_error=repr(error),close_errors=closes)) from error
    return result
