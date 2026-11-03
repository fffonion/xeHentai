/* Controlled resolver input for the default-RPC-hostname bind regression.
 * Test-only: compiled by the regression driver and LD_PRELOADed into that one
 * bounded child process; never installed, never a product mode.
 *
 * For an AF_UNSPEC `localhost` query it returns libc's real numeric loopback
 * answers with `::1` ordered first, reproducing the dual-stack resolver input
 * under which a hostname handed straight to `TcpListener::bind` listens on
 * IPv6 loopback only. Every other query (including AF_INET `localhost`) takes
 * the ordinary libc path. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <netdb.h>
#include <string.h>

int getaddrinfo(const char *node, const char *service,
                const struct addrinfo *hints, struct addrinfo **res) {
    static int (*real_getaddrinfo)(const char *, const char *,
                                   const struct addrinfo *,
                                   struct addrinfo **) = 0;
    if (!real_getaddrinfo) {
        real_getaddrinfo = dlsym(RTLD_NEXT, "getaddrinfo");
    }
    if (node && strcmp(node, "localhost") == 0 &&
        (!hints || hints->ai_family == AF_UNSPEC)) {
        struct addrinfo opt = {0};
        struct addrinfo *v6 = 0, *v4 = 0, *tail;
        if (hints) opt = *hints;
        opt.ai_flags = (opt.ai_flags | AI_NUMERICHOST) & ~AI_ADDRCONFIG;
        opt.ai_family = AF_INET6;
        int rc = real_getaddrinfo("::1", service, &opt, &v6);
        if (rc) return rc;
        opt.ai_family = AF_INET;
        rc = real_getaddrinfo("127.0.0.1", service, &opt, &v4);
        if (rc) { freeaddrinfo(v6); return rc; }
        tail = v6;
        while (tail->ai_next) tail = tail->ai_next;
        tail->ai_next = v4;
        *res = v6;
        return 0;
    }
    return real_getaddrinfo(node, service, hints, res);
}
