/* Included by adapter.c: safe interim refusal before #1998 implements exec
 * and posix_spawn lease recovery. Never leave hidden lease descriptors alive
 * in an executable that cannot close them with its application descriptors. */
#include <spawn.h>
#include <crt_externs.h>

static _Thread_local int executing;
static int enter_exec(int file_actions) {
    if (!adapter_ready()) return 0;
    if (executing) return 0;
    lock_table();
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) {
        if (entries[i].fd < 0) continue;
        int flags = fcntl(entries[i].fd, F_GETFD);
        if (flags >= 0 && (file_actions || !(flags & FD_CLOEXEC))) {
            unlock_table();
            return failure(EACCES);
        }
    }
    executing = 1;
    return 1;
}
static int leave_exec(int owned, int result) {
    int saved = errno;
    if (owned) { executing = 0; unlock_table(); }
    errno = saved;
    return result;
}
static int wrapped_execve(const char *path, char *const argv[], char *const env[]) {
    int owned = enter_exec(0);
    if (owned < 0) return -1;
    return leave_exec(owned, execve(path, argv, env));
}
static int wrapped_execv(const char *path, char *const argv[]) {
    int owned = enter_exec(0);
    if (owned < 0) return -1;
    return leave_exec(owned, execv(path, argv));
}
static int wrapped_execvp(const char *path, char *const argv[]) {
    int owned = enter_exec(0);
    if (owned < 0) return -1;
    return leave_exec(owned, execvp(path, argv));
}
static int wrapped_execvP(const char *path, const char *search, char *const argv[]) {
    int owned = enter_exec(0);
    if (owned < 0) return -1;
    return leave_exec(owned, execvP(path, search, argv));
}
static int variable_exec(const char *path, const char *first, va_list arguments, int search, int custom_environment) {
    int owned = enter_exec(0);
    if (owned < 0) return -1;
    va_list count_arguments;
    va_copy(count_arguments, arguments);
    size_t count = first ? 1 : 0;
    if (first) {
        while (va_arg(count_arguments, char *) != NULL) {
            if (++count > 4096) { va_end(count_arguments); return leave_exec(owned, failure(E2BIG)); }
        }
    }
    va_end(count_arguments);
    char **argv = calloc(count + 1, sizeof(*argv));
    if (!argv) return leave_exec(owned, -1);
    if (first) {
        argv[0] = (char *)first;
        for (size_t i = 1; i < count; ++i) argv[i] = va_arg(arguments, char *);
        (void)va_arg(arguments, char *); /* terminating NULL */
    }
    char *const *env = custom_environment ? va_arg(arguments, char *const *) : *_NSGetEnviron();
    int result = search ? execvp(path, argv) : execve(path, argv, env);
    int saved = errno; free(argv); errno = saved;
    return leave_exec(owned, result);
}
static int wrapped_execl(const char *path, const char *first, ...) {
    va_list arguments; va_start(arguments, first);
    int result = variable_exec(path, first, arguments, 0, 0);
    va_end(arguments); return result;
}
static int wrapped_execlp(const char *path, const char *first, ...) {
    va_list arguments; va_start(arguments, first);
    int result = variable_exec(path, first, arguments, 1, 0);
    va_end(arguments); return result;
}
static int wrapped_execle(const char *path, const char *first, ...) {
    va_list arguments; va_start(arguments, first);
    int result = variable_exec(path, first, arguments, 0, 1);
    va_end(arguments); return result;
}
static int wrapped_posix_spawn(pid_t *pid, const char *path,
        const posix_spawn_file_actions_t *actions, const posix_spawnattr_t *attributes,
        char *const argv[], char *const env[]) {
    int owned = enter_exec(actions != NULL);
    if (owned < 0) return EACCES;
    return leave_exec(owned, posix_spawn(pid, path, actions, attributes, argv, env));
}
static int wrapped_posix_spawnp(pid_t *pid, const char *path,
        const posix_spawn_file_actions_t *actions, const posix_spawnattr_t *attributes,
        char *const argv[], char *const env[]) {
    int owned = enter_exec(actions != NULL);
    if (owned < 0) return EACCES;
    return leave_exec(owned, posix_spawnp(pid, path, actions, attributes, argv, env));
}

INTERPOSE(wrapped_execve, execve);
INTERPOSE(wrapped_execv, execv);
INTERPOSE(wrapped_execvp, execvp);
INTERPOSE(wrapped_execvP, execvP);
INTERPOSE(wrapped_execl, execl);
INTERPOSE(wrapped_execlp, execlp);
INTERPOSE(wrapped_execle, execle);
INTERPOSE(wrapped_posix_spawn, posix_spawn);
INTERPOSE(wrapped_posix_spawnp, posix_spawnp);
