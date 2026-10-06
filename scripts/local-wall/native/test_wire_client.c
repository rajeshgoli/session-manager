#include "wire_client.h"
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/resource.h>
#include <unistd.h>

static unsigned descriptor_count(void) {
    struct rlimit limit;
    assert(getrlimit(RLIMIT_NOFILE, &limit) == 0 && limit.rlim_cur <= 65536);
    unsigned count = 0;
    for (int fd = 0; fd < (int)limit.rlim_cur; ++fd) if (fcntl(fd, F_GETFD) >= 0) ++count;
    return count;
}

int main(int argc, char **argv) {
    assert(argc == 10 || argc == 11);
    struct wall_configuration configuration = { .endpoint = argv[1] };
    for (int i = 0; i < 8; ++i) {
        char *end = NULL;
        unsigned long token = strtoul(argv[i + 2], &end, 10);
        assert(end && !*end && token <= UINT32_MAX);
        configuration.peer_token[i] = (uint32_t)token;
    }
    if (argc == 11) {
        assert(!strcmp(argv[10], "malformed"));
        unsigned before = descriptor_count();
        for (int i = 0; i < 5; ++i) {
            struct wall_socket rejected;
            assert(wall_bind(&configuration, 1, 0, 0, &rejected) == -1);
            assert(errno == EPROTO || errno == ENOTSOCK || errno == ECONNRESET);
            assert(rejected.descriptor == -1 && rejected.control == -1);
            assert(descriptor_count() == before);
        }
        puts("malformed replies and descriptor cleanup passed");
        return 0;
    }
    struct wall_configuration forged = configuration;
    forged.peer_token[7] ^= 1;
    struct wall_socket rejected;
    assert(wall_bind(&forged, 1, 0, 0, &rejected) == -1 && errno == EACCES);
    assert(rejected.descriptor == -1 && rejected.control == -1);
    assert(wall_bind(&configuration, 3, 0, 0, &rejected) == -1 && errno == EINVAL);
    assert(wall_bind(&configuration, 1, 8420, 0, &rejected) == -1 && errno == EACCES);
    assert(rejected.descriptor == -1 && rejected.control == -1);
    for (uint8_t family = 1; family <= 2; ++family) {
        assert(wall_retain(&configuration, family, 24000, &rejected) == -1 && errno == EACCES);
        assert(rejected.descriptor == -1 && rejected.control == -1);
        struct wall_socket listener, outgoing;
        assert(wall_bind(&configuration, family, 0, 0, &listener) == 0);
        assert(listener.port && listener.lease && listener.descriptor >= 0 && listener.control >= 0);
        assert(fcntl(listener.descriptor, F_GETFD) & FD_CLOEXEC);
        assert(fcntl(listener.control, F_GETFD) & FD_CLOEXEC);
        assert(wall_connect(&configuration, family, listener.port, &outgoing) == -1 && errno == ECONNREFUSED);
        assert(wall_listen(&listener, 4097) == -1 && errno == EINVAL);
        assert(wall_listen(&listener, 8) == 0);
        struct wall_socket retained;
        assert(wall_retain(&configuration, family, listener.port, &retained) == 0);
        assert(retained.lease == listener.lease && retained.port == listener.port);
        wall_dispose(&listener);
        listener = retained;
        assert(wall_connect(&configuration, family, listener.port, &outgoing) == 0);
        assert(outgoing.control == -1 && outgoing.lease == 0);
        assert(fcntl(outgoing.descriptor, F_GETFD) & FD_CLOEXEC);
        assert(send(outgoing.descriptor, "native", 6, MSG_NOSIGNAL) == 6);
        int incoming = accept(listener.descriptor, NULL, NULL);
        assert(incoming >= 0);
        char message[6];
        assert(recv(incoming, message, sizeof(message), MSG_WAITALL) == 6);
        assert(!memcmp(message, "native", 6));
        close(incoming);
        wall_dispose(&outgoing);
        uint16_t port = listener.port;
        assert(wall_release(&listener) == 0);
        assert(listener.descriptor == -1 && listener.control == -1);
        /* Release acknowledgement precedes host destruction by a few
         * instructions. Wait boundedly for that last host copy to close. */
        int bound = -1;
        for (int attempt = 0; attempt < 100; ++attempt) {
            bound = wall_bind(&configuration, family, port, 1, &listener);
            if (!bound) break;
            assert(errno == EADDRINUSE);
            usleep(10000);
        }
        assert(bound == 0);
        wall_dispose(&listener);
    }
    puts("native wire identity, IPv4/IPv6 transfer, activation and release passed");
    return 0;
}
