# web

A time-travel debugger for a container, in a browser: stand nginx, gunicorn
and Django on any instruction of a request the page itself sent.

The demo, which wants docker and about a minute:

    web/demo.sh                           # bakes demo/hello-django, boots it under Node, writes the snapshot
    python3 -m http.server -d . 8000
    # http://localhost:8000/web/?module=demo/hello-django.wasm&snapshot=demo/hello-django.snapshot&live=80
    # press play; send "GET / HTTP/1.0\r\n\r\n" to port 80; then drag the slider back into the request

The fixture and the tests:

    web/fixture.sh web/fixture            # a small program baked, run and recorded; a server, and its snapshot
    node web/test.mjs web/fixture/module.wasm web/fixture/tape.bin web/fixture/stdout.txt web/fixture/server.wasm
    node web/browser-test.mjs             # the page itself, in headless Chrome; drives the demo too when it exists
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
of booting one; history begins at the file's instant.

The panels are reads of the container's store: processes, registers, the
disassembly from `rip`, the stack under `rsp`, the memory map, descriptors,
the console. The syscall log holds rows for a window around the present, so
a run of a million syscalls stays quick.

Checkpoints are maps of non-zero 4 KiB pages, shared between checkpoints,
each recording only the pages that changed since the one before
(`checkpoints.js`): a container's memory is hundreds of megabytes and
almost all of it is zero or unchanging. Seeking to an instant reconstructs
the nearest checkpoint at or before it and runs, interpreted, to the exact
instruction. Everything the page shows is a
read of the container's own store — the isotope Server Protocol — through
`/iso/server`.
