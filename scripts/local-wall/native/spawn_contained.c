/* Included by spawn_lifecycle.c. The production wall denies the spawn syscall
 * because its session flags bypass a denial of setsid/setpgid. Spawn through a
 * kernel fork and async-signal-safe child operations instead. No atfork user
 * callbacks run, as with native posix_spawn. The new image initializes libc. */
#include <sys/syscall.h>
extern const int wall_contained_spawns;

static int apply_spawn_actions(const struct action_list *list, short flags,
        const sigset_t *defaults, const sigset_t *mask,
        const int *holds, int count, int notification) {
    if (flags & POSIX_SPAWN_CLOEXEC_DEFAULT) {
        int limit = getdtablesize();
        for (int fd = 0; fd < limit; ++fd) {
            int old = fcntl(fd, F_GETFD);
            if (old >= 0 && fcntl(fd, F_SETFD, old | FD_CLOEXEC) < 0) return errno;
        }
    }
    if (flags & POSIX_SPAWN_SETSIGMASK)
        if (sigprocmask(SIG_SETMASK, mask, NULL) < 0) return errno;
    if (flags & POSIX_SPAWN_SETSIGDEF) {
        struct sigaction action = { .sa_handler = SIG_DFL };
        sigemptyset(&action.sa_mask);
        for (int signal = 1; signal < NSIG; ++signal)
            // Native spawn accepts a full default set. These two dispositions
            // are permanently default and cannot be changed with sigaction.
            if (signal != SIGKILL && signal != SIGSTOP &&
                sigismember(defaults, signal) == 1 && sigaction(signal, &action, NULL) < 0) return errno;
    }
    if (flags & POSIX_SPAWN_RESETIDS)
        if (setegid(getgid()) < 0 || seteuid(getuid()) < 0) return errno;
    if (list) for (unsigned i = 0; i < list->count; ++i) {
        const struct spawn_action *action = &list->actions[i];
        int result = 0;
        switch (action->kind) {
            case ACTION_CLOSE: result = close(action->fd); break;
            case ACTION_DUP:
                result = dup2(action->fd, action->target);
                if (result >= 0) result = fcntl(action->target, F_SETFD, 0);
                break;
            case ACTION_INHERIT: result = fcntl(action->fd, F_SETFD, 0); break;
            case ACTION_CHDIR: result = chdir(action->path); break;
            case ACTION_FCHDIR: result = fchdir(action->fd); break;
            case ACTION_OPEN: {
                (void)close(action->fd);
                int opened = open(action->path, action->flags, action->mode);
                if (opened < 0) return errno;
                result = opened == action->fd ? 0 : dup2(opened, action->fd);
                if (opened != action->fd) close(opened);
                break;
            }
        }
        if (result < 0 && !(action->kind == ACTION_CLOSE && errno == EBADF)) return errno;
    }
    for (int i = 0; i < count; ++i) if (fcntl(holds[i], F_SETFD, 0) < 0) return errno;
    if (notification >= 0 && fcntl(notification, F_SETFD, 0) < 0) return errno;
    return 0;
}

static int contained_spawn(pid_t *pid, const char *path, const struct action_list *list,
        const posix_spawnattr_t *attributes, short flags, char *const argv[], char *const environment[],
        const int *holds, int count, int notification, int minimum) {
    if (flags & (POSIX_SPAWN_SETSID | POSIX_SPAWN_SETPGROUP)) return EPERM;
    const short supported = POSIX_SPAWN_RESETIDS | POSIX_SPAWN_SETSIGDEF | POSIX_SPAWN_SETSIGMASK |
        POSIX_SPAWN_SETEXEC | POSIX_SPAWN_CLOEXEC_DEFAULT;
    if (flags & ~supported) return ENOTSUP;
    sigset_t defaults, mask;
    int error;
    if (attributes && (flags & POSIX_SPAWN_SETSIGDEF) &&
        (error = posix_spawnattr_getsigdefault(attributes, &defaults))) return error;
    if (attributes && (flags & POSIX_SPAWN_SETSIGMASK) &&
        (error = posix_spawnattr_getsigmask(attributes, &mask))) return error;
    if (flags & POSIX_SPAWN_SETEXEC) {
        error = apply_spawn_actions(list, flags, &defaults, &mask, holds, count, notification);
        if (error) return error;
        execve(path, argv, environment);
        return errno;
    }
    int pair[2], receipt[2];
    if (pipe(pair) < 0) return errno;
    receipt[0] = fcntl(pair[0], F_DUPFD_CLOEXEC, minimum);
    if (receipt[0] >= 0) minimum = receipt[0] + 1;
    receipt[1] = fcntl(pair[1], F_DUPFD_CLOEXEC, minimum);
    error = errno;
    close(pair[0]); close(pair[1]);
    if (receipt[0] < 0 || receipt[1] < 0) {
        if (receipt[0] >= 0) close(receipt[0]);
        if (receipt[1] >= 0) close(receipt[1]);
        return error;
    }
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
    pid_t parent = (pid_t)syscall(SYS_getpid);
    pid_t child = (pid_t)syscall(SYS_fork);
    // Darwin returns a separate child discriminator from the fork syscall;
    // comparing kernel PIDs also works across supported native architectures.
    if ((pid_t)syscall(SYS_getpid) != parent) child = 0;
#pragma clang diagnostic pop
    if (child < 0) { error = errno; close(receipt[0]); close(receipt[1]); return error; }
    if (!child) {
        close(receipt[0]);
        error = apply_spawn_actions(list, flags, &defaults, &mask, holds, count, notification);
        if (!error) { execve(path, argv, environment); error = errno; }
        const char *bytes = (const char *)&error;
        size_t sent = 0;
        while (sent < sizeof(error)) {
            ssize_t written = write(receipt[1], bytes + sent, sizeof(error) - sent);
            if (written < 0 && errno == EINTR) continue;
            if (written <= 0) break;
            sent += (size_t)written;
        }
        _exit(127);
    }
    close(receipt[1]);
    size_t received = 0;
    error = 0;
    while (received < sizeof(error)) {
        ssize_t bytes = read(receipt[0], (char *)&error + received, sizeof(error) - received);
        if (bytes < 0 && errno == EINTR) continue;
        if (bytes < 0) { error = errno; break; }
        if (!bytes) break;
        received += (size_t)bytes;
    }
    close(receipt[0]);
    if (received && received != sizeof(error)) error = EIO;
    if (error) { while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {} return error; }
    *pid = child;
    return 0;
}
