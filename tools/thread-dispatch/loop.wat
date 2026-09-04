(module
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory 1)(export "memory" (memory 0))
  (func $run (export "_start") (local $n i64) (local.set $n (i64.const 2000000000))
    (block $d (loop $L (br_if $d (i64.eqz (local.get $n)))
      (local.set $n (i64.sub (local.get $n) (i64.const 1))) (br $L)))
    (call $exit (i32.const 0))))
