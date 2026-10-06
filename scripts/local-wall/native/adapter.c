#include "wire_client.h"
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/tcp.h>
#include <pthread.h>
#include <stdarg.h>
#include <stdatomic.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>
#include <poll.h>
#include <sys/un.h>
#include <limits.h>

extern const struct wall_configuration wall_configuration;
extern const uint16_t wall_direct_ports[4];
extern const uint16_t wall_control_port;
extern const int wall_control_fd;

#define TRACKED_LIMIT 1024
struct entry { int fd; struct wall_socket socket; };
static struct entry entries[TRACKED_LIMIT];
static pthread_mutex_t table_lock = PTHREAD_MUTEX_INITIALIZER;
static int refresh_after_fork;
static int control_source = -1;
static int fork_pending;
/* libSystem invokes interposed functions before this image's constructor.
 * No pthread operation or thread-local access is safe in that startup window. */
static _Atomic int initialized;
static int adapter_ready(void) { return atomic_load_explicit(&initialized, memory_order_acquire); }
static _Thread_local int cancellation_state;
static void lock_table(void) {
    pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, &cancellation_state);
    pthread_mutex_lock(&table_lock);
}
static void unlock_table(void) {
    pthread_mutex_unlock(&table_lock);
    pthread_setcancelstate(cancellation_state, NULL);
}

static int failure(int code) { errno = code; return -1; }
static struct entry *find(int fd) {
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) if (entries[i].fd == fd) return &entries[i];
    return NULL;
}
static struct entry *vacant(void) {
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) if (entries[i].fd < 0) return &entries[i];
    errno = EMFILE;
    return NULL;
}
static void forget(struct entry *entry) {
    if (!entry) return;
    int saved = errno;
    if (entry->socket.control >= 0) {
        int final = entry->socket.lease != 0 && !fork_pending;
        for (unsigned i = 0; i < TRACKED_LIMIT; ++i)
            if (&entries[i] != entry && entries[i].fd >= 0 &&
                entries[i].socket.lease == entry->socket.lease) final = 0;
        if (final) {
            struct wall_socket releasing = entry->socket;
            releasing.descriptor = -1;
            (void)wall_release(&releasing);
        } else close(entry->socket.control);
    }
    entry->fd = -1;
    errno = saved;
}

/* Returns protocol family/port only for exact loopback. No hostname, wildcard,
 * mapped IPv6, or alternative 127/8 address can become a broker request. */
static int destination(const struct sockaddr *address, socklen_t size,
                       uint8_t *family, uint16_t *port) {
    if (!address || size < sizeof(sa_family_t) + 1) return failure(EINVAL);
    if (address->sa_family == AF_INET) {
        if (size < sizeof(struct sockaddr_in)) return failure(EINVAL);
        const struct sockaddr_in *v4 = (const struct sockaddr_in *)address;
        if (v4->sin_addr.s_addr != htonl(INADDR_LOOPBACK)) return failure(EACCES);
        *family = 1; *port = ntohs(v4->sin_port);
    } else if (address->sa_family == AF_INET6) {
        if (size < sizeof(struct sockaddr_in6)) return failure(EINVAL);
        const struct sockaddr_in6 *v6 = (const struct sockaddr_in6 *)address;
        if (memcmp(&v6->sin6_addr, &in6addr_loopback, sizeof(in6addr_loopback)) || v6->sin6_scope_id) return failure(EACCES);
        *family = 2; *port = ntohs(v6->sin6_port);
    } else return failure(EAFNOSUPPORT);
    return 0;
}

static int tcp_socket(int fd, uint8_t family) {
    int type = 0;
    socklen_t size = sizeof(type);
    if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &size) < 0) return -1;
    if (size != sizeof(type) || type != SOCK_STREAM) return failure(EPROTOTYPE);
    struct sockaddr_storage current;
    size = sizeof(current);
    if (getsockname(fd, (struct sockaddr *)&current, &size) < 0) return -1;
    if (current.ss_family != (family == 1 ? AF_INET : AF_INET6)) return failure(EAFNOSUPPORT);
    return 0;
}

static int copy_options(int source, int target) {
    const int options[] = { SO_KEEPALIVE, SO_REUSEADDR, SO_RCVBUF, SO_SNDBUF, SO_NOSIGPIPE };
    for (unsigned i = 0; i < sizeof(options) / sizeof(options[0]); ++i) {
        int value;
        socklen_t size = sizeof(value);
        if (getsockopt(source, SOL_SOCKET, options[i], &value, &size) < 0 ||
            setsockopt(target, SOL_SOCKET, options[i], &value, size) < 0) return -1;
    }
    const int times[] = { SO_RCVTIMEO, SO_SNDTIMEO };
    for (unsigned i = 0; i < sizeof(times) / sizeof(times[0]); ++i) {
        struct timeval value;
        socklen_t size = sizeof(value);
        if (getsockopt(source, SOL_SOCKET, times[i], &value, &size) < 0 ||
            setsockopt(target, SOL_SOCKET, times[i], &value, size) < 0) return -1;
    }
    int value;
    socklen_t size = sizeof(value);
    if (getsockopt(source, IPPROTO_TCP, TCP_NODELAY, &value, &size) < 0 ||
        setsockopt(target, IPPROTO_TCP, TCP_NODELAY, &value, size) < 0) return -1;
    return 0;
}

/* Called under the table lock. A failed replacement never leaves a new lease
 * attached to the original application descriptor. */
static int install(int fd, struct wall_socket *prepared, int track) {
    struct entry *slot = track ? vacant() : NULL;
    if (track && !slot) return -1;
    int flags = fcntl(fd, F_GETFL), descriptor_flags = fcntl(fd, F_GETFD);
    if (flags < 0 || descriptor_flags < 0) return -1;
    if (copy_options(fd, prepared->descriptor) < 0 ||
        fcntl(prepared->descriptor, F_SETFL, flags) < 0 ||
        (prepared->control >= 0 && fcntl(prepared->control, F_SETFD, descriptor_flags) < 0)) return -1;
    if (dup2(prepared->descriptor, fd) < 0) return -1;
    close(prepared->descriptor);
    prepared->descriptor = -1;
    if (fcntl(fd, F_SETFD, descriptor_flags) < 0) {
        int saved = errno; close(fd); return failure(saved);
    }
    if (slot) {
        slot->fd = fd; slot->socket = *prepared; slot->socket.descriptor = fd;
        prepared->control = -1;
    }
    return 0;
}

/* A forked process must not transact on its parent's shared Unix byte stream.
 * Acquire a fresh same-agent lease once per listener, then duplicate that
 * control descriptor for local duplicates. No allocation or pthread lock is
 * performed in the atfork child callback itself. */
static int refresh(void) {
    if (!refresh_after_fork) return 0;
    unsigned char refreshed[TRACKED_LIMIT] = {0};
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) {
        struct entry *entry = &entries[i];
        if (refreshed[i] || entry->fd < 0 || entry->socket.control < 0) continue;
        int descriptor_flags = fcntl(entry->fd, F_GETFD);
        if (descriptor_flags < 0) { forget(entry); continue; }
        struct wall_socket retained;
        if (wall_retain(&wall_configuration, entry->socket.family, entry->socket.port, &retained) < 0) return -1;
        close(retained.descriptor);
        retained.descriptor = entry->fd;
        if (fcntl(retained.control, F_SETFD, descriptor_flags) < 0) { wall_dispose(&retained); entry->fd = -1; return -1; }
        uint64_t lease = entry->socket.lease;
        int old_control = entry->socket.control;
        entry->socket = retained;
        close(old_control);
        for (unsigned j = i + 1; j < TRACKED_LIMIT; ++j) {
            struct entry *other = &entries[j];
            if (other->fd < 0 || other->socket.control < 0 || other->socket.lease != lease) continue;
            int flags = fcntl(other->fd, F_GETFD);
            int control = dup(retained.control);
            if (control < 0 || flags < 0 || fcntl(control, F_SETFD, flags) < 0) {
                int saved = errno; if (control >= 0) close(control); return failure(saved);
            }
            close(other->socket.control);
            other->socket.control = control;
            refreshed[j] = 1;
        }
    }
    refresh_after_fork = 0;
    return 0;
}

static void hold_fork_leases(void);
static void fork_prepare(void) { lock_table(); hold_fork_leases(); }
static void fork_parent(void) { unlock_table(); }
static void fork_child(void) {
    /* Drop kernel-closed entries before application code can reuse their
     * descriptor numbers. fcntl is async-signal-safe; no IPC occurs here. */
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i)
        if (entries[i].fd >= 0 && fcntl(entries[i].fd, F_GETFD) < 0)
            entries[i].fd = -1;
    refresh_after_fork = 1;
    unlock_table();
}

#include "exec_recovery.c"

__attribute__((constructor)) static void initialize(void) {
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) entries[i].fd = -1;
    if (wall_control_fd >= 0) {
        struct sockaddr_storage address;
        socklen_t size = sizeof(address);
        uint8_t family; uint16_t port;
        int flags = fcntl(wall_control_fd, F_GETFD);
        if (flags < 0 && errno != EBADF) _exit(126);
        if (flags >= 0 && (getsockname(wall_control_fd, (struct sockaddr *)&address, &size) < 0 ||
            destination((struct sockaddr *)&address, size, &family, &port) < 0 ||
            port != wall_control_port || tcp_socket(wall_control_fd, family) < 0 ||
            fcntl(wall_control_fd, F_SETFD, flags | FD_CLOEXEC) < 0)) _exit(126);
        if (flags >= 0) control_source = wall_control_fd;
    }
    if (recover_inherited() < 0) _exit(126);
    if (pthread_atfork(fork_prepare, fork_parent, fork_child)) _exit(126);
    atomic_store_explicit(&initialized, 1, memory_order_release);
    acknowledge_recovery();
}

static int wrapped_bind(int fd, const struct sockaddr *address, socklen_t size) {
    if (!adapter_ready()) return bind(fd, address, size);
    if (address && address->sa_family != AF_INET && address->sa_family != AF_INET6) return bind(fd, address, size);
    uint8_t family; uint16_t port;
    if (destination(address, size, &family, &port) < 0 || tcp_socket(fd, family) < 0) return -1;
    lock_table();
    int result = -1;
    struct wall_socket prepared = { .descriptor = -1, .control = -1 };
    if (refresh() < 0) goto finish;
    if (find(fd)) { errno = EINVAL; goto finish; }
    if (wall_control_port && port == wall_control_port) {
        for (unsigned i = 0; i < TRACKED_LIMIT; ++i) {
            if (entries[i].fd >= 0 && !entries[i].socket.lease && entries[i].socket.port == port && entries[i].socket.family == family) {
                errno = EADDRINUSE; goto finish;
            }
        }
        struct sockaddr_storage source;
        socklen_t length = sizeof(source);
        uint8_t source_family; uint16_t source_port;
        if (control_source < 0 || getsockname(control_source, (struct sockaddr *)&source, &length) < 0 ||
            destination((struct sockaddr *)&source, length, &source_family, &source_port) < 0 ||
            source_family != family || source_port != port || tcp_socket(control_source, family) < 0) { errno = EACCES; goto finish; }
        prepared.descriptor = dup(control_source);
        if (prepared.descriptor < 0) goto finish;
        prepared.family = family; prepared.port = port;
    } else {
        int reuse = 0;
        socklen_t length = sizeof(reuse);
        if (getsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &reuse, &length) < 0 ||
            wall_bind(&wall_configuration, family, port, reuse != 0, &prepared) < 0) goto finish;
    }
    result = install(fd, &prepared, 1);
finish: {
    int saved = errno;
    wall_dispose(&prepared);
    unlock_table();
    errno = saved; return result;
}}

static int wrapped_listen(int fd, int backlog) {
    if (!adapter_ready()) return listen(fd, backlog);
    lock_table();
    int result = -1;
    if (refresh() == 0) {
        struct entry *entry = find(fd);
        if (!entry) result = listen(fd, backlog);
        else if (entry->socket.control < 0) result = entry->socket.lease ? failure(EACCES) : 0;
        else result = wall_listen(&entry->socket, backlog < 0 ? 0 : (unsigned)backlog > 4096 ? 4096 : (unsigned)backlog);
    }
    int saved = errno; unlock_table(); errno = saved; return result;
}

static int wrapped_connect(int fd, const struct sockaddr *address, socklen_t size) {
    if (!adapter_ready()) return connect(fd, address, size);
    if (address && address->sa_family != AF_INET && address->sa_family != AF_INET6) return connect(fd, address, size);
    uint8_t family; uint16_t port;
    if (destination(address, size, &family, &port) < 0 || tcp_socket(fd, family) < 0) return -1;
    for (unsigned i = 0; i < 4; ++i) if (wall_direct_ports[i] && port == wall_direct_ports[i]) return connect(fd, address, size);
    lock_table();
    struct wall_socket prepared = { .descriptor = -1, .control = -1 };
    int result = -1;
    struct sockaddr_storage peer;
    socklen_t peer_size = sizeof(peer);
    if (getpeername(fd, (struct sockaddr *)&peer, &peer_size) == 0) errno = EISCONN;
    else if (find(fd)) errno = EINVAL;
    else if (refresh() == 0 && wall_connect(&wall_configuration, family, port, &prepared) == 0) result = install(fd, &prepared, 0);
    int saved = errno; wall_dispose(&prepared); unlock_table(); errno = saved; return result;
}

static int wrapped_close(int fd) {
    if (!adapter_ready()) return close(fd);
    lock_table();
    if (refresh() < 0) { int saved = errno; unlock_table(); return failure(saved); }
    struct entry *entry = find(fd);
    int result = close(fd), saved = errno;
    if (result == 0) {
        forget(entry);
        for (unsigned i = 0; i < TRACKED_LIMIT; ++i)
            if (entries[i].fd >= 0 && entries[i].socket.control == fd)
                entries[i].socket.control = -1;
    }
    unlock_table(); errno = saved; return result;
}

static int duplicate(int source, int target, int command, int argument) {
    if (!adapter_ready()) return command == -1 ? dup(source) : command == -2 ? dup2(source, target) : fcntl(source, command, argument);
    lock_table();
    int result = -1, control = -1;
    struct entry *slot = NULL, *entry = NULL;
    if (refresh() < 0) goto finish;
    /* dup2 can explicitly target a hidden control descriptor. Move that
     * descriptor first so ordinary application replacement remains valid. */
    if (command == -2 && source != target) {
        for (unsigned i = 0; i < TRACKED_LIMIT; ++i) {
            if (entries[i].fd < 0 || entries[i].socket.control != target) continue;
            int flags = fcntl(target, F_GETFD);
            int moved = fcntl(target, F_DUPFD, 3);
            if (moved < 0) goto finish;
            if (flags < 0 || fcntl(moved, F_SETFD, flags) < 0) { int saved = errno; close(moved); errno = saved; goto finish; }
            entries[i].socket.control = moved;
        }
    }
    entry = find(source);
    if (entry && !(command == -2 && source == target)) {
        slot = vacant();
        if (!slot) goto finish;
        if (entry->socket.control >= 0) {
            control = fcntl(entry->socket.control, command >= 0 ? command : F_DUPFD, 3);
            if (control < 0) goto finish;
            if (command == -2 && control == target) {
                int moved = fcntl(control, command == F_DUPFD_CLOEXEC ? F_DUPFD_CLOEXEC : F_DUPFD, 3);
                if (moved < 0) goto finish;
                close(control); control = moved;
            }
        }
    }
    result = command == -1 ? dup(source) : command == -2 ? dup2(source, target) : fcntl(source, command, argument);
    if (result < 0) goto finish;
    if (command == -2 && source == target) goto finish;
    forget(find(result));
    if (slot) {
        slot->fd = result; slot->socket = entry->socket;
        slot->socket.descriptor = result; slot->socket.control = control; control = -1;
    }
finish: {
    int saved = errno; if (control >= 0) close(control);
    unlock_table(); errno = saved; return result;
}}
static int wrapped_dup(int source) { return duplicate(source, -1, -1, 0); }
static int wrapped_dup2(int source, int target) { return duplicate(source, target, -2, 0); }

static int wrapped_fcntl(int fd, int command, ...) {
    va_list arguments;
    va_start(arguments, command);
    int integer = 0;
    void *pointer = NULL;
    enum { NO_ARGUMENT, INTEGER, POINTER, OFFSET } kind = POINTER;
    off_t offset = 0;
    switch (command) {
        case F_GETFD: case F_GETFL: case F_GETOWN: case F_FULLFSYNC:
        case F_BARRIERFSYNC: case F_GETNOSIGPIPE: case F_GETPROTECTIONCLASS:
        case F_GETPROTECTIONLEVEL: case F_GETLEASE: case F_CHKCLEAN:
        case F_FREEZE_FS: case F_THAW_FS: kind = NO_ARGUMENT; break;
        case F_DUPFD: case F_DUPFD_CLOEXEC: case F_SETFD: case F_SETFL:
#ifdef F_DUPFD_CLOFORK
        case F_DUPFD_CLOFORK:
#endif
        case F_SETOWN: case F_RDAHEAD: case F_NOCACHE: case F_GLOBAL_NOCACHE:
        case F_NODIRECT: case F_SETPROTECTIONCLASS: case F_SETBACKINGSTORE:
        case F_SETNOSIGPIPE: case F_SINGLE_WRITER: case F_SETLEASE:
        case F_NOCACHE_EXT: case F_TRANSFEREXTENTS:
            kind = INTEGER; integer = va_arg(arguments, int); break;
        case F_SETSIZE: kind = OFFSET; offset = va_arg(arguments, off_t); break;
        default: pointer = va_arg(arguments, void *); break;
    }
    va_end(arguments);
    if (!adapter_ready()) {
        if (kind == NO_ARGUMENT) return fcntl(fd, command);
        if (kind == INTEGER) return fcntl(fd, command, integer);
        if (kind == OFFSET) return fcntl(fd, command, offset);
        return fcntl(fd, command, pointer);
    }
    if (command == F_DUPFD || command == F_DUPFD_CLOEXEC
#ifdef F_DUPFD_CLOFORK
        || command == F_DUPFD_CLOFORK
#endif
    ) return duplicate(fd, -1, command, integer);
    if (command == F_SETFD) {
        lock_table();
        struct entry *entry = find(fd);
        int previous = fcntl(fd, F_GETFD);
        int result = fcntl(fd, command, integer), saved = errno;
        if (result == 0 && entry && entry->socket.control >= 0 && fcntl(entry->socket.control, F_SETFD, integer) < 0) {
            result = -1; saved = errno;
            if (previous >= 0) (void)fcntl(fd, F_SETFD, previous);
        }
        unlock_table(); errno = saved; return result;
    }
    if (kind == NO_ARGUMENT) return fcntl(fd, command);
    if (kind == INTEGER) return fcntl(fd, command, integer);
    if (kind == OFFSET) return fcntl(fd, command, offset);
    return fcntl(fd, command, pointer);
}

#define INTERPOSE(function, original) \
    __attribute__((used)) static struct { const void *replacement; const void *replacee; } \
    interpose_##original __attribute__((section("__DATA,__interpose"))) = { (const void *)function, (const void *)original }
INTERPOSE(wrapped_bind, bind);
INTERPOSE(wrapped_listen, listen);
INTERPOSE(wrapped_connect, connect);
INTERPOSE(wrapped_close, close);
INTERPOSE(wrapped_dup, dup);
INTERPOSE(wrapped_dup2, dup2);
INTERPOSE(wrapped_fcntl, fcntl);

#include "exec_guards.c"

#include "fork_lifecycle.c"
