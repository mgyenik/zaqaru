(module
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory 1)
  (export "memory" (memory 0))
  (global $pc (mut i32) (i32.const 0))
  (global $r0 (mut i64) (i64.const 0))
  (global $r1 (mut i64) (i64.const 200000000))
  (data (i32.const 0) "\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\02\00\00\00\00\00\00\00\03\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00")

  (type $handler (func))
  (table $h 4 funcref)
  (elem (i32.const 0) $h_halt $h_addi $h_dec $h_brnz)
  ;; dispatch: read op at pc, tail-call its handler
  (func $disp
    (return_call_indirect (type $handler)
      (i32.wrap_i64 (i64.and (i64.load (i32.mul (global.get $pc) (i32.const 8))) (i64.const 0xff)))))
  (func $h_halt
    (call $exit (i32.wrap_i64 (i64.and (global.get $r0) (i64.const 0xff)))))
  (func $h_addi
    (global.set $r0 (i64.add (global.get $r0)
      (i64.shr_u (i64.load (i32.mul (global.get $pc) (i32.const 8))) (i64.const 32))))
    (global.set $pc (i32.add (global.get $pc) (i32.const 1)))
    (return_call $disp))
  (func $h_dec
    (global.set $r1 (i64.sub (global.get $r1) (i64.const 1)))
    (global.set $pc (i32.add (global.get $pc) (i32.const 1)))
    (return_call $disp))
  (func $h_brnz
    (if (i64.ne (global.get $r1) (i64.const 0))
      (then (global.set $pc (i32.wrap_i64 (i64.shr_u (i64.load (i32.mul (global.get $pc) (i32.const 8))) (i64.const 32)))))
      (else (global.set $pc (i32.add (global.get $pc) (i32.const 1)))))
    (return_call $disp))
  (func $run (export "_start") (call $disp))
)
