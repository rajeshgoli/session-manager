/* Included before initialization: kernel socket state is the only recovery
 * metadata. No environment value or writable manifest attributes a socket.
 * Initialization uses native libc calls while interposition remains disabled. */
static int recover_inherited(void) {
    int limit = getdtablesize();
    if (limit < 0) return -1;
    unsigned char *old_controls = calloc((size_t)limit, 1);
    if (!old_controls) return -1;
    // Snapshot before opening fresh connections, which may reuse free numbers.
    for (int fd = 0; fd < limit; ++fd)
        old_controls[fd] = (unsigned char)wall_is_control(&wall_configuration, fd);
    int result = -1;
    for (int fd = 0; fd < limit; ++fd) {
        if (fd == control_source || old_controls[fd]) continue;
        struct sockaddr_storage address, peer;
        socklen_t size = sizeof(address), peer_size = sizeof(peer);
        int flags = fcntl(fd, F_GETFD);
        if (flags < 0 || getsockname(fd, (struct sockaddr *)&address, &size) < 0 ||
            (address.ss_family != AF_INET && address.ss_family != AF_INET6) ||
            getpeername(fd, (struct sockaddr *)&peer, &peer_size) == 0) continue;
        uint8_t family; uint16_t port;
        if (destination((struct sockaddr *)&address, size, &family, &port) < 0 ||
            !port || tcp_socket(fd, family) < 0) continue;
        struct entry *slot = vacant();
        if (!slot) goto done;
        struct wall_socket retained = { .descriptor = -1, .control = -1 };
        if (port != wall_control_port) {
            struct entry *copy = NULL;
            for (unsigned i = 0; i < TRACKED_LIMIT; ++i)
                if (entries[i].fd >= 0 && entries[i].socket.family == family &&
                    entries[i].socket.port == port) { copy = &entries[i]; break; }
            if (copy) {
                retained = copy->socket;
                retained.control = dup(copy->socket.control);
                retained.descriptor = -1;
                if (retained.control < 0) goto done;
            } else {
                if (wall_retain(&wall_configuration, family, port, &retained) < 0) goto done;
                close(retained.descriptor);
                retained.descriptor = -1;
            }
            if (fcntl(retained.control, F_SETFD, flags) < 0) { wall_dispose(&retained); goto done; }
        }
        retained.descriptor = fd; retained.family = family; retained.port = port;
        slot->fd = fd; slot->socket = retained;
    }
    // Every surviving listener now holds a fresh lease. Old inherited streams
    // are closed without sending requests to a possibly concurrent parent.
    for (int fd = 0; fd < limit; ++fd) if (old_controls[fd]) close(fd);
    result = 0;
done:
    free(old_controls);
    return result;
}

/* This environment value identifies a completion channel only. Socket
 * authority still comes exclusively from the immutable configuration and
 * host verification. Forging the hint never supplies a lease or identity. */
static void acknowledge_recovery(void) {
    const char *value = getenv("SM_WALL_RECOVERY_FD");
    if (!value || !*value) return;
    char *end;
    long number = strtol(value, &end, 10);
    if (*end || number < 0 || number > INT_MAX) return;
    int fd = (int)number, type = 0;
    uint32_t token[8];
    socklen_t size = sizeof(type);
    if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &size) < 0 || type != SOCK_STREAM) return;
    size = sizeof(token);
    if (getsockopt(fd, SOL_LOCAL, LOCAL_PEERTOKEN, token, &size) < 0 ||
        size != sizeof(token) || token[5] != (uint32_t)getppid()) return;
    char marker[8];
    if (recv(fd, marker, sizeof(marker), MSG_DONTWAIT | MSG_PEEK) != sizeof(marker) ||
        memcmp(marker, "SMREADY1", sizeof(marker))) return;
    if (recv(fd, marker, sizeof(marker), MSG_DONTWAIT) != sizeof(marker)) return;
    (void)send(fd, "R", 1, MSG_NOSIGNAL);
    close(fd);
}
