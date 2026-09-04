(module
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory 1)(export "memory" (memory 0))
  (type $t (func (param i64)))
  (table 1 funcref) (elem (i32.const 0) $f)
  (func $f (param $n i64)
    (if (i64.eqz (local.get $n)) (then (call $exit (i32.const 0))))
    (return_call_indirect (type $t) (i64.sub (local.get $n) (i64.const 1)) (i32.const 0)))
  (func $run (export "_start") (call $f (i64.const 2000000000))))
