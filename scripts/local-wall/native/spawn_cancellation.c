/* Emulate cancellation of attached command groups without creating a kernel
 * group. Only the pinned provider can use this mapping, and each root is bound
 * to its kernel start time. All signals remain subject to same-sandbox rules. */
#include <libproc.h>
#include <sys/proc.h>
#include <time.h>

struct attached_command { struct proc_bsdinfo identity; };
static struct attached_command attached_commands[TRACKED_LIMIT];

static int command_info(pid_t pid, struct proc_bsdinfo *info) {
    return proc_pidinfo(pid, PROC_PIDTBSDINFO, 0, info, sizeof(*info)) == sizeof(*info);
}
static int same_command(const struct proc_bsdinfo *expected, struct proc_bsdinfo *current) {
    return command_info((pid_t)expected->pbi_pid, current) &&
        current->pbi_start_tvsec == expected->pbi_start_tvsec &&
        current->pbi_start_tvusec == expected->pbi_start_tvusec &&
        current->pbi_pgid == expected->pbi_pgid;
}
/* Called while spawn_process holds the adapter table lock. */
static int attached_command_slot(void) {
    struct proc_bsdinfo current;
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i)
        if (!attached_commands[i].identity.pbi_pid ||
            !same_command(&attached_commands[i].identity, &current)) return (int)i;
    return -1;
}
struct stopped_command { struct proc_bsdinfo identity; int resume; };
static double command_time(void) {
    struct timespec now;
    clock_gettime(CLOCK_MONOTONIC, &now);
    return now.tv_sec + now.tv_nsec / 1e9;
}
static int freeze_command(struct stopped_command *node, double deadline) {
    struct proc_bsdinfo current;
    if (!same_command(&node->identity, &current)) return 0;
    node->resume = current.pbi_status != SSTOP;
    if (current.pbi_status == SZOMB || !node->resume) return 0;
    if (kill((pid_t)current.pbi_pid, SIGSTOP) < 0) return errno == ESRCH ? 0 : -1;
    do {
        if (!same_command(&node->identity, &current) ||
            current.pbi_status == SSTOP || current.pbi_status == SZOMB) return 0;
        struct timespec pause = { .tv_nsec = 1000000 };
        nanosleep(&pause, NULL);
    } while (command_time() < deadline);
    errno = ETIMEDOUT;
    return -1;
}
static int cancel_command(const struct proc_bsdinfo *root, int signal) {
    struct stopped_command *tree = calloc(TRACKED_LIMIT, sizeof(*tree));
    pid_t *children = calloc(TRACKED_LIMIT, sizeof(*children));
    if (!tree || !children) { free(tree); free(children); return failure(ENOMEM); }
    unsigned count = 1;
    tree[0].identity = *root;
    int error = 0;
    double deadline = command_time() + 2;
    // Stop each parent before discovering its children, so it cannot create
    // more children while cancellation traverses the command's subtree.
    for (unsigned next = 0; next < count; ++next) {
        if (freeze_command(&tree[next], deadline) < 0) { error = errno; break; }
        errno = 0;
        int found = proc_listchildpids((pid_t)tree[next].identity.pbi_pid,
            children, TRACKED_LIMIT * sizeof(*children));
        if (found < 0 || (!found && errno)) { error = errno ? errno : EIO; break; }
        if (found >= TRACKED_LIMIT) { error = E2BIG; break; }
        for (int i = 0; i < found; ++i) {
            struct proc_bsdinfo info;
            if (!command_info(children[i], &info)) {
                if (errno == ESRCH) continue;
                error = errno ? errno : EIO; break;
            }
            if (info.pbi_ppid != tree[next].identity.pbi_pid ||
                info.pbi_pgid != root->pbi_pgid) continue;
            if (count == TRACKED_LIMIT) { error = E2BIG; break; }
            tree[count++].identity = info;
        }
        if (error) break;
    }
    if (!error) for (unsigned i = count; i-- > 0;) {
        struct proc_bsdinfo current;
        if (same_command(&tree[i].identity, &current) &&
            kill((pid_t)current.pbi_pid, signal) < 0 && errno != ESRCH) error = errno;
    }
    for (unsigned i = count; i-- > 0;) {
        struct proc_bsdinfo current;
        if (tree[i].resume && same_command(&tree[i].identity, &current))
            (void)kill((pid_t)current.pbi_pid, SIGCONT);
    }
    free(children); free(tree);
    return error ? failure(error) : 0;
}
static int wrapped_kill(pid_t pid, int signal) {
    if (!adapter_ready() || !wall_contained_spawns || !attached_spawn_caller() ||
        (signal != SIGTERM && signal != SIGKILL) || pid == 0 || pid == -1 || pid == INT_MIN)
        return kill(pid, signal);
    pid_t root = pid < 0 ? -pid : pid;
    lock_table();
    int result = -1, mapped = 0;
    for (unsigned i = 0; i < TRACKED_LIMIT; ++i) {
        const struct proc_bsdinfo *identity = &attached_commands[i].identity;
        if (identity->pbi_pid != (uint32_t)root) continue;
        mapped = 1;
        struct proc_bsdinfo current;
        if (!same_command(identity, &current)) result = failure(ESRCH);
        else result = cancel_command(identity, signal);
        break;
    }
    int saved = errno;
    unlock_table();
    errno = saved;
    return mapped ? result : kill(pid, signal);
}
