let default_workers = 4
let default_iterations = 1_000_000

let positive_argument index default name =
  if Array.length Sys.argv <= index then default
  else
    let value = int_of_string Sys.argv.(index) in
    if value <= 0 then invalid_arg (name ^ " must be positive") else value

let mix worker iterations =
  let state = ref (worker + 1) in
  for iteration = 1 to iterations do
    state := ((!state lxor iteration * 1_103_515_245) + 12_345) land max_int
  done;
  !state

let () =
  let workers = positive_argument 1 default_workers "workers" in
  let iterations = positive_argument 2 default_iterations "iterations" in
  let start_mutex = Mutex.create () in
  let start_condition = Condition.create () in
  let ready = ref 0 in
  let start = ref false in
  let next_slot = Atomic.make 0 in
  let completion_order = Array.make workers (-1) in
  let await_start () =
    Mutex.lock start_mutex;
    incr ready;
    Condition.broadcast start_condition;
    while not !start do
      Condition.wait start_condition start_mutex
    done;
    Mutex.unlock start_mutex
  in
  let run worker =
    await_start ();
    let result = mix worker iterations in
    let slot = Atomic.fetch_and_add next_slot 1 in
    completion_order.(slot) <- worker;
    result
  in
  let domains =
    Array.init workers (fun worker -> Domain.spawn (fun () -> run worker))
  in
  Mutex.lock start_mutex;
  while !ready <> workers do
    Condition.wait start_condition start_mutex
  done;
  start := true;
  Condition.broadcast start_condition;
  Mutex.unlock start_mutex;
  let checksum =
    Array.fold_left (fun sum domain -> sum lxor Domain.join domain) 0 domains
  in
  Printf.printf "order=";
  Array.iteri
    (fun index worker ->
      if index > 0 then print_char ',';
      print_int worker)
    completion_order;
  Printf.printf " checksum=%d\n" checksum
