(module
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory 1)
  (export "memory" (memory 0))
  (global $pc (mut i32) (i32.const 0))
  (global $r0 (mut i64) (i64.const 0))
  (global $r1 (mut i64) (i64.const 200000000))
  (data (i32.const 0) "\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\01\00\00\00\02\00\00\00\00\00\00\00\03\00\00\00\00\00\00\00\00\00\00\00\00\00\00\00")

  (func $run (export "_start")
    (local $w i64) (local $op i32)
    (block $done
      (loop $L
        (local.set $w (i64.load (i32.mul (global.get $pc) (i32.const 8))))
        (local.set $op (i32.wrap_i64 (i64.and (local.get $w) (i64.const 0xff))))
        (global.set $pc (i32.add (global.get $pc) (i32.const 1)))
        (block $c3 (block $c2 (block $c1 (block $c0
          (br_table $c0 $c1 $c2 $c3 (local.get $op)))
          ;; c0 = HALT
          (br $done))
          ;; c1 = ADDI
          (global.set $r0 (i64.add (global.get $r0) (i64.shr_u (local.get $w) (i64.const 32))))
          (br $L))
          ;; c2 = DEC
          (global.set $r1 (i64.sub (global.get $r1) (i64.const 1)))
          (br $L))
        ;; c3 = BRNZ
        (if (i64.ne (global.get $r1) (i64.const 0))
          (then (global.set $pc (i32.wrap_i64 (i64.shr_u (local.get $w) (i64.const 32))))))
        (br $L)))
    (call $exit (i32.wrap_i64 (i64.and (global.get $r0) (i64.const 0xff)))))
)
