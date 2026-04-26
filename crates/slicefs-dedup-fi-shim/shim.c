// Test-only DYLD_INSERT_LIBRARIES shim. macOS only.
// Turns fcntl(fd, F_FULLFSYNC) into a no-op. All other fcntl commands
// are forwarded to the real fcntl via dlsym.

#ifdef __APPLE__

#include <fcntl.h>
#include <stdarg.h>
#include <stddef.h>
#include <stdint.h>
#include <dlfcn.h>

typedef int (*fcntl_real_t)(int, int, ...);
static fcntl_real_t fcntl_real = NULL;

int fcntl(int fd, int cmd, ...) {
    if (cmd == F_FULLFSYNC) {
        return 0; // no-op the F_FULLFSYNC call
    }
    if (!fcntl_real) {
        fcntl_real = (fcntl_real_t)dlsym(RTLD_NEXT, "fcntl");
        if (!fcntl_real) return -1;
    }
    // Forward up to one extra arg (most fcntl commands take 0 or 1 extra).
    va_list ap;
    va_start(ap, cmd);
    void *arg = va_arg(ap, void*);
    va_end(ap);
    return fcntl_real(fd, cmd, arg);
}

#endif
