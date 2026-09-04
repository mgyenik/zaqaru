import struct, subprocess, os, time

# Bytecode: 8x ADDI(1); DEC r1; BRNZ->0; HALT.  op = word & 0xff, imm = word>>32.
ITER = 200_000_000
words = [ (1 | (1<<32)) ]*8 + [2] + [3 | (0<<32)] + [0]
data = b"".join(struct.pack("<Q", w) for w in words)
data_str = "".join("\\%02x" % b for b in data)

common_head = f'''(module
  (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
  (memory 1)
  (global $pc (mut i32) (i32.const 0))
  (global $r0 (mut i64) (i64.const 0))
  (global $r1 (mut i64) (i64.const {ITER}))
  (data (i32.const 0) "{data_str}")
'''

# ---- switch-loop interpreter: one central br_table ----
switch = common_head + '''
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
'''

# ---- threaded interpreter: one handler per op, return_call_indirect ----
threaded = common_head + '''
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
'''

for name, src in [("switch", switch), ("threaded", threaded)]:
    open(f"/tmp/thread-exp/{name}.wat","w").write(src)
    r = subprocess.run(["wat2wasm", f"/tmp/thread-exp/{name}.wat", "-o", f"/tmp/thread-exp/{name}.wasm"], capture_output=True, text=True)
    if r.returncode != 0:
        print(f"{name} wat2wasm FAILED:\n{r.stderr}"); continue
    print(f"{name}.wasm built")
