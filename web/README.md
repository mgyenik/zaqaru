# web

A time-travel debugger for a container, in a browser: replay a recorded
run and stand the machine on any instruction.

    web/fixture.sh web/fixture            # a small program, baked, run and recorded
    node web/test.mjs web/fixture/module.wasm web/fixture/tape.bin web/fixture/stdout.txt
    python3 -m http.server -d . 8000
    # then open http://localhost:8000/web/?module=fixture/module.wasm&tape=fixture/tape.bin

`zaqaru.js` is the host: the two imports the module needs, a mount table, a
tape to replay from, and snapshot and restore. It runs under Node too, which
is what `test.mjs` uses to check it against the wasmtime host's own run.
`worker.js` owns the container and its checkpoints; `app.js` is the page.

Seeking to an instant restores the nearest checkpoint at or before it and
runs, interpreted, to the exact instruction. Everything the page shows is a
read of the container's own store — the isotope Server Protocol — through
`/iso/server`.
