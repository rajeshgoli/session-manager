/* Included by adapter.c. Hold the descriptor table stable until exec commits
 * or fails; the replacement image recovers leases from kernel socket state. */
#include <spawn.h>
#include <crt_externs.h>
#include <stdio.h>
#include "exec_image.c"
extern const char wall_image_path[];
#include "spawn_actions.c"

static _Thread_local int executing;
static _Thread_local int inherit_exec;
static int enter_exec(void) {
    if (!adapter_ready()) return 0;
    if (executing) return 0;
    lock_table();
    if (refresh() < 0) { int saved = errno; unlock_table(); return failure(saved); }
    inherit_exec = 0;
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) {
        if (entries[i].fd < 0) continue;
        int flags = fcntl(entries[i].fd, F_GETFD);
        if (flags >= 0 && !(flags & FD_CLOEXEC)) inherit_exec = 1;
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
static void free_exec_environment(char **environment);
static char **exec_environment(char *const environment[], int insert, int notification) {
    size_t count = 0;
    while (environment && environment[count]) {
        if (++count > 65536) { errno = E2BIG; return NULL; }
    }
    char **copy = calloc(count + 3, sizeof(*copy));
    if (!copy) return NULL;
    size_t used = 0;
    // An inherited listener must reload this exact immutable image. Discard
    // caller loader overrides without changing ordinary environment values.
    for (size_t i = 0; i < count; ++i)
        if (strncmp(environment[i], "DYLD_INSERT_LIBRARIES=", 22) &&
            strncmp(environment[i], "SM_WALL_RECOVERY_FD=", 20)) copy[used++] = environment[i];
    if (insert) {
        size_t size = sizeof("DYLD_INSERT_LIBRARIES=") + strlen(wall_image_path);
        copy[count + 1] = malloc(size);
        if (!copy[count + 1]) { free(copy); return NULL; }
        snprintf(copy[count + 1], size, "DYLD_INSERT_LIBRARIES=%s", wall_image_path);
        copy[used++] = copy[count + 1];
        // Move ownership out of the environment vector's null terminator.
        copy[count + 1] = NULL;
    }
    if (notification >= 0) {
        copy[used] = malloc(64);
        if (!copy[used]) { free_exec_environment(copy); return NULL; }
        snprintf(copy[used++], 64, "SM_WALL_RECOVERY_FD=%d", notification);
    }
    return copy;
}
static void free_exec_environment(char **environment) {
    if (!environment) return;
    for (size_t i = 0; environment[i]; ++i)
        if (!strncmp(environment[i], "DYLD_INSERT_LIBRARIES=", 22) ||
            !strncmp(environment[i], "SM_WALL_RECOVERY_FD=", 20)) free(environment[i]);
    free(environment);
}
static int execute_path(const char *path, char *const argv[], char *const environment[]) {
    char canonical[PATH_MAX];
    if (inherit_exec) {
        if (immutable_executable(path, canonical) < 0) return -1;
        path = canonical;
    }
    int insert = image_permits(path) == 0;
    int permission_error = errno;
    if (!insert && inherit_exec) return failure(permission_error ? permission_error : EACCES);
    char **prepared = exec_environment(environment, insert, -1);
    if (!prepared) return -1;
    int result = execve(path, argv, prepared), saved = errno;
    free_exec_environment(prepared);
    return failure(saved == 0 && result < 0 ? EACCES : saved);
}
static int execute_shell(const char *path, char *const argv[], char *const environment[]) {
    size_t count = 0;
    while (argv[count]) if (++count > 65536) return failure(E2BIG);
    char **shell = calloc(count + 3, sizeof(*shell));
    if (!shell) return -1;
    shell[0] = "/bin/sh"; shell[1] = (char *)path;
    for (size_t i = 1; i < count; ++i) shell[i + 1] = argv[i];
    execute_path(shell[0], shell, environment);
    int saved = errno; free(shell); return failure(saved);
}
static int execute_search(const char *path, const char *search, char *const argv[], char *const environment[]) {
    if (strchr(path, '/')) {
        execute_path(path, argv, environment);
        return errno == ENOEXEC && !inherit_exec ? execute_shell(path, argv, environment) : -1;
    }
    if (!search) search = "/usr/bin:/bin";
    int denied = 0;
    const char *start = search;
    for (;;) {
        const char *end = strchr(start, ':');
        size_t length = end ? (size_t)(end - start) : strlen(start);
        char candidate[PATH_MAX];
        if (length + strlen(path) + 2 > sizeof(candidate)) return failure(ENAMETOOLONG);
        if (length) snprintf(candidate, sizeof(candidate), "%.*s/%s", (int)length, start, path);
        else snprintf(candidate, sizeof(candidate), "%s", path);
        execute_path(candidate, argv, environment);
        if (errno == EACCES) denied = 1;
        else if (errno == ENOEXEC && !inherit_exec) {
            return execute_shell(candidate, argv, environment);
        } else if (errno != ENOENT && errno != ENOTDIR) return -1;
        if (!end) break;
        start = end + 1;
    }
    return failure(denied ? EACCES : ENOENT);
}
static int wrapped_execve(const char *path, char *const argv[], char *const env[]) {
    if (!adapter_ready()) return execve(path, argv, env);
    int owned = enter_exec();
    if (owned < 0) return -1;
    return leave_exec(owned, execute_path(path, argv, env));
}
static int wrapped_execv(const char *path, char *const argv[]) {
    if (!adapter_ready()) return execv(path, argv);
    int owned = enter_exec();
    if (owned < 0) return -1;
    return leave_exec(owned, execute_path(path, argv, *_NSGetEnviron()));
}
static int wrapped_execvp(const char *path, char *const argv[]) {
    if (!adapter_ready()) return execvp(path, argv);
    int owned = enter_exec();
    if (owned < 0) return -1;
    return leave_exec(owned, execute_search(path, getenv("PATH"), argv, *_NSGetEnviron()));
}
static int wrapped_execvP(const char *path, const char *search, char *const argv[]) {
    if (!adapter_ready()) return execvP(path, search, argv);
    int owned = enter_exec();
    if (owned < 0) return -1;
    return leave_exec(owned, execute_search(path, search, argv, *_NSGetEnviron()));
}
static int variable_exec(const char *path, const char *first, va_list arguments, int search, int custom_environment) {
    int owned = enter_exec();
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
    int result = search ? execute_search(path, getenv("PATH"), argv, env) : execute_path(path, argv, env);
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
#include <signal.h>
#include <sys/wait.h>
#include "spawn_lifecycle.c"

static int wrapped_posix_spawn(pid_t *pid, const char *path,
        const posix_spawn_file_actions_t *actions, const posix_spawnattr_t *attributes,
        char *const argv[], char *const env[]) {
    return spawn_process(pid, path, actions, attributes, argv, env, 0);
}
static int wrapped_posix_spawnp(pid_t *pid, const char *path,
        const posix_spawn_file_actions_t *actions, const posix_spawnattr_t *attributes,
        char *const argv[], char *const env[]) {
    return spawn_process(pid, path, actions, attributes, argv, env, 1);
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
INTERPOSE(wrapped_kill, kill);
