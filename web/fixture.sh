#!/bin/sh
# Makes the fixture web/test.mjs runs against: a small forking program baked
# into a module, and a recorded, traced run of it.
#
#   web/fixture.sh <out-dir>
set -eu
out=${1:?usage: web/fixture.sh <out-dir>}
repo=$(cd "$(dirname "$0")/.." && pwd)
mkdir -p "$out/root"
cat > "$out/program.c" <<'EOF'
#include <stdio.h>
#include <unistd.h>
#include <sys/wait.h>
int main(int argc, char **argv) {
    int fd[2];
    pipe(fd);
    pid_t child = fork();
    if (child == 0) {
        long sum = 0;
        for (long i = 0; i < 2000000; i++) sum += i * 7;
        char text[32];
        int length = snprintf(text, sizeof text, "%ld", sum);
        write(fd[1], text, length);
        _exit(4);
    }
    close(fd[1]);
    char text[64];
    int length = read(fd[0], text, sizeof text - 1);
    text[length] = 0;
    int status;
    waitpid(child, &status, 0);
    printf("child said %s and exited %d; %d args\n", text, WEXITSTATUS(status), argc - 1);
    FILE *missing = fopen("/etc/nothing-here", "r");
    printf("open of a missing file: %s\n", missing ? "succeeded" : "failed");
    return 0;
}
EOF
gcc -O1 -static -no-pie -fcf-protection=none -fno-stack-protector -fno-asynchronous-unwind-tables \
    -o "$out/root/init" "$out/program.c"
# And a server, for the live mode's edge: it listens on 8080, answers one
# connection with "pong", and exits.
mkdir -p "$out/server"
cat > "$out/server.c" <<'EOF'
#include <netinet/in.h>
#include <poll.h>
#include <stdio.h>
#include <sys/socket.h>
#include <unistd.h>
int main(void) {
    int server = socket(AF_INET, SOCK_STREAM, 0);
    int one = 1;
    setsockopt(server, SOL_SOCKET, SO_REUSEADDR, &one, sizeof one);
    struct sockaddr_in mine = {0};
    mine.sin_family = AF_INET;
    mine.sin_addr.s_addr = htonl(INADDR_ANY);
    mine.sin_port = htons(8080);
    if (bind(server, (struct sockaddr *)&mine, sizeof mine) != 0) { perror("bind"); return 1; }
    if (listen(server, 8) != 0) { perror("listen"); return 1; }
    printf("listening on 8080\n");
    fflush(stdout);
    struct pollfd waiting = { .fd = server, .events = POLLIN };
    if (poll(&waiting, 1, 600000) <= 0) { printf("nobody came\n"); return 0; }
    int client = accept(server, 0, 0);
    if (client < 0) { perror("accept"); return 1; }
    char asked[64] = {0};
    ssize_t got = read(client, asked, sizeof asked - 1);
    printf("read %zd: %s", got, asked);
    write(client, "pong\n", 5);
    close(client);
    close(server);
    return 0;
}
EOF
gcc -O1 -static -no-pie -fcf-protection=none -fno-stack-protector -fno-asynchronous-unwind-tables \
    -o "$out/server/init" "$out/server.c"
cargo build --release --quiet -p zaqaru --manifest-path "$repo/Cargo.toml"
"$repo/target/release/zaqaru" bake "$out/server" -o "$out/server.wasm"
"$repo/target/release/zaqaru" bake "$out/root" -o "$out/module.wasm" -- /init a b
"$repo/target/release/zaqaru" run --trace "$out/trace.txt" --record "$out/tape.bin" --seed 51 \
    "$out/module.wasm" > "$out/stdout.txt" 2> "$out/stderr.txt" || true
echo "fixture in $out: module.wasm, tape.bin, trace.txt, stdout.txt, server.wasm"
