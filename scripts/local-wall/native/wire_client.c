#include "wire_client.h"
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <poll.h>
#include <stddef.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <time.h>
#include <unistd.h>

#define REQUEST_SIZE 40
#define REPLY_SIZE 32
#define MAX_BACKLOG 4096
#define TIMEOUT_MS 2000
/* XNU bsd/kern/uipc_usrreq.c bounds one SCM_RIGHTS message at
 * UIPC_MAX_CMSG_FD=512. Receive the full message before rejecting extras:
 * Darwin externalizes all descriptors before copying ancillary bytes out.
 * https://github.com/apple-oss-distributions/xnu/blob/main/bsd/kern/uipc_usrreq.c */
#define RECEIVED_LIMIT 512

static void put16(unsigned char *out, uint16_t value) {
    out[0] = (unsigned char)(value >> 8); out[1] = (unsigned char)value;
}
static uint16_t get16(const unsigned char *in) {
    return (uint16_t)((uint16_t)in[0] << 8 | in[1]);
}
static void put64(unsigned char *out, uint64_t value) {
    for (int i = 7; i >= 0; --i) { out[i] = (unsigned char)value; value >>= 8; }
}
static uint64_t get64(const unsigned char *in) {
    uint64_t value = 0;
    for (int i = 0; i < 8; ++i) value = (value << 8) | in[i];
    return value;
}
static int fail(int code) { errno = code; return -1; }

static int64_t milliseconds(void) {
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return -1;
    return (int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000;
}

static int ready(int fd, short events, int64_t deadline) {
    for (;;) {
        int64_t now = milliseconds();
        if (now < 0) return -1;
        if (now >= deadline) return fail(ETIMEDOUT);
        struct pollfd item = { .fd = fd, .events = events };
        int result = poll(&item, 1, (int)(deadline - now));
        if (result < 0 && errno == EINTR) continue;
        if (result < 0) return -1;
        if (!result) return fail(ETIMEDOUT);
        if (item.revents & POLLNVAL) return fail(EBADF);
        /* HUP may accompany the last reply bytes. recvmsg checks EOF. */
        return 0;
    }
}

static int close_on_exec(int fd) {
    int flags = fcntl(fd, F_GETFD);
    if (flags < 0) return -1;
    return fcntl(fd, F_SETFD, flags | FD_CLOEXEC);
}

static int broker(const struct wall_configuration *configuration) {
    if (!configuration || !configuration->endpoint) return fail(EINVAL);
    size_t length = strnlen(configuration->endpoint, sizeof(((struct sockaddr_un *)0)->sun_path));
    if (!length || length >= sizeof(((struct sockaddr_un *)0)->sun_path)) return fail(ENAMETOOLONG);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) return -1;
    int saved = 0;
    if (close_on_exec(fd) < 0 || fcntl(fd, F_SETFL, O_NONBLOCK) < 0) goto error;
    struct sockaddr_un address = {0};
    address.sun_len = (uint8_t)(offsetof(struct sockaddr_un, sun_path) + length + 1);
    address.sun_family = AF_UNIX;
    memcpy(address.sun_path, configuration->endpoint, length + 1);
    int64_t start = milliseconds();
    if (start < 0) goto error;
    if (connect(fd, (const struct sockaddr *)&address, address.sun_len) < 0) {
        if (errno != EINPROGRESS && errno != EAGAIN) goto error;
        if (ready(fd, POLLOUT, start + TIMEOUT_MS) < 0) goto error;
        int result = 0;
        socklen_t size = sizeof(result);
        if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &result, &size) < 0) goto error;
        if (size != sizeof(result)) { errno = EPROTO; goto error; }
        if (result) { errno = result; goto error; }
    }
    /* LOCAL_PEERTOKEN is the full opaque macOS audit_token_t. Comparing its
     * complete bytes requires no cross-sandbox process-information query. */
    uint32_t token[8] = {0};
    socklen_t size = sizeof(token);
    if (getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, token, &size) < 0) goto error;
    if (size != sizeof(token) || memcmp(token, configuration->peer_token, sizeof(token))) {
        errno = EACCES; goto error;
    }
    return fd;
error:
    saved = errno;
    close(fd);
    return fail(saved);
}

static void frame(unsigned char *out, unsigned operation, uint8_t family,
                  uint16_t port, uint64_t lease, unsigned backlog, int reuse) {
    memset(out, 0, REQUEST_SIZE);
    memcpy(out, "SMSK", 4);
    out[4] = 1; out[5] = (unsigned char)operation;
    out[6] = family; out[7] = (unsigned char)reuse;
    put16(out + 8, port); put16(out + 10, (uint16_t)backlog);
    put64(out + 16, lease);
    if (family == 1) { out[24] = 127; out[27] = 1; }
    if (family == 2) out[39] = 1;
}

static int socket_matches(int fd, uint8_t family, uint16_t port, int connected) {
    int type = 0;
    socklen_t size = sizeof(type);
    if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &size) < 0) return -1;
    if (size != sizeof(type) || type != SOCK_STREAM) return fail(EPROTO);
    struct sockaddr_storage address = {0};
    size = sizeof(address);
    int result = connected ? getpeername(fd, (struct sockaddr *)&address, &size)
                           : getsockname(fd, (struct sockaddr *)&address, &size);
    if (result < 0) return -1;
    if (family == 1) {
        const struct sockaddr_in *v4 = (const struct sockaddr_in *)&address;
        if (size != sizeof(*v4) || v4->sin_family != AF_INET || v4->sin_addr.s_addr != htonl(INADDR_LOOPBACK) || ntohs(v4->sin_port) != port) return fail(EPROTO);
    } else if (family == 2) {
        const struct sockaddr_in6 *v6 = (const struct sockaddr_in6 *)&address;
        if (size != sizeof(*v6) || v6->sin6_family != AF_INET6 || memcmp(&v6->sin6_addr, &in6addr_loopback, sizeof(in6addr_loopback)) || ntohs(v6->sin6_port) != port || v6->sin6_scope_id) return fail(EPROTO);
        if (!connected) {
            int only = 0;
            size = sizeof(only);
            if (getsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &only, &size) < 0) return -1;
            if (size != sizeof(only) || only != 1) return fail(EPROTO);
        }
    } else return fail(EPROTO);
    return 0;
}

static int transact(int control, const unsigned char *request, struct wall_socket *socket_out) {
    unsigned char reply[REPLY_SIZE] = {0};
    int received[RECEIVED_LIMIT];
    size_t descriptor_count = 0, offset = 0;
    int saved = 0;
    int64_t start = milliseconds();
    if (start < 0) return -1;
    int64_t deadline = start + TIMEOUT_MS;
    while (offset < REQUEST_SIZE) {
        if (ready(control, POLLOUT, deadline) < 0) goto error;
        ssize_t count = send(control, request + offset, REQUEST_SIZE - offset, MSG_NOSIGNAL);
        if (count < 0 && (errno == EINTR || errno == EAGAIN)) continue;
        if (count < 0) goto error;
        if (!count) { errno = EPIPE; goto error; }
        offset += (size_t)count;
    }
    offset = 0;
    while (offset < REPLY_SIZE) {
        if (ready(control, POLLIN, deadline) < 0) goto error;
        struct iovec iov = { .iov_base = reply + offset, .iov_len = REPLY_SIZE - offset };
        union { struct cmsghdr alignment; unsigned char bytes[CMSG_SPACE(sizeof(received))]; } ancillary;
        memset(&ancillary, 0, sizeof(ancillary));
        struct msghdr message = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = ancillary.bytes, .msg_controllen = sizeof(ancillary.bytes) };
        ssize_t count = recvmsg(control, &message, 0);
        if (count < 0 && (errno == EINTR || errno == EAGAIN)) continue;
        if (count < 0) goto error;
        /* Own and close every delivered descriptor even on malformed replies. */
        int malformed = (message.msg_flags & (MSG_CTRUNC | MSG_TRUNC)) != 0;
        for (struct cmsghdr *item = CMSG_FIRSTHDR(&message); item; item = CMSG_NXTHDR(&message, item)) {
            if (item->cmsg_level != SOL_SOCKET || item->cmsg_type != SCM_RIGHTS || item->cmsg_len < CMSG_LEN(0)) { malformed = 1; continue; }
            size_t bytes = item->cmsg_len - CMSG_LEN(0);
            size_t available = message.msg_controllen - (size_t)((unsigned char *)item - ancillary.bytes);
            if (available < CMSG_LEN(0)) { malformed = 1; break; }
            if (bytes > available - CMSG_LEN(0)) { bytes = available - CMSG_LEN(0); malformed = 1; }
            if (bytes % sizeof(int)) malformed = 1;
            const int *fds = (const int *)CMSG_DATA(item);
            for (size_t i = 0; i < bytes / sizeof(int); ++i) {
                if (descriptor_count == RECEIVED_LIMIT) { close(fds[i]); malformed = 1; continue; }
                received[descriptor_count++] = fds[i];
                if (close_on_exec(fds[i]) < 0) malformed = 1;
            }
        }
        if (malformed || descriptor_count > 1) { errno = EPROTO; goto error; }
        if (!count) { errno = ECONNRESET; goto error; }
        offset += (size_t)count;
    }
    if (memcmp(reply, "SMSK", 4) || reply[4] != 1 || reply[5] != request[5] || reply[14] || reply[15]) { errno = EPROTO; goto error; }
    for (int i = 24; i < REPLY_SIZE; ++i) if (reply[i]) { errno = EPROTO; goto error; }
    uint32_t code = (uint32_t)reply[8] << 24 | (uint32_t)reply[9] << 16 | (uint32_t)reply[10] << 8 | reply[11];
    uint16_t port = get16(reply + 12);
    uint64_t lease = get64(reply + 16);
    if (code) {
        if (code > INT_MAX || reply[6] || reply[7] || descriptor_count || port || lease) { errno = EPROTO; goto error; }
        errno = (int)code; goto error;
    }
    int allocation = request[5] == 1 || request[5] == 3 || request[5] == 5;
    if (allocation) {
        if (reply[6] != request[6] || reply[7] != 1 || descriptor_count != 1 || !port || (get16(request + 8) && port != get16(request + 8)) || (request[5] != 3 ? !lease : lease != 0)) { errno = EPROTO; goto error; }
        if (socket_matches(received[0], reply[6], port, request[5] == 3) < 0) goto error;
        socket_out->descriptor = received[0];
        socket_out->family = reply[6]; socket_out->port = port; socket_out->lease = lease;
        return 0;
    }
    if (reply[6] || reply[7] || descriptor_count || port || !lease || lease != get64(request + 16)) { errno = EPROTO; goto error; }
    return 0;
error:
    saved = errno;
    for (size_t i = 0; i < descriptor_count; ++i) close(received[i]);
    return fail(saved);
}

static int allocate(const struct wall_configuration *configuration, unsigned operation,
                    uint8_t family, uint16_t port, int reuse, struct wall_socket *out) {
    if (!out) return fail(EINVAL);
    *out = (struct wall_socket){ .descriptor = -1, .control = -1 };
    if ((family != 1 && family != 2) || (reuse != 0 && reuse != 1) || (operation != 1 && !port)) return fail(EINVAL);
    int control = broker(configuration);
    if (control < 0) return -1;
    unsigned char request[REQUEST_SIZE];
    frame(request, operation, family, port, 0, 0, reuse);
    if (transact(control, request, out) < 0) { int saved = errno; close(control); return fail(saved); }
    if (operation != 3) out->control = control;
    else close(control);
    return 0;
}

int wall_bind(const struct wall_configuration *configuration, uint8_t family,
              uint16_t port, int reuse, struct wall_socket *out) {
    return allocate(configuration, 1, family, port, reuse, out);
}
int wall_connect(const struct wall_configuration *configuration, uint8_t family,
                 uint16_t port, struct wall_socket *out) {
    return allocate(configuration, 3, family, port, 0, out);
}
int wall_retain(const struct wall_configuration *configuration, uint8_t family,
                uint16_t port, struct wall_socket *out) {
    return allocate(configuration, 5, family, port, 0, out);
}
int wall_listen(struct wall_socket *socket, unsigned backlog) {
    if (!socket || socket->control < 0 || !socket->lease || backlog > MAX_BACKLOG) return fail(EINVAL);
    unsigned char request[REQUEST_SIZE];
    frame(request, 2, 0, 0, socket->lease, backlog, 0);
    return transact(socket->control, request, socket);
}
int wall_release(struct wall_socket *socket) {
    if (!socket || socket->control < 0 || !socket->lease) return fail(EINVAL);
    unsigned char request[REQUEST_SIZE];
    frame(request, 4, 0, 0, socket->lease, 0, 0);
    int result = transact(socket->control, request, socket);
    int saved = errno;
    wall_dispose(socket);
    errno = saved;
    return result;
}
void wall_dispose(struct wall_socket *socket) {
    if (!socket) return;
    int saved = errno;
    if (socket->descriptor >= 0) close(socket->descriptor);
    if (socket->control >= 0) close(socket->control);
    *socket = (struct wall_socket){ .descriptor = -1, .control = -1 };
    errno = saved;
}
