(* Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under the BSD-style license found in the
 * LICENSE file in the root directory of this source tree. *)

let () =
  Random.self_init ();
  Printf.printf "%08x%08x%08x%08x\n"
    (Random.bits ()) (Random.bits ()) (Random.bits ()) (Random.bits ())
