/* Trusted host process: remain alive as the registered ancestry root while
 * the sandboxed application executes. No agent supplies this command line. */
#include <errno.h>
#include <libproc.h>
#include <poll.h>
#include <signal.h>
#include <stdlib.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

enum { CONTROL_FD = 198, GATE_FD = 199 };

static int same_member(const struct proc_bsdinfo *identity, pid_t session) {
    struct proc_bsdinfo current;
    return proc_pidinfo((int)identity->pbi_pid, PROC_PIDTBSDINFO, 1,
            &current, sizeof(current)) == sizeof(current) &&
        current.pbi_start_tvsec == identity->pbi_start_tvsec &&
        current.pbi_start_tvusec == identity->pbi_start_tvusec &&
        current.pbi_status != 5 && getsid((pid_t)current.pbi_pid) == session;
}
static void cleanup_session(void) {
    pid_t session = getpid();
    // This trusted process never forks again here. Keep its PID reserved until
    // no live member remains, including orphaned workers in separate groups.
    for (;;) {
        int capacity = proc_listallpids(NULL, 0) + 64;
        pid_t *pids = capacity > 64 ? calloc((size_t)capacity, sizeof(*pids)) : NULL;
        int count = pids ? proc_listallpids(pids, capacity * (int)sizeof(*pids)) : 0;
        int alive = count <= 0 || count >= capacity;
        for (int i = 0; i < count && i < capacity; ++i) {
            pid_t member = pids[i];
            if (member <= 0 || member == session || getsid(member) != session) continue;
            struct proc_bsdinfo identity;
            // Nonzero arg includes unreaped exited processes, so they cannot
            // keep a disconnected supervisor retrying cleanup indefinitely.
            if (proc_pidinfo(member, PROC_PIDTBSDINFO, 1, &identity, sizeof(identity)) != sizeof(identity)) {
                if (getsid(member) == session) alive = 1;
                continue;
            }
            if (identity.pbi_status == 5) continue;
            alive = 1;
            if (same_member(&identity, session)) (void)kill(member, SIGKILL);
        }
        free(pids);
        if (!alive) break;
        (void)poll(NULL, 0, 2);
    }
    kill(session, SIGKILL);
    _exit(126);
}

int main(int argc, char **argv) {
    if (argc < 3 || (strcmp(argv[1], "--control") && strcmp(argv[1], "--no-control"))) return 126;
    if (getsid(0) != getpid()) return 126;
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
    // Only the application produces stdout/stderr. Keeping these copies open
    // would hide its exit from readers while we wait for host cleanup.
    close(STDOUT_FILENO);
    close(STDERR_FILENO);
    int status;
    for (;;) {
        count = waitpid(child, &status, WNOHANG);
        if (count == child) break;
        if (count < 0 && errno != EINTR) cleanup_session();
        struct pollfd host = { .fd = GATE_FD, .events = POLLIN };
        int ready = poll(&host, 1, 100);
        if ((ready < 0 && errno != EINTR) ||
            (ready > 0 && (host.revents & (POLLIN | POLLHUP | POLLERR | POLLNVAL))))
            cleanup_session();
    }
    const char *bytes = (const char *)&status;
    size_t sent = 0;
    while (sent < sizeof(status)) {
        count = write(GATE_FD, bytes + sent, sizeof(status) - sent);
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) break;
        sent += (size_t)count;
    }
    /* Keep our PID reserved until the host kills the session and reaps us.
     * A disconnected host also removes all command groups and orphaned workers. */
    do { count = read(GATE_FD, &byte, 1); } while (count < 0 && errno == EINTR);
    cleanup_session();
    return 126;
}
