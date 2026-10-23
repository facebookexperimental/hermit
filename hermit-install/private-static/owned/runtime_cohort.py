"""Finite source association only; full chosen-head custody and Rust proofs remain mandatory."""
import hashlib
import os
from pathlib import Path
import stat

# Projected once from the previously accepted historical source objects. Never
# derive expected rows from the tree being built. New candidate tuple is absent
# until its COMPLETE final source contents receive independent review.
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
COHORTS = (("historical-bounded-bootstrap", HISTORICAL),
           ("merged-runtime-content-v1", CURRENT_MERGED),
           ("final-candidate-content-v1", FINAL_CANDIDATE_V1),
           ("development-051-content-v1", FINAL_DEVELOPMENT_051))


def need(ok, message):
    if not ok:
        raise RuntimeError(message)


def observed_file(path):
    need(path.is_absolute() and path.resolve() == path, 'runtime cohort indirect path')
    descriptor = os.open(path, os.O_RDONLY | os.O_CLOEXEC | os.O_NOFOLLOW | os.O_NONBLOCK)
    try:
        before = os.fstat(descriptor)
        need(stat.S_ISREG(before.st_mode) and 0 < before.st_size <= 2 * 1024 * 1024,
             'runtime cohort nonregular/oversized source')
        digest = hashlib.sha256()
        remaining = before.st_size
        while remaining:
            data = os.read(descriptor, min(remaining, 1024 * 1024))
            need(data, 'runtime cohort source truncated')
            digest.update(data)
            remaining -= len(data)
        after = os.fstat(descriptor)
        def identity(info):
            return (info.st_dev, info.st_ino, info.st_mode, info.st_nlink,
                    info.st_size, info.st_mtime_ns, info.st_ctime_ns)
        need(identity(before) == identity(after) == identity(path.lstat()),
             'runtime cohort source changed while reading')
        return dict(path=str(path), bytes=before.st_size, sha256=digest.hexdigest(),
                    identity=list(identity(before)))
    finally:
        os.close(descriptor)


def bind(workspace):
    """Match ONE complete declared source tuple and close every successful read."""
    workspace = Path(workspace)
    roots = {'H': workspace, 'R': workspace / 'reverie'}
    keys = sorted({(row[0],row[1]) for _,rows in COHORTS for row in rows})
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
        matches = [name for name,rows in COHORTS if len(rows) == len(observed) and
                   len({(v[0],v[1]) for v in rows}) == len(rows) and
                   all(observed.get((label,relative)) == (size,digest)
                       for label,relative,size,digest in rows)]
        need(len(matches) == 1, 'runtime sources are not one complete reviewed content cohort')
        result = dict(cohort=matches[0], files=[fact for _,_,fact in opened])
    except BaseException as failure:
        error = failure
    # Close every successful read, then re-raise the first KeyboardInterrupt
    # or SystemExit itself; ordinary failures keep the aggregate RuntimeError.
    cancelled = error if error is not None and not isinstance(error, Exception) else None
    closes = []
    for _,_,fact in opened:
        try:
            need(observed_file(Path(fact['path'])) == fact, 'runtime cohort source changed at close')
        except BaseException as failure:
            closes.append(repr(failure))
            if cancelled is None and not isinstance(failure, Exception):
                cancelled = failure
    if cancelled is not None:
        if error is cancelled and not closes:
            raise cancelled
        raise cancelled from RuntimeError(dict(body_error=repr(error),close_errors=closes))
    if error is not None or closes:
        raise RuntimeError(dict(body_error=repr(error),close_errors=closes)) from error
    return result
