/* Keep old leases alive through user atfork callbacks, then let the parent
 * return only after the child has opened independent control connections. */
static int fork_transaction;
static int fork_completion[2];
static struct wall_socket fork_holds[TRACKED_LIMIT];
static unsigned fork_hold_count;
static int fork_hold_error;
static void hold_fork_leases(void) {
    if (!fork_transaction) return;
    fork_hold_count = 0;
    fork_hold_error = 0;
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) {
        if (entries[i].fd < 0 || entries[i].socket.control < 0) continue;
        int held = 0;
        for (unsigned j = 0; j < fork_hold_count; ++j)
            if (fork_holds[j].lease == entries[i].socket.lease) held = 1;
        if (held) continue;
        int duplicate = fcntl(entries[i].socket.control, F_DUPFD_CLOEXEC, 3);
        if (duplicate < 0) { fork_hold_error = errno; break; }
        fork_holds[fork_hold_count] = entries[i].socket;
        fork_holds[fork_hold_count].descriptor = -1;
        fork_holds[fork_hold_count++].control = duplicate;
    }
    fork_pending = 1;
}
static void finish_fork_holds(int child) {
    for (unsigned i = 0; i < fork_hold_count; ++i) {
        int final = !child;
        for (unsigned j = 0; j < TRACKED_LIMIT; ++j)
            if (entries[j].fd >= 0 && entries[j].socket.lease == fork_holds[i].lease) final = 0;
        if (final) (void)wall_release(&fork_holds[i]);
        else close(fork_holds[i].control);
    }
    fork_hold_count = 0;
}
static pid_t wrapped_fork(void) {
    if (!adapter_ready()) return fork();
    // Serialize concurrent fork transactions without holding the descriptor
    // lock across libc's own atfork callbacks.
    static pthread_mutex_t fork_lock = PTHREAD_MUTEX_INITIALIZER;
    int previous_cancellation;
    pthread_setcancelstate(PTHREAD_CANCEL_DISABLE, &previous_cancellation);
    pthread_mutex_lock(&fork_lock);
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, fork_completion) < 0) {
        int saved = errno;
        pthread_mutex_unlock(&fork_lock);
        pthread_setcancelstate(previous_cancellation, NULL);
        return failure(saved);
    }
    (void)fcntl(fork_completion[0], F_SETFD, FD_CLOEXEC);
    (void)fcntl(fork_completion[1], F_SETFD, FD_CLOEXEC);
    fork_transaction = 1;
    pid_t child = fork();
    int saved = errno;
    if (!child) {
        close(fork_completion[0]);
        lock_table();
        int result = fork_hold_error ? failure(fork_hold_error) : refresh();
        saved = errno;
        finish_fork_holds(1);
        fork_pending = 0; fork_transaction = 0;
        unlock_table();
        (void)send(fork_completion[1], result == 0 ? "R" : "F", 1, MSG_NOSIGNAL);
        close(fork_completion[1]);
        pthread_mutex_unlock(&fork_lock);
        if (result < 0) _exit(126);
        pthread_setcancelstate(previous_cancellation, NULL);
        errno = saved; return 0;
    }
    close(fork_completion[1]);
    if (child > 0) {
        struct pollfd event = { .fd = fork_completion[0], .events = POLLIN };
        int ready;
        do { ready = poll(&event, 1, 2000); } while (ready < 0 && errno == EINTR);
        char reply;
        if (ready <= 0 || recv(fork_completion[0], &reply, 1, MSG_DONTWAIT) != 1 || reply != 'R') {
            (void)kill(child, SIGKILL);
            while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {}
            child = -1; saved = EACCES;
        }
    }
    close(fork_completion[0]);
    lock_table();
    fork_pending = 0; fork_transaction = 0;
    finish_fork_holds(0);
    unlock_table();
    pthread_mutex_unlock(&fork_lock);
    pthread_setcancelstate(previous_cancellation, NULL);
    errno = saved; return child;
}
INTERPOSE(wrapped_fork, fork);
