/* LD_PRELOAD shim for the power-loss test (see README.md).
 * It counts the sync-type calls of the process: fsync, fdatasync, rename,
 * renameat and renameat2. With PL_KILL_AT=N (N >= 1) the process kills
 * itself with SIGKILL just before its N-th such call, so that call and
 * all later ones never run. PL_LOG=<file> gets one line per call
 * ("<n> <op> <path>"), and "KILL <n>" before the kill.
 * The log is a plain append with no sync; it must not be on the file
 * system under test. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

static atomic_long counter;

static void logline(const char *line) {
    const char *p = getenv("PL_LOG");
    if (!p) return;
    int fd = open(p, O_WRONLY | O_APPEND | O_CREAT | O_CLOEXEC, 0644);
    if (fd < 0) return;
    ssize_t r = write(fd, line, strlen(line));
    (void)r;
    close(fd);
}

static void point(const char *op, const char *path) {
    long n = atomic_fetch_add(&counter, 1) + 1;
    const char *k = getenv("PL_KILL_AT");
    long kill_at = k ? atol(k) : 0;
    char line[4200];
    snprintf(line, sizeof line, "%ld %s %s\n", n, op, path ? path : "?");
    logline(line);
    if (kill_at > 0 && n == kill_at) {
        snprintf(line, sizeof line, "KILL %ld\n", n);
        logline(line);
        kill(getpid(), SIGKILL);
        for (;;) pause();
    }
}

static void fd_path(int fd, char *out, size_t len) {
    char link[64];
    snprintf(link, sizeof link, "/proc/self/fd/%d", fd);
    ssize_t r = readlink(link, out, len - 1);
    out[r < 0 ? 0 : r] = 0;
}

#define REAL(name, ret, ...) \
    static ret (*real)(__VA_ARGS__); \
    if (!real) real = (ret (*)(__VA_ARGS__))dlsym(RTLD_NEXT, name)

int fsync(int fd) {
    REAL("fsync", int, int);
    char p[4096];
    fd_path(fd, p, sizeof p);
    point("fsync", p);
    return real(fd);
}

int fdatasync(int fd) {
    REAL("fdatasync", int, int);
    char p[4096];
    fd_path(fd, p, sizeof p);
    point("fdatasync", p);
    return real(fd);
}

int rename(const char *from, const char *to) {
    REAL("rename", int, const char *, const char *);
    point("rename", to);
    return real(from, to);
}

int renameat(int a, const char *from, int b, const char *to) {
    REAL("renameat", int, int, const char *, int, const char *);
    point("renameat", to);
    return real(a, from, b, to);
}

int renameat2(int a, const char *from, int b, const char *to, unsigned int flags) {
    REAL("renameat2", int, int, const char *, int, const char *, unsigned int);
    point("renameat2", to);
    return real(a, from, b, to, flags);
}
