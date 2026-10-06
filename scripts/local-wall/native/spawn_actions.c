/* Public file-action APIs are mirrored; never inspect libc's opaque storage.
 * A copied action list preserves user order and adds only private lease holds. */
#define ACTION_OBJECTS 64
#define ACTION_LIMIT 256
enum action_kind { ACTION_CLOSE, ACTION_DUP, ACTION_OPEN, ACTION_INHERIT, ACTION_CHDIR, ACTION_FCHDIR };
struct spawn_action { enum action_kind kind; int fd, target, flags; mode_t mode; char *path; };
struct action_list { void *key; unsigned count; struct spawn_action actions[ACTION_LIMIT]; };
static struct action_list action_lists[ACTION_OBJECTS];
static struct action_list *action_list(const posix_spawn_file_actions_t *actions) {
    if (!actions) return NULL;
    for (unsigned i = 0; i < ACTION_OBJECTS; ++i)
        if (action_lists[i].key == *actions) return &action_lists[i];
    return NULL;
}
static int wrapped_actions_init(posix_spawn_file_actions_t *actions) {
    if (!adapter_ready()) return posix_spawn_file_actions_init(actions);
    lock_table();
    struct action_list *slot = NULL;
    for (unsigned i = 0; i < ACTION_OBJECTS; ++i)
        if (!action_lists[i].key) { slot = &action_lists[i]; break; }
    int result = slot ? posix_spawn_file_actions_init(actions) : ENOMEM;
    if (!result) { memset(slot, 0, sizeof(*slot)); slot->key = *actions; }
    unlock_table(); return result;
}
static int wrapped_actions_destroy(posix_spawn_file_actions_t *actions) {
    if (!adapter_ready()) return posix_spawn_file_actions_destroy(actions);
    lock_table();
    struct action_list *list = action_list(actions);
    int result = posix_spawn_file_actions_destroy(actions);
    if (!result && list) {
        for (unsigned i = 0; i < list->count; ++i) free(list->actions[i].path);
        memset(list, 0, sizeof(*list));
    }
    unlock_table(); return result;
}
static int replay_action(posix_spawn_file_actions_t *actions, const struct spawn_action *action) {
    switch (action->kind) {
        case ACTION_CLOSE: return posix_spawn_file_actions_addclose(actions, action->fd);
        case ACTION_DUP: return posix_spawn_file_actions_adddup2(actions, action->fd, action->target);
        case ACTION_OPEN: return posix_spawn_file_actions_addopen(actions, action->fd, action->path, action->flags, action->mode);
        case ACTION_INHERIT: return posix_spawn_file_actions_addinherit_np(actions, action->fd);
        case ACTION_CHDIR: return posix_spawn_file_actions_addchdir(actions, action->path);
        case ACTION_FCHDIR: return posix_spawn_file_actions_addfchdir(actions, action->fd);
    }
    return EINVAL;
}
static int append_action(posix_spawn_file_actions_t *actions, struct spawn_action action) {
    if (!adapter_ready()) return replay_action(actions, &action);
    lock_table();
    struct action_list *list = action_list(actions);
    int result = 0;
    char *path = action.path ? strdup(action.path) : NULL;
    if (action.path && !path) result = ENOMEM;
    else if (!list || list->count == ACTION_LIMIT) result = ENOMEM;
    else result = replay_action(actions, &action);
    if (!result) {
        // libc may grow and replace its opaque allocation on every addition.
        list->key = *actions;
        action.path = path; list->actions[list->count++] = action;
    }
    else free(path);
    unlock_table(); return result;
}
static int wrapped_actions_close(posix_spawn_file_actions_t *actions, int fd) {
    return append_action(actions, (struct spawn_action){ .kind = ACTION_CLOSE, .fd = fd });
}
static int wrapped_actions_dup(posix_spawn_file_actions_t *actions, int fd, int target) {
    return append_action(actions, (struct spawn_action){ .kind = ACTION_DUP, .fd = fd, .target = target });
}
static int wrapped_actions_open(posix_spawn_file_actions_t *actions, int fd, const char *path, int flags, mode_t mode) {
    return append_action(actions, (struct spawn_action){ .kind = ACTION_OPEN, .fd = fd, .path = (char *)path, .flags = flags, .mode = mode });
}
static int wrapped_actions_inherit(posix_spawn_file_actions_t *actions, int fd) {
    return append_action(actions, (struct spawn_action){ .kind = ACTION_INHERIT, .fd = fd });
}
static int wrapped_actions_chdir(posix_spawn_file_actions_t *actions, const char *path) {
    return append_action(actions, (struct spawn_action){ .kind = ACTION_CHDIR, .path = (char *)path });
}
static int wrapped_actions_fchdir(posix_spawn_file_actions_t *actions, int fd) {
    return append_action(actions, (struct spawn_action){ .kind = ACTION_FCHDIR, .fd = fd });
}
INTERPOSE(wrapped_actions_init, posix_spawn_file_actions_init);
INTERPOSE(wrapped_actions_destroy, posix_spawn_file_actions_destroy);
INTERPOSE(wrapped_actions_close, posix_spawn_file_actions_addclose);
INTERPOSE(wrapped_actions_dup, posix_spawn_file_actions_adddup2);
INTERPOSE(wrapped_actions_open, posix_spawn_file_actions_addopen);
INTERPOSE(wrapped_actions_inherit, posix_spawn_file_actions_addinherit_np);
INTERPOSE(wrapped_actions_chdir, posix_spawn_file_actions_addchdir);
INTERPOSE(wrapped_actions_fchdir, posix_spawn_file_actions_addfchdir);
#pragma clang diagnostic push
#pragma clang diagnostic ignored "-Wdeprecated-declarations"
INTERPOSE(wrapped_actions_chdir, posix_spawn_file_actions_addchdir_np);
INTERPOSE(wrapped_actions_fchdir, posix_spawn_file_actions_addfchdir_np);
#pragma clang diagnostic pop
