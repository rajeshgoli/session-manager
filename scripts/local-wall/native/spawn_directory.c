/* Resolve directory actions without changing the parent's cwd or performing
 * user open side effects. Only directory descriptors are opened here. */
static int action_directory(const struct action_list *list, unsigned before, int fd, const int *directories) {
    for (unsigned i = before; i > 0; --i) {
        const struct spawn_action *action = &list->actions[i - 1];
        if (action->kind == ACTION_DUP && action->target == fd)
            return action_directory(list, i - 1, action->fd, directories);
        if (action->fd != fd) continue;
        if (action->kind == ACTION_CLOSE) return failure(EBADF);
        if (action->kind == ACTION_OPEN)
            return openat(directories[i - 1], action->path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    }
    int copy = fcntl(fd, F_DUPFD_CLOEXEC, 3);
    if (copy < 0) return -1;
    struct stat status;
    if (fstat(copy, &status) < 0 || !S_ISDIR(status.st_mode)) { close(copy); return failure(ENOTDIR); }
    return copy;
}
static int child_executable_path(const struct action_list *list, const char *path, char resolved[PATH_MAX]) {
    if (!list || path[0] == '/') {
        if (strlen(path) >= PATH_MAX) return failure(ENAMETOOLONG);
        strcpy(resolved, path); return 0;
    }
    int owned[ACTION_LIMIT + 1], directories[ACTION_LIMIT];
    unsigned count = 0;
    int current = open(".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    if (current < 0) return -1;
    owned[count++] = current;
    int result = -1;
    for (unsigned i = 0; i < list->count; ++i) {
        directories[i] = current;
        const struct spawn_action *action = &list->actions[i];
        int next = -1;
        if (action->kind == ACTION_CHDIR)
            next = openat(current, action->path, O_RDONLY | O_DIRECTORY | O_CLOEXEC);
        else if (action->kind == ACTION_FCHDIR)
            next = action_directory(list, i, action->fd, directories);
        else continue;
        if (next < 0) goto done;
        current = next; owned[count++] = next;
    }
    char directory[PATH_MAX];
    if (fcntl(current, F_GETPATH, directory) < 0) goto done;
    if (strlen(directory) + strlen(path) + 2 > PATH_MAX) { errno = ENAMETOOLONG; goto done; }
    snprintf(resolved, PATH_MAX, "%s/%s", directory, path);
    result = 0;
done: {
    int saved = errno;
    for (unsigned i = 0; i < count; ++i) close(owned[i]);
    errno = saved; return result;
}}
