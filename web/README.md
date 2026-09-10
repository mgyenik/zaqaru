# web

A time-travel debugger for a container, in a browser: stand nginx, gunicorn
and Django on any instruction of a request the page itself sent.

The demo is published at <https://mgyenik.github.io/zaqaru/>, built from
source by `.github/workflows/pages.yml` on every push to `main`: the
workflow runs `web/demo.sh`, checks the snapshot answers `GET /` with
Django's page (`check-demo.mjs`), and deploys it beside the page and the
landing page in `web/pages/`. Nothing built is committed.

To make the demo locally, which wants docker and about a minute:

    web/demo.sh                           # bakes demo/hello-django, boots it under Node, writes the snapshot
    python3 -m http.server -d . 8000
    # http://localhost:8000/web/?module=demo/hello-django.wasm&snapshot=demo/hello-django.snapshot&live=80
    # press play; send "GET / HTTP/1.0\r\n\r\n" to port 80; then drag the slider back into the request

The fixture and the tests:

    web/fixture.sh web/fixture            # a small program baked, run and recorded; a server, and its snapshot
    node web/test.mjs web/fixture/module.wasm web/fixture/tape.bin web/fixture/stdout.txt web/fixture/server.wasm
    node web/browser-test.mjs             # the page itself, in headless Chrome; drives the demo too when it exists
    node web/browser-test.mjs --browser firefox --driver firefox.geckodriver   # the same in Firefox (snap)
    # replay:    http://localhost:8000/web/?module=fixture/module.wasm&tape=fixture/tape.bin
    # live:      http://localhost:8000/web/?module=fixture/server.wasm&live=8080
    #            then press play and send "ping\n" to port 8080
    # continued: http://localhost:8000/web/?module=fixture/server.wasm&snapshot=fixture/server.snapshot&live=8080

`zaqaru.js` is the host: the two imports the module needs, a mount table, a
tape to replay from or a recording to make, an edge the page sends requests
through, and snapshot and restore. It runs under Node too, which is what
`test.mjs` uses to check it against the wasmtime host's own run.
`worker.js` owns the container and its checkpoints; `app.js` is the page.

Without a tape the container runs live against the page's clock and
entropy; everything the host answers is recorded, so the slider still seeks
into the run's past by re-executing from a checkpoint against the
recording. The edge panel sends a request to a listener inside the
container and shows what came back, with the instant it was answered as a
link. A snapshot (`snapshot.js`, written by `preboot.mjs` once a container
has booted and gone quiet) starts the live run from a booted server instead
of booting one; history begins at the file's instant. Before writing the
file the tool asks the kernel to flush its block caches and zero its page
pool, and leaves out the decompressed files' buffers and the guest pages no
process can reach; the page refills the files when it continues. That is
what takes Django's file from 75 MB to 39 MB as gzip, and `--brotli 10`
takes it to 29, inflated by `brotli.wasm` — a decoder built from
`crates/brotli` by the fixture and demo scripts, since browsers inflate
gzip natively and brotli not at all.

The page is a store browser with a time axis. The panels come from the
container's own `meta` lens — the worker reads every path the lens
declares that the instant can fill, so a path the kernel adds appears on
the page unasked — and each is titled by the path it read, with the raw
JSON value a click away; a path bar reads any path at the instant in view.
Processes, the process's files (browsed as they stood at the instant),
its descriptors with the paths they were opened by, and the container's
sockets stand in the open; registers, the disassembly from `rip`, the
stack under `rsp`, the memory map and the caches fold under "the
machine". Clicking a process card views that process's paths. The
timeline shows the container's exchanges with the host — every read and
write under `/iso`, with what crossed — beside its syscalls, each placed
at the syscall it was made in, and holds rows for a window around the
present so a run of a million events stays quick. Pinning an instant makes
every panel show what changed since it. Processes are named by what they
were started from and coloured, on their cards, on the timeline's rows
and on a lane strip under the slider that shows which ran when; a syscall
row's paths and descriptors are links into the files, descriptors and net
panels, and clicking a row opens what it is about. Live, the page opens on
the edge box alone and, when the first answer arrives, stands the machine
on the instant the request came in.

Checkpoints are maps of non-zero 4 KiB pages, shared between checkpoints,
each recording only the pages that changed since the one before
(`checkpoints.js`): a container's memory is hundreds of megabytes and
almost all of it is zero or unchanging. Seeking to an instant reconstructs
the nearest checkpoint at or before it and runs, interpreted, to the exact
instruction. Everything the page shows is a
read of the container's own store — the isotope Server Protocol — through
`/iso/server`.
