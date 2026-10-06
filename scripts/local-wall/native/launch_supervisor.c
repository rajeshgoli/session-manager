/* Trusted host process: remain alive as the registered ancestry root while
 * the sandboxed application executes. No agent supplies this command line. */
#include <errno.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

enum { CONTROL_FD = 198, GATE_FD = 199 };

int main(int argc, char **argv) {
    if (argc < 3 || (strcmp(argv[1], "--control") && strcmp(argv[1], "--no-control"))) return 126;
    int control = !strcmp(argv[1], "--control");
    signal(SIGPIPE, SIG_IGN);
    /* The caller starts with a cleared environment. Never carry an unrelated
     * host descriptor through either the supervisor or sandbox-exec. */
    int maximum = getdtablesize();
    for (int fd = 3; fd < maximum; ++fd)
        if ((fd != CONTROL_FD || !control) && fd != GATE_FD) close(fd);
    char byte = 0;
    ssize_t count;
    do { count = read(GATE_FD, &byte, 1); } while (count < 0 && errno == EINTR);
    if (count != 1 || byte != 'R') return 126;
    pid_t child = fork();
    if (child < 0) return 126;
    if (child == 0) {
        signal(SIGPIPE, SIG_DFL);
        close(GATE_FD);
        execv("/usr/bin/sandbox-exec", argv + 2);
        _exit(126);
    }
    close(CONTROL_FD);
    int status;
    do { count = waitpid(child, &status, 0); } while (count < 0 && errno == EINTR);
    if (count < 0) return 126;
    const char *bytes = (const char *)&status;
    size_t sent = 0;
    while (sent < sizeof(status)) {
        count = write(GATE_FD, bytes + sent, sizeof(status) - sent);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) break;
        sent += (size_t)count;
    }
    /* Keep our PID reserved until the host kills the group and reaps us.
     * A disconnected host also terminates the entire private process group. */
    do { count = read(GATE_FD, &byte, 1); } while (count < 0 && errno == EINTR);
    kill(0, SIGKILL);
    return 126;
}
