# zaqaru

Run x86-64 OCI containers in WebAssembly.

`zaqaru bake` turns a container image — a `docker save` tarball, or any
root directory — into one `.wasm` module. `zaqaru run` runs it. Inside the
module an x86-64 interpreter executes the image's programs exactly as they
shipped, and a Linux-personality kernel serves their syscalls without
leaving the module. The host supplies two functions: a store to read from
and write to. Everything else — files, memory, processes, threads, sockets
between them — is inside.

The name is Akkadian, *zaqāru*: to build high, as a tower is built.

```sh
docker build -t hello-django demo/hello-django
docker save hello-django -o hello-django.tar
zaqaru bake hello-django.tar -o hello-django.wasm
zaqaru run hello-django.wasm -p 8080:80
curl http://localhost:8080/
```

That is nginx, gunicorn and Django — five processes, an ordinary Debian
image, nothing rebuilt — served from a wasm module by wasmtime.

## The tool

```
zaqaru bake    <image.tar | rootfs/> [-o out.wasm] [--env NAME=value]... [-- command...]
zaqaru run     <module.wasm> [-p HOST:GUEST]... [--trace FILE] [--record TAPE | --replay TAPE]
               [--no-bytecode] [--seed BYTE] [--perfmap]
zaqaru emulate <image.tar | rootfs/> [-p HOST:GUEST]... [--env NAME=value]... [--trace]
               [--no-bytecode] [--profile-out FILE] [-- command...]
zaqaru image inspect <image.tar | rootfs/> [--list]
```

- **bake** reads the image's layers, entrypoint, command, environment and
  working directory, flattens the filesystem into a packed index and a
  blob, and links them with the guest archive — the kernel, the
  interpreter and the FPU, compiled once for wasm32 and embedded in the
  binary. It needs `wasm-ld` and nothing else. Arguments after `--`
  replace the image's command, as `docker run image cmd...` does.
- **run** instantiates the module under wasmtime with a console, a clock,
  entropy, a shutdown switch, and — with `-p` — a network edge that
  publishes guest ports. `--record` keeps every answer the host gave;
  `--replay` runs the same container again from that tape, with no network
  at all, byte for byte. Ctrl-C is the container's `SIGTERM`.
- **emulate** runs the same kernel and interpreter natively, without a
  module: two to three times faster, and where a native profiler sees the
  engine's own frames. A development instrument; its numbers are not the
  container's.
- **image inspect** says what an image would boot as.

A container is deterministic: the scheduler switches on a count of retired
instructions, `rdtsc` answers from the same count, and everything else —
time, entropy, the network — arrives as a store read. Two runs with the
same tape are the same run.

## The debugger

`web/` is a time-travel debugger for a container, in a browser: load a
module and a tape of one of its runs, drag a slider through the run, and
stand the machine on any instruction — the processes, a thread's
registers, the memory map, the descriptors and the console as of that
instant, with the syscall log as a clickable time axis. It rests on two
facts about a container: every run is a pure function of its tape, and
between two instructions linear memory is the whole machine. See
[web/README.md](web/README.md) and
[docs/time-travel-debugger.md](docs/time-travel-debugger.md).

## Layout

```
crates/cpu      the x86-64 machine: interpreter, block cache, lazy flags, page table,
                and the bytecode accelerator hot blocks are transpiled into
crates/kernel   the Linux-personality kernel: syscalls, VFS and overlay, processes,
                threads, signals, sockets, the packed image format
crates/x87      the soft x87 FPU: extended precision and the register stack
crates/guest    the container module's guest side, as one wasm32 staticlib:
                the boot export and the two host imports over the kernel
crates/image    the packager: OCI layers or a directory, into the image the kernel reads
crates/bake     the link: an image and the guest archive become a module
crates/host     the wasmtime host: the two imports, the mount table, the network edge
crates/cli      the zaqaru binary
web/            the time-travel debugger: the host in JavaScript, and the page
demo/           the nginx + gunicorn + Django image, and the scripts that trace and replay it
tools/          the microbenchmarks
docs/           architecture, fidelity, performance
```

## Building

Rust with the `wasm32-unknown-unknown` target, and `wasm-ld` (from LLVM)
on the path. The tests also want `gcc`, `clang` and `ldd`.

```sh
rustup target add wasm32-unknown-unknown
cargo build --release -p zaqaru
target/release/zaqaru --help
```

The build compiles the guest archive for wasm32 and embeds it; the first
build takes a minute for that.

## Documentation

- [docs/architecture.md](docs/architecture.md) — how it works: the
  machine, the kernel, the host boundary, the bake, determinism.
- [docs/fidelity.md](docs/fidelity.md) — where the kernel differs from
  Linux, and what a program would have to do to notice.
- [docs/performance.md](docs/performance.md) — what a container costs,
  where the time goes, how it is measured, and what was tried.
- [docs/archive/](docs/archive/) — the design and planning documents the
  project was built from, kept as dated records.
