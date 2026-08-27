/* A function pointer that crosses an object boundary: this object takes the
   address of a local function and hands it out; the other one calls through
   it. The table slot is assigned by the linker, so neither object knows it. */

typedef int (*handler)(int value);

static int tripled(int value) { return value * 3; }
static int halved(int value) { return value / 2; }

handler installed_handler(int which) { return which ? halved : tripled; }
