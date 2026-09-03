// Faithful register-machine bytecode interp with a `mixed` kernel mirroring
// the C one op-for-op: LCG arithmetic, permission-checked load/store into a
// 16 KB region, and a data-dependent unpredictable branch.
struct Vm { code: Vec<u64>, regs: [u64;16], mem: Vec<u8>, perm: Vec<u64> }
#[inline(always)]
fn permitted(perm:&[u64], addr:u64, len:u64)->bool{
    let p=(addr>>12) as usize; let q=((addr+len-1)>>12) as usize;
    (perm[p>>6]>>(p&63))&1==1 && (perm[q>>6]>>(q&63))&1==1
}
#[inline(never)]
fn run(vm:&mut Vm)->u64{
    let mut pc=0usize; let mut retired=0u64; let base=vm.mem.as_mut_ptr();
    loop{
        let w=unsafe{*vm.code.get_unchecked(pc)}; pc+=1; retired+=1;
        let op=(w&0xff) as usize;
        let d=((w>>8)&0xf) as usize; let a=((w>>16)&0xf) as usize; let b=((w>>24)&0xf) as usize;
        let imm=w>>32;
        match op{
            0=>return retired,
            1=>vm.regs[d]=vm.regs[a].wrapping_add(vm.regs[b]),
            2=>vm.regs[d]=vm.regs[a].wrapping_add(imm),
            3=>vm.regs[d]=vm.regs[a].wrapping_sub(vm.regs[b]),
            4=>vm.regs[d]=vm.regs[a].wrapping_mul(vm.regs[b]),
            5=>vm.regs[d]=vm.regs[a]^vm.regs[b],
            6=>vm.regs[d]=vm.regs[a]&vm.regs[b],
            8=>vm.regs[d]=vm.regs[a]>>(vm.regs[b]&63),
            9=>vm.regs[d]=vm.regs[a],
            10=>vm.regs[d]=imm,
            11=>{let addr=vm.regs[a].wrapping_add(imm); if !permitted(&vm.perm,addr,8){return retired;} vm.regs[d]=unsafe{(base.add(addr as usize) as *const u64).read_unaligned()};}
            12=>{let addr=vm.regs[a].wrapping_add(imm); if !permitted(&vm.perm,addr,8){return retired;} unsafe{(base.add(addr as usize) as *mut u64).write_unaligned(vm.regs[b]);}}
            14=>if vm.regs[a]!=0 {pc=imm as usize;},
            15=>if vm.regs[a]==0 {pc=imm as usize;},
            16=>{vm.regs[d]=unsafe{*vm.code.get_unchecked(pc)}; pc+=1;}   // LI64: next word
            _=>unreachable!(),
        }
    }
}
fn enc(op:u64,d:u64,a:u64,b:u64,imm:u64)->u64{(op&0xff)|((d&0xf)<<8)|((a&0xf)<<16)|((b&0xf)<<24)|(imm<<32)}
fn main(){
    let scale:u64=std::env::args().nth(1).and_then(|s|s.parse().ok()).unwrap_or(200000);
    let mut c:Vec<u64>=Vec::new();
    c.push(enc(16,5,0,0,0)); c.push(6364136223846793005);   // r5 = K
    c.push(enc(16,6,0,0,0)); c.push(1442695040888963407);   // r6 = C
    c.push(enc(10,2,0,0,1));      // r2 state=1
    c.push(enc(10,9,0,0,33));     // r9=33
    c.push(enc(10,10,0,0,0x3ff8));// r10 mask
    c.push(enc(10,11,0,0,0x100)); // r11
    c.push(enc(10,12,0,0,64));    // r12
    c.push(enc(10,13,0,0,24));    // r13 stride
    c.push(enc(10,14,0,0,1));     // r14
    c.push(enc(10,15,0,0,scale)); // r15 counter
    let body=c.len() as u64;
    c.push(enc(6,8,1,10,0));   // r8 = idx & mask
    c.push(enc(11,4,8,0,0));   // x = [r8]
    c.push(enc(4,2,2,5,0));    // state *= K
    c.push(enc(1,2,2,6,0));    // state += C
    c.push(enc(8,7,2,9,0));    // r7 = state >> 33
    c.push(enc(5,4,4,7,0));    // x ^= r7
    c.push(enc(1,8,1,12,0));   // r8 = idx + 64
    c.push(enc(6,8,8,10,0));   // r8 &= mask
    c.push(enc(12,0,8,4,0));   // [r8] = x
    c.push(enc(6,7,4,11,0));   // r7 = x & 0x100
    let brz=c.len(); c.push(0);// BRZ placeholder
    c.push(enc(1,3,3,4,0));    // sum += x
    let skip=c.len() as u64; c[brz]=enc(15,0,7,0,skip);
    c.push(enc(1,1,1,13,0));   // idx += 24
    c.push(enc(3,15,15,14,0)); // counter--
    c.push(enc(14,0,15,0,body));// BRNZ -> body
    c.push(enc(0,0,0,0,0));
    let ops_per_iter = 15u64; // body ops (14 + ~1 taken half the time)
    let mut mem=vec![0u8;1<<20]; // 1 MB
    for i in 0..2048usize { let v=(i as u64)*2654435761+1; mem[i*8..i*8+8].copy_from_slice(&v.to_le_bytes()); }
    let perm=vec![u64::MAX; (1<<20)/4096/64 + 1];
    let mut vm=Vm{code:c, regs:[0;16], mem, perm};
    let start=std::time::Instant::now();
    let retired=run(&mut vm);
    let secs=start.elapsed().as_secs_f64();
    let _=ops_per_iter;
    println!("mixed: {retired} ops in {secs:.3}s = {:.1} MIPS (sum={:#x})", retired as f64/secs/1e6, vm.regs[3]);
}
