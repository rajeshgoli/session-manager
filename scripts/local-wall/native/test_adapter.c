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

static void inherited_exec(const char *application, int method) {
    int family = method % 2 ? AF_INET6 : AF_INET;
    int fd;
    if (method == 6) {
        fd = socket(family, SOCK_STREAM, 0);
        struct sockaddr_storage address;
        socklen_t size = loopback(&address, family, 0);
        assert(bind(fd, (struct sockaddr *)&address, size) == 0);
    } else fd = listening(family);
    assert(fcntl(fd, F_SETFD, 0) == 0);
    int copy = dup(fd);
    assert(copy >= 0);
    char *failed_arguments[] = { "missing", NULL };
    char *empty_environment[] = { NULL };
    assert(execve("/nonexistent-sm-adapter-test", failed_arguments, empty_environment) == -1 && errno == ENOENT);
    pid_t child = fork();
    assert(child >= 0);
    if (!child) {
        char first[32], second[32], family_text[32];
        snprintf(first, sizeof(first), "%d", fd);
        snprintf(second, sizeof(second), "%d", copy);
        snprintf(family_text, sizeof(family_text), "%d", family);
        char *arguments[] = { (char *)application, "inherited-exec", first, second, family_text, NULL };
        char *environment[] = { "DYLD_INSERT_LIBRARIES=/tmp/forged.dylib", "SM_BROKER_ENDPOINT=/tmp/forged", NULL };
        assert(setenv("DYLD_INSERT_LIBRARIES", "/tmp/forged.dylib", 1) == 0);
        const char *leaf = strrchr(application, '/');
        assert(leaf);
        char directory[1024];
        assert((size_t)(leaf - application) < sizeof(directory));
        memcpy(directory, application, (size_t)(leaf - application));
        directory[leaf - application] = 0;
        assert(setenv("PATH", directory, 1) == 0);
        switch (method) {
            case 0: (void)execve(application, arguments, environment); break;
            case 1: (void)execv(application, arguments); break;
            case 2: (void)execvp(leaf + 1, arguments); break;
            case 3: (void)execvP(leaf + 1, directory, arguments); break;
            case 4: (void)execl(application, application, "inherited-exec", first, second, family_text, (char *)NULL); break;
            case 5: (void)execlp(leaf + 1, application, "inherited-exec", first, second, family_text, (char *)NULL); break;
            case 6: (void)execle(application, application, "inherited-exec", first, second, family_text, (char *)NULL, empty_environment); break;
        }
        _exit(127);
    }
    int status;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
    roundtrip(fd, family);
    close(copy); close(fd);
}

static void inherited_spawn(const char *application) {
    for (int defaults = 0; defaults < 4; ++defaults) {
        int fd = listening(AF_INET);
        posix_spawn_file_actions_t actions;
        assert(posix_spawn_file_actions_init(&actions) == 0);
        if (defaults == 2) assert(posix_spawn_file_actions_addinherit_np(&actions, fd) == 0);
        assert(posix_spawn_file_actions_adddup2(&actions, fd, 101) == 0);
        assert(posix_spawn_file_actions_adddup2(&actions, 101, 102) == 0);
        if (defaults != 2) assert(posix_spawn_file_actions_addclose(&actions, fd) == 0);
        assert(posix_spawn_file_actions_addopen(&actions, STDOUT_FILENO, "/dev/null", O_WRONLY, 0) == 0);
        posix_spawnattr_t attributes;
        assert(posix_spawnattr_init(&attributes) == 0);
        if (defaults == 1 || defaults == 2) assert(posix_spawnattr_setflags(&attributes, POSIX_SPAWN_CLOEXEC_DEFAULT) == 0);
        char *arguments[] = { (char *)application, "inherited-exec", "101", "102", "2", NULL };
        char *environment[] = { "SM_WALL_RECOVERY_FD=0", NULL };
        pid_t child;
        assert(posix_spawn(&child, "/nonexistent-sm-adapter-test", &actions, &attributes, arguments, environment) == ENOENT);
        assert(fcntl(fd, F_GETFD) & FD_CLOEXEC);
        roundtrip(fd, AF_INET);
        int result = defaults == 3 ? posix_spawnp(&child, application, &actions, &attributes, arguments, environment) :
            posix_spawn(&child, application, &actions, &attributes, arguments, environment);
        assert(result == 0);
        close(fd);
        int status;
        assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
        assert(posix_spawn_file_actions_destroy(&actions) == 0);
        assert(posix_spawnattr_destroy(&attributes) == 0);
    }
}

static void immediate_rebind(void) {
    for (int index = 0; index < 32; ++index) {
        int fd = listening(AF_INET);
        uint16_t port = bound_port(fd, AF_INET);
        int copy = dup(fd);
        close(fd);
        roundtrip(copy, AF_INET);
        close(copy);
        int replacement = socket(AF_INET, SOCK_STREAM, 0);
        int reuse = 1;
        assert(setsockopt(replacement, SOL_SOCKET, SO_REUSEADDR, &reuse, sizeof(reuse)) == 0);
        struct sockaddr_storage address;
        socklen_t size = loopback(&address, AF_INET, port);
        assert(bind(replacement, (struct sockaddr *)&address, size) == 0);
        assert(listen(replacement, 8) == 0);
        close(replacement);
    }
}

static void inherited_tools(void) {
    const char *programs[] = { getenv("SM_TEST_RUST_APPLICATION"), getenv("SM_TEST_PYTHON") };
    for (int tool = 0; tool < 2; ++tool) {
        assert(programs[tool]);
        int fd = listening(AF_INET);
        assert(fcntl(fd, F_SETFD, 0) == 0);
        char number[32];
        snprintf(number, sizeof(number), "%d", fd);
        char *rust_arguments[] = { (char *)programs[tool], number, NULL };
        char *python_arguments[] = { (char *)programs[tool], "-c",
            "import socket,sys; s=socket.socket(fileno=int(sys.argv[1])); d=s.dup(); s.close(); d.setblocking(True); c=socket.create_connection(d.getsockname()); c.sendall(b'python'); a,_=d.accept(); assert a.recv(6)==b'python'; a.close(); c.close(); d.close()",
            number, NULL };
        char *environment[] = { NULL };
        pid_t child;
        int result = posix_spawn(&child, programs[tool], NULL, NULL, tool ? python_arguments : rust_arguments, environment);
        if (result) fprintf(stderr, "tool %s spawn error %d\n", programs[tool], result);
        assert(result == 0);
        close(fd);
        int status;
        assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
    }
}

static int parent_callback_fd = -1;
static void parent_closes_listener(void) {
    if (parent_callback_fd >= 0) { close(parent_callback_fd); parent_callback_fd = -1; }
}
static void fork_parent_closes(void) {
    assert(pthread_atfork(NULL, parent_closes_listener, NULL) == 0);
    int fd = listening(AF_INET);
    parent_callback_fd = fd;
    pid_t child = fork();
    assert(child >= 0);
    if (!child) { roundtrip(fd, AF_INET); close(fd); _exit(0); }
    assert(parent_callback_fd == -1);
    int status;
    assert(waitpid(child, &status, 0) == child && WIFEXITED(status) && !WEXITSTATUS(status));
}

int main(int argc, char **argv) {
    if (argc == 5 && !strcmp(argv[1], "inherited-exec")) {
        int first = atoi(argv[2]), second = atoi(argv[3]);
        int family = atoi(argv[4]);
        assert(listen(first, 8) == 0);
        close(first);
        assert(listen(second, 8) == 0);
        roundtrip(second, family);
        close(second);
        return 0;
    }
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
    for (int method = 0; method < 7; ++method) inherited_exec(argv[0], method);
    inherited_spawn(argv[0]);
    immediate_rebind();
    inherited_tools();
    fork_parent_closes();
    exec_guards();
    return 0;
}
