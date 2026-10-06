#ifndef SM_WALL_WIRE_CLIENT_H
#define SM_WALL_WIRE_CLIENT_H
#include <stdint.h>

/* The host compiles these values into the immutable adapter. Environment
 * variables never choose a broker endpoint or its trusted identity. */
struct wall_configuration {
    const char *endpoint;
    uint32_t peer_token[8];
};

struct wall_socket {
    int descriptor;
    int control;
    uint64_t lease;
    uint16_t port;
    uint8_t family; /* protocol family: 1 = IPv4, 2 = IPv6 */
};

int wall_bind(const struct wall_configuration *, uint8_t, uint16_t, int,
              struct wall_socket *);
int wall_connect(const struct wall_configuration *, uint8_t, uint16_t,
                 struct wall_socket *);
int wall_retain(const struct wall_configuration *, uint8_t, uint16_t,
                struct wall_socket *);
int wall_listen(struct wall_socket *, unsigned);
int wall_release(struct wall_socket *);
int wall_is_control(const struct wall_configuration *, int);
void wall_dispose(struct wall_socket *);
#endif
