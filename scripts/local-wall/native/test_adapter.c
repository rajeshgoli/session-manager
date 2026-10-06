#include <arpa/inet.h>
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <spawn.h>
#include <stdlib.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

/* This application uses only ordinary socket APIs. It links no broker code. */
/* A dedicated negative-control check bypasses interposition to prove that
 * the process-wide sandbox still denies direct kernel operations. */
static int kernel_bind(int fd, const struct sockaddr *address, socklen_t size) {
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    int result = (int)syscall(SYS_bind, fd, address, size);
#pragma clang diagnostic pop
    return result;
}
static void raw_control(int allowed) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in address = { .sin_len = sizeof(address), .sin_family = AF_INET, .sin_addr = { .s_addr = htonl(INADDR_LOOPBACK) } };
    int result = kernel_bind(fd, (struct sockaddr *)&address, sizeof(address));
    if (allowed) assert(result == 0);
    else assert(result == -1 && (errno == EPERM || errno == EACCES));
    close(fd);
}
static socklen_t loopback(struct sockaddr_storage *storage, int family, uint16_t port) {
    memset(storage, 0, sizeof(*storage));
    if (family == AF_INET) {
        struct sockaddr_in *address = (struct sockaddr_in *)storage;
        address->sin_len = sizeof(*address); address->sin_family = AF_INET;
        address->sin_addr.s_addr = htonl(INADDR_LOOPBACK); address->sin_port = htons(port);
        return sizeof(*address);
    }
    struct sockaddr_in6 *address = (struct sockaddr_in6 *)storage;
    address->sin6_len = sizeof(*address); address->sin6_family = AF_INET6;
    address->sin6_addr = in6addr_loopback; address->sin6_port = htons(port);
    return sizeof(*address);
}

static uint16_t bound_port(int fd, int family) {
    struct sockaddr_storage address;
    socklen_t length = sizeof(address);
    assert(getsockname(fd, (struct sockaddr *)&address, &length) == 0);
    assert(address.ss_family == family);
    return ntohs(family == AF_INET ? ((struct sockaddr_in *)&address)->sin_port : ((struct sockaddr_in6 *)&address)->sin6_port);
}

static int listening(int family) {
    int fd = socket(family, SOCK_STREAM, 0);
    assert(fd >= 0);
    assert(fcntl(fd, F_SETFL, O_NONBLOCK) == 0);
    assert(fcntl(fd, F_SETFD, FD_CLOEXEC) == 0);
    struct sockaddr_storage address;
    socklen_t size = loopback(&address, family, 0);
    assert(bind(fd, (struct sockaddr *)&address, size) == 0);
    assert(bound_port(fd, family));
    assert(fcntl(fd, F_GETFL) & O_NONBLOCK);
    assert(fcntl(fd, F_GETFD) & FD_CLOEXEC);
    assert(listen(fd, 8) == 0);
    return fd;
}

static void roundtrip(int fd, int family) {
    int outgoing = socket(family, SOCK_STREAM, 0);
    assert(outgoing >= 0);
    assert(fcntl(outgoing, F_SETFL, O_NONBLOCK) == 0);
    struct sockaddr_storage address;
    socklen_t size = loopback(&address, family, bound_port(fd, family));
    int result = connect(outgoing, (struct sockaddr *)&address, size);
    assert(result == 0 || (result == -1 && errno == EINPROGRESS));
    struct pollfd event = { .fd = outgoing, .events = POLLOUT };
    assert(poll(&event, 1, 2000) == 1);
    int error = -1;
    socklen_t length = sizeof(error);
    assert(getsockopt(outgoing, SOL_SOCKET, SO_ERROR, &error, &length) == 0 && error == 0);
    assert(fcntl(outgoing, F_GETFL) & O_NONBLOCK);
    assert(send(outgoing, "application", 11, MSG_NOSIGNAL) == 11);
    event = (struct pollfd){ .fd = fd, .events = POLLIN };
    assert(poll(&event, 1, 2000) == 1);
    int incoming = accept(fd, NULL, NULL);
    assert(incoming >= 0);
    int accepted_flags = fcntl(incoming, F_GETFL);
    assert(accepted_flags >= 0);
    assert(fcntl(incoming, F_SETFL, accepted_flags & ~O_NONBLOCK) == 0);
    char bytes[11];
    assert(recv(incoming, bytes, sizeof(bytes), MSG_WAITALL) == 11);
    assert(!memcmp(bytes, "application", sizeof(bytes)));
    close(incoming); close(outgoing);
}

static void *thread_client(void *unused) {
    (void)unused;
    int fd = listening(AF_INET);
    roundtrip(fd, AF_INET);
    close(fd);
    return NULL;
}

static void exec_guards(void) {
#if defined(F_DUPFD_CLOFORK) && defined(FD_CLOFORK)
    int source = listening(AF_INET);
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    int baseline = (int)syscall(SYS_fcntl, source, F_DUPFD_CLOFORK, 3);
#pragma clang diagnostic pop
    int baseline_errno = errno;
    if (baseline >= 0) close(baseline);
    int fork_closed = fcntl(source, F_DUPFD_CLOFORK, 3);
    if (fork_closed >= 0) {
        assert(fcntl(fork_closed, F_GETFD) & FD_CLOFORK);
        pid_t child = fork();
        assert(child >= 0);
        if (!child) {
            assert(fcntl(fork_closed, F_GETFD) == -1 && errno == EBADF);
            int replacement = listening(AF_INET);
            roundtrip(replacement, AF_INET);
            close(replacement); close(source);
            _exit(0);
        }
        int status;
        assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
        close(fork_closed);
    } else {
        int adapted_errno = errno;
        fprintf(stderr, "close-on-fork baseline=%d errno=%d; adapter errno=%d\n", baseline, baseline_errno, adapted_errno);
        assert(baseline < 0 && adapted_errno == baseline_errno);
    }
    close(source);
#endif
    int fd = listening(AF_INET);
    char *arguments[] = { "true", NULL };
    char *environment[] = { NULL };
    pid_t child;
    assert(posix_spawn(&child, "/usr/bin/true", NULL, NULL, arguments, environment) == 0);
    int status;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
    assert(fcntl(fd, F_SETFD, 0) == 0);
    assert(execve("/usr/bin/true", arguments, environment) == -1 && errno == EACCES);
    assert(execv("/usr/bin/true", arguments) == -1 && errno == EACCES);
    assert(execvp("true", arguments) == -1 && errno == EACCES);
    assert(execl("/usr/bin/true", "true", (char *)NULL) == -1 && errno == EACCES);
    assert(execlp("true", "true", (char *)NULL) == -1 && errno == EACCES);
    assert(execle("/usr/bin/true", "true", (char *)NULL, environment) == -1 && errno == EACCES);
    assert(posix_spawn(&child, "/usr/bin/true", NULL, NULL, arguments, environment) == EACCES);
    assert(posix_spawnp(&child, "true", NULL, NULL, arguments, environment) == EACCES);
    roundtrip(fd, AF_INET);
    close(fd);
}

static void provider_control(uint16_t port) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_storage address;
    socklen_t size = loopback(&address, AF_INET, port);
    assert(bind(fd, (struct sockaddr *)&address, size) == 0);
    assert(bound_port(fd, AF_INET) == port);
    assert(listen(fd, 8) == 0);
    int client = socket(AF_INET, SOCK_STREAM, 0);
    assert(connect(client, (struct sockaddr *)&address, size) == -1 && errno == EACCES);
    assert(bind(client, (struct sockaddr *)&address, size) == -1 && errno == EADDRINUSE);
    close(client); close(fd);
}

int main(int argc, char **argv) {
    if (argc == 2 && !strcmp(argv[1], "raw-control")) { raw_control(1); return 0; }
    raw_control(0);
    if (argc == 2) {
        unsigned long port = strtoul(argv[1], NULL, 10);
        assert(port && port <= UINT16_MAX);
        provider_control((uint16_t)port);
    }
    int forbidden = socket(AF_INET, SOCK_STREAM, 0);
    struct sockaddr_in wildcard = { .sin_len = sizeof(wildcard), .sin_family = AF_INET };
    assert(bind(forbidden, (struct sockaddr *)&wildcard, sizeof(wildcard)) == -1 && errno == EACCES);
    close(forbidden);
    for (int index = 0; index < 2; ++index) {
        int family = index ? AF_INET6 : AF_INET;
        int original = listening(family);
        int duplicate = dup(original);
        assert(duplicate >= 0 && !(fcntl(duplicate, F_GETFD) & FD_CLOEXEC));
        close(original);
        roundtrip(duplicate, family);
        int copy = fcntl(duplicate, F_DUPFD_CLOEXEC, 3);
        assert(copy >= 0 && (fcntl(copy, F_GETFD) & FD_CLOEXEC));
        close(duplicate);
        int replacement = socket(family, SOCK_STREAM, 0);
        assert(dup2(copy, replacement) == replacement);
        close(copy);
        assert(listen(replacement, 8) == 0);
        roundtrip(replacement, family);
        pid_t child = fork();
        assert(child >= 0);
        if (!child) {
            assert(listen(replacement, 8) == 0);
            roundtrip(replacement, family);
            close(replacement);
            _exit(0);
        }
        int status;
        assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0);
        roundtrip(replacement, family);
        close(replacement);
    }
    pthread_t workers[4];
    for (int i = 0; i < 4; ++i) assert(pthread_create(&workers[i], NULL, thread_client, NULL) == 0);
    for (int i = 0; i < 4; ++i) assert(pthread_join(workers[i], NULL) == 0);
    puts("ordinary socket IPv4/IPv6, flags, dup, fork and concurrent listeners passed");
    exec_guards();
    return 0;
}
