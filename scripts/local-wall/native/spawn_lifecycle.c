#include "spawn_directory.c"
#include "spawn_contained.c"
#include <mach-o/dyld.h>
extern const char wall_attached_spawn_executable[];
static int attached_spawn_caller(void) {
    if (!wall_attached_spawn_executable[0]) return 0;
    char image[PATH_MAX], canonical[PATH_MAX];
    uint32_t size = sizeof(image);
    return !_NSGetExecutablePath(image, &size) && realpath(image, canonical) &&
        !strcmp(canonical, wall_attached_spawn_executable);
}
struct future_descriptor { int fd, source, keep; };
#define FUTURE_LIMIT (TRACKED_LIMIT + ACTION_LIMIT + 1)
static struct future_descriptor *future_find(struct future_descriptor *future, int fd) {
    for (unsigned i = 0; i < FUTURE_LIMIT; ++i) if (future[i].fd == fd) return &future[i];
    return NULL;
}
static int resolve_spawn_path(const char *path, int search, char result[PATH_MAX]) {
    if (!search || strchr(path, '/')) {
        if (strlen(path) >= PATH_MAX) return ENAMETOOLONG;
        strcpy(result, path); return 0;
    }
    const char *start = getenv("PATH");
    if (!start) start = "/usr/bin:/bin";
    int denied = 0;
    for (;;) {
        const char *end = strchr(start, ':');
        size_t length = end ? (size_t)(end - start) : strlen(start);
        if (length + strlen(path) + 2 >= PATH_MAX) return ENAMETOOLONG;
        if (length) snprintf(result, PATH_MAX, "%.*s/%s", (int)length, start, path);
        else snprintf(result, PATH_MAX, "%s", path);
        if (!access(result, X_OK)) return 0;
        if (errno == EACCES) denied = 1;
        else if (errno != ENOENT && errno != ENOTDIR) return errno;
        if (!end) break;
        start = end + 1;
    }
    return denied ? EACCES : ENOENT;
}
static int spawn_process(pid_t *pid, const char *path,
        const posix_spawn_file_actions_t *actions, const posix_spawnattr_t *attributes,
        char *const argv[], char *const environment[], int search) {
    if (!adapter_ready()) return search ? posix_spawnp(pid, path, actions, attributes, argv, environment) :
        posix_spawn(pid, path, actions, attributes, argv, environment);
    int owned = enter_exec();
    if (owned < 0) return errno;
    int result = 0, clone_ready = 0, holds[TRACKED_LIMIT], hold_count = 0;
    int original_flags[TRACKED_LIMIT], notification[2] = {-1, -1};
    char **prepared = NULL;
    posix_spawn_file_actions_t clone;
    struct future_descriptor future[FUTURE_LIMIT];
    for (unsigned i = 0; i < FUTURE_LIMIT; ++i) future[i].fd = -1;
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) original_flags[i] = -1;
    short spawn_flags = 0;
    if (attributes && (result = posix_spawnattr_getflags(attributes, &spawn_flags))) goto done;
    // Pinned Opencode asks to detach ordinary Git and Bash commands. For that
    // exact host-staged image, keep them in the verified launch group instead.
    // Other images still reject these flags; raw detachment stays kernel-denied.
    // Opencode falls back to signalling the child when no child group exists.
    if (wall_contained_spawns && attached_spawn_caller())
        spawn_flags &= ~(POSIX_SPAWN_SETSID | POSIX_SPAWN_SETPGROUP);
    struct action_list *list = action_list(actions);
    if (wall_contained_spawns && actions && !list) { result = EACCES; goto done; }
    int tracked = 0, minimum = 3;
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) {
        if (entries[i].fd < 0) continue;
        ++tracked;
        int flags = fcntl(entries[i].fd, F_GETFD);
        if (flags < 0) { result = errno; goto done; }
        future[i] = (struct future_descriptor){ entries[i].fd, (int)i,
            !(flags & FD_CLOEXEC) && !(spawn_flags & POSIX_SPAWN_CLOEXEC_DEFAULT) };
        if (entries[i].fd >= minimum) minimum = entries[i].fd + 1;
        if (entries[i].socket.control >= minimum) minimum = entries[i].socket.control + 1;
    }
    if (actions && !list && tracked) { result = EACCES; goto done; }
    if (list) {
        for (unsigned i = 0; i < list->count; ++i) {
            const struct spawn_action *action = &list->actions[i];
            if (action->fd == INT_MAX || action->target == INT_MAX) { result = EMFILE; goto done; }
            if (action->fd >= minimum) minimum = action->fd + 1;
            if (action->target >= minimum) minimum = action->target + 1;
            struct future_descriptor *source = future_find(future, action->fd);
            if (action->kind == ACTION_CLOSE || action->kind == ACTION_OPEN) {
                if (source) source->fd = -1;
            } else if (action->kind == ACTION_INHERIT) {
                if (source) source->keep = 1;
            } else if (action->kind == ACTION_DUP) {
                struct future_descriptor copied = { .fd = -1 };
                if (source) copied = *source;
                struct future_descriptor *target = future_find(future, action->target);
                if (target) target->fd = -1;
                if (copied.fd >= 0) {
                    if (!target) for (unsigned j = 0; j < FUTURE_LIMIT; ++j)
                        if (future[j].fd < 0) { target = &future[j]; break; }
                    if (!target) { result = EMFILE; goto done; }
                    *target = (struct future_descriptor){ action->target, copied.source, 1 };
                }
            }
        }
    }
    int inherited = 0;
    for (unsigned i = 0; i < FUTURE_LIMIT; ++i)
        if (future[i].fd >= 0 && future[i].keep) ++inherited;
    // These modes cannot complete the child recovery handshake. Forward them
    // unchanged when there are no surviving adapted descriptors.
    int asynchronous_start = spawn_flags & POSIX_SPAWN_START_SUSPENDED;
    int replaces_current = spawn_flags & POSIX_SPAWN_SETEXEC;
    if (asynchronous_start && inherited) { result = EACCES; goto done; }
    char resolved[PATH_MAX];
    if ((result = resolve_spawn_path(path, search, resolved))) goto done;
    char child_path[PATH_MAX];
    if (child_executable_path(list, resolved, child_path) < 0) { result = errno; goto done; }
    strcpy(resolved, child_path);
    if (inherited) {
        char canonical[PATH_MAX];
        if (immutable_executable(resolved, canonical) < 0) { result = errno; goto done; }
        strcpy(resolved, canonical);
    }
    int insert = image_permits(resolved) == 0;
    int permission_error = errno;
    if (!insert && inherited) {
        result = permission_error ? permission_error : EACCES;
        goto done;
    }
    if (actions && !list) insert = 0;
    if (asynchronous_start) insert = 0;
    if ((result = posix_spawn_file_actions_init(&clone))) goto done;
    clone_ready = 1;
    if (list) for (unsigned i = 0; i < list->count; ++i) {
        if ((result = replay_action(&clone, &list->actions[i]))) goto done;
    }
    // Original hidden streams never leak into an unadapted child. Temporary
    // holds live above all caller action descriptors and survive only in the
    // child's copied action list; their parent copies always stay close-on-exec.
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) {
        int fd = entries[i].socket.control;
        if (entries[i].fd < 0 || fd < 0) continue;
        original_flags[i] = fcntl(fd, F_GETFD);
        if (original_flags[i] < 0 || fcntl(fd, F_SETFD, original_flags[i] | FD_CLOEXEC) < 0) { result = errno; goto done; }
        int needed = 0;
        for (unsigned j = 0; j < FUTURE_LIMIT; ++j)
            if (future[j].fd >= 0 && future[j].keep && future[j].source == (int)i) needed = 1;
        if (!needed) continue;
        int hold = fcntl(fd, F_DUPFD_CLOEXEC, minimum);
        if (hold < 0) { result = errno; goto done; }
        minimum = hold + 1; holds[hold_count++] = hold;
        // SETEXEC uses the caller's descriptor table: its private holds must
        // already survive exec, rather than relying only on inherit actions.
        if (replaces_current && fcntl(hold, F_SETFD, 0) < 0) { result = errno; goto done; }
        if ((result = posix_spawn_file_actions_addinherit_np(&clone, hold))) goto done;
    }
    if (insert && !replaces_current) {
        int pair[2];
        if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) < 0) { result = errno; goto done; }
        notification[0] = fcntl(pair[0], F_DUPFD_CLOEXEC, minimum);
        if (notification[0] >= 0) minimum = notification[0] + 1;
        notification[1] = fcntl(pair[1], F_DUPFD_CLOEXEC, minimum);
        close(pair[0]); close(pair[1]);
        if (notification[1] < 0 || fcntl(notification[0], F_SETFD, FD_CLOEXEC) < 0) { result = errno; goto done; }
        if (send(notification[0], "SMREADY1", 8, MSG_NOSIGNAL) != 8) { result = EIO; goto done; }
        if ((result = posix_spawn_file_actions_addinherit_np(&clone, notification[1]))) goto done;
    }
    prepared = exec_environment(environment, insert, notification[1]);
    if (!prepared) { result = errno; goto done; }
    pid_t child;
    result = wall_contained_spawns ? contained_spawn(&child, resolved, list, attributes, spawn_flags,
        argv, prepared, holds, hold_count, notification[1], minimum) :
        posix_spawn(&child, resolved, actions && !list ? actions : &clone, attributes, argv, prepared);
    if (!result && insert && !replaces_current) {
        close(notification[1]); notification[1] = -1;
        struct pollfd item = { .fd = notification[0], .events = POLLIN };
        char reply = 0;
        int ready;
        do { ready = poll(&item, 1, 2000); } while (ready < 0 && errno == EINTR);
        if (ready <= 0 || recv(notification[0], &reply, 1, MSG_DONTWAIT) != 1 || reply != 'R') {
            // Do not return success for a process that ignored the adapter.
            (void)kill(child, SIGKILL);
            while (waitpid(child, NULL, 0) < 0 && errno == EINTR) {}
            result = EACCES;
        }
    }
    if (!result && pid) *pid = child;
done:
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i)
        if (original_flags[i] >= 0) (void)fcntl(entries[i].socket.control, F_SETFD, original_flags[i]);
    for (int i = 0; i < hold_count; ++i) close(holds[i]);
    for (unsigned i = 0; i < 2; ++i) if (notification[i] >= 0) close(notification[i]);
    if (clone_ready) posix_spawn_file_actions_destroy(&clone);
    free_exec_environment(prepared);
    (void)leave_exec(owned, result);
    return result;
}
