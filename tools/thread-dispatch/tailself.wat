(module
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory 1)(export "memory" (memory 0))
  (func $f (param $n i64)
    (if (i64.eqz (local.get $n)) (then (call $exit (i32.const 0))))
    (return_call $f (i64.sub (local.get $n) (i64.const 1))))
  (func $run (export "_start") (call $f (i64.const 2000000000))))
