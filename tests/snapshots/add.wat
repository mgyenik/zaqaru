(module
  (type (;0;) (func))
  (type (;1;) (func (param i64 i64 i64 i64 i64 i64 f64 f64 f64 f64 f64 f64 f64 f64) (result i64 f64)))
  (import "env" "__linear_memory" (memory (;0;) 0))
  (import "env" "__stack_pointer" (global (;0;) (mut i32)))
  (global (;1;) (mut i64) i64.const 0)
  (global (;2;) (mut i64) i64.const 0)
  (global (;3;) (mut i64) i64.const 0)
  (global (;4;) (mut i64) i64.const 0)
  (global (;5;) (mut i64) i64.const 0)
  (global (;6;) (mut i64) i64.const 0)
  (global (;7;) (mut i64) i64.const 0)
  (global (;8;) (mut i64) i64.const 0)
  (global (;9;) (mut i64) i64.const 0)
  (global (;10;) (mut i64) i64.const 0)
  (global (;11;) (mut i64) i64.const 0)
  (global (;12;) (mut i64) i64.const 0)
  (global (;13;) (mut i64) i64.const 0)
  (global (;14;) (mut i64) i64.const 0)
  (global (;15;) (mut i64) i64.const 0)
  (global (;16;) (mut i64) i64.const 0)
  (global (;17;) (mut i64) i64.const 0)
  (global (;18;) (mut i64) i64.const 0)
  (global (;19;) (mut i64) i64.const 0)
  (global (;20;) (mut i64) i64.const 0)
  (global (;21;) (mut i64) i64.const 0)
  (global (;22;) (mut i64) i64.const 0)
  (global (;23;) (mut i64) i64.const 0)
  (global (;24;) (mut i64) i64.const 0)
  (global (;25;) (mut i64) i64.const 0)
  (global (;26;) (mut i64) i64.const 0)
  (global (;27;) (mut i64) i64.const 0)
  (global (;28;) (mut i64) i64.const 0)
  (global (;29;) (mut i64) i64.const 0)
  (global (;30;) (mut i64) i64.const 0)
  (global (;31;) (mut i64) i64.const 0)
  (global (;32;) (mut i64) i64.const 0)
  (global (;33;) (mut i64) i64.const 0)
  (global (;34;) (mut i64) i64.const 0)
  (global (;35;) (mut i64) i64.const 0)
  (global (;36;) (mut i64) i64.const 0)
  (global (;37;) (mut i64) i64.const 0)
  (global (;38;) (mut i64) i64.const 0)
  (global (;39;) (mut i64) i64.const 0)
  (global (;40;) (mut i64) i64.const 0)
  (global (;41;) (mut i64) i64.const 0)
  (global (;42;) (mut i64) i64.const 0)
  (global (;43;) (mut i64) i64.const 0)
  (global (;44;) (mut i64) i64.const 0)
  (global (;45;) (mut i64) i64.const 0)
  (global (;46;) (mut i64) i64.const 0)
  (global (;47;) (mut i64) i64.const 0)
  (global (;48;) (mut i64) i64.const 0)
  (global (;49;) (mut i32) i32.const 0)
  (global (;50;) (mut i32) i32.const 0)
  (global (;51;) (mut i32) i32.const 0)
  (global (;52;) (mut i32) i32.const 0)
  (global (;53;) (mut i32) i32.const 0)
  (func (;0;) (type 0)
    (local i64 i64 i64 i64 i32 i32 i32 i32 i32 i32)
    global.get 1
    local.set 0
    global.get 5
    local.set 1
    global.get 7
    local.set 2
    global.get 8
    local.set 3
    global.get 49
    local.set 4
    global.get 50
    local.set 5
    global.get 51
    local.set 6
    global.get 52
    local.set 7
    global.get 53
    local.set 8
    local.get 3
    local.get 2
    i64.add
    i32.wrap_i64
    local.set 9
    local.get 9
    i64.extend_i32_u
    local.set 0
    local.get 1
    i64.const 8
    i64.add
    local.set 1
    local.get 0
    global.set 1
    local.get 1
    global.set 5
    return
  )
  (func (;1;) (type 1) (param i64 i64 i64 i64 i64 i64 f64 f64 f64 f64 f64 f64 f64 f64) (result i64 f64)
    local.get 0
    global.set 8
    local.get 1
    global.set 7
    local.get 2
    global.set 3
    local.get 3
    global.set 2
    local.get 4
    global.set 9
    local.get 5
    global.set 10
    local.get 6
    i64.reinterpret_f64
    global.set 17
    local.get 7
    i64.reinterpret_f64
    global.set 19
    local.get 8
    i64.reinterpret_f64
    global.set 21
    local.get 9
    i64.reinterpret_f64
    global.set 23
    local.get 10
    i64.reinterpret_f64
    global.set 25
    local.get 11
    i64.reinterpret_f64
    global.set 27
    local.get 12
    i64.reinterpret_f64
    global.set 29
    local.get 13
    i64.reinterpret_f64
    global.set 31
    global.get 0
    i64.extend_i32_u
    i64.const -16
    i64.and
    i64.const 8
    i64.sub
    global.set 5
    global.get 5
    i32.wrap_i64
    i64.const 8818454208714178560
    i64.store
    call 0
    global.get 1
    global.get 17
    f64.reinterpret_i64
  )
  (@custom "linking" (after code) "\02\08\e4\058\02\10\00\02\01\01\07x86_rax\02\01\02\07x86_rcx\02\01\03\07x86_rdx\02\01\04\07x86_rbx\02\01\05\07x86_rsp\02\01\06\07x86_rbp\02\01\07\07x86_rsi\02\01\08\07x86_rdi\02\01\09\06x86_r8\02\01\0a\06x86_r9\02\01\0b\07x86_r10\02\01\0c\07x86_r11\02\01\0d\07x86_r12\02\01\0e\07x86_r13\02\01\0f\07x86_r14\02\01\10\07x86_r15\02\01\11\0bx86_xmm0_lo\02\01\12\0bx86_xmm0_hi\02\01\13\0bx86_xmm1_lo\02\01\14\0bx86_xmm1_hi\02\01\15\0bx86_xmm2_lo\02\01\16\0bx86_xmm2_hi\02\01\17\0bx86_xmm3_lo\02\01\18\0bx86_xmm3_hi\02\01\19\0bx86_xmm4_lo\02\01\1a\0bx86_xmm4_hi\02\01\1b\0bx86_xmm5_lo\02\01\1c\0bx86_xmm5_hi\02\01\1d\0bx86_xmm6_lo\02\01\1e\0bx86_xmm6_hi\02\01\1f\0bx86_xmm7_lo\02\01 \0bx86_xmm7_hi\02\01!\0bx86_xmm8_lo\02\01\22\0bx86_xmm8_hi\02\01#\0bx86_xmm9_lo\02\01$\0bx86_xmm9_hi\02\01%\0cx86_xmm10_lo\02\01&\0cx86_xmm10_hi\02\01'\0cx86_xmm11_lo\02\01(\0cx86_xmm11_hi\02\01)\0cx86_xmm12_lo\02\01*\0cx86_xmm12_hi\02\01+\0cx86_xmm13_lo\02\01,\0cx86_xmm13_hi\02\01-\0cx86_xmm14_lo\02\01.\0cx86_xmm14_hi\02\01/\0cx86_xmm15_lo\02\010\0cx86_xmm15_hi\02\011\06x86_zf\02\012\06x86_sf\02\013\06x86_cf\02\014\06x86_of\02\015\06x86_pf\00\04\00\09add_guest\00 \01\03add")
  (@custom "reloc.CODE" (after code) "\04\1f\07\08\01\07\10\05\07\18\07\07 \08\07(1\0702\0783\07@4\07H5\07f\01\07n\05\07{\08\07\83\01\07\07\8b\01\03\07\93\01\02\07\9b\01\09\07\a3\01\0a\07\ac\01\11\07\b5\01\13\07\be\01\15\07\c7\01\17\07\d0\01\19\07\d9\01\1b\07\e2\01\1d\07\eb\01\1f\07\f1\01\00\07\fe\01\05\07\84\02\05\00\99\026\07\9f\02\01\07\a5\02\11")
  (@custom "target_features" (after code) "\02+\0fmutable-globals+\08sign-ext")
)
