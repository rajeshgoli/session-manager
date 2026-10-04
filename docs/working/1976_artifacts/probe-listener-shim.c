#include <arpa/inet.h>
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <unistd.h>

static int private_listener(int fd) {
    struct sockaddr_in bound;
    socklen_t size = sizeof(bound);
    int type = 0;
    socklen_t option_size = sizeof(type);
    return getsockname(fd, (struct sockaddr *)&bound, &size) == 0
        && bound.sin_family == AF_INET
        && bound.sin_addr.s_addr == htonl(INADDR_LOOPBACK)
        && bound.sin_port != 0
        && getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &option_size) == 0
        && type == SOCK_STREAM;
}

static int wrapped_bind(int fd, const struct sockaddr *address, socklen_t size) {
    const char *value = getenv("SM_LOOPBACK_LISTENER_FD");
    if (value && size >= sizeof(struct sockaddr_in) && address->sa_family == AF_INET) {
        char *end;
        long source = strtol(value, &end, 10);
        const struct sockaddr_in *requested = (const struct sockaddr_in *)address;
        struct sockaddr_in bound;
        socklen_t bound_size = sizeof(bound);
        if (*end == 0 && source >= 3 && source <= 1024 && private_listener((int)source)
            && requested->sin_addr.s_addr == htonl(INADDR_LOOPBACK)
            && getsockname((int)source, (struct sockaddr *)&bound, &bound_size) == 0
            && (!requested->sin_port || requested->sin_port == bound.sin_port)) {
            int old_flags = fcntl(fd, F_GETFL);
            if (dup2((int)source, fd) < 0) return -1;
            if (old_flags >= 0) fcntl(fd, F_SETFL, old_flags);
            fcntl(fd, F_SETFD, FD_CLOEXEC);
            fcntl((int)source, F_SETFD, FD_CLOEXEC);
            return 0;
        }
    }
    return bind(fd, address, size);
}

static int wrapped_listen(int fd, int backlog) {
    if (private_listener(fd)) return 0;
    return listen(fd, backlog);
}

__attribute__((used)) static struct { const void *replacement; const void *original; }
bind_interpose __attribute__((section("__DATA,__interpose"))) = {wrapped_bind, bind};
__attribute__((used)) static struct { const void *replacement; const void *original; }
listen_interpose __attribute__((section("__DATA,__interpose"))) = {wrapped_listen, listen};
