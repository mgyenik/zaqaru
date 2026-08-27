/* The other half of the cross-object function-pointer case. */

typedef int (*handler)(int value);

handler installed_handler(int which);

int run_installed(int which, int value) { return installed_handler(which)(value); }
