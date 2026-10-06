/* Fixture observer: inspect only this process and its queue-wrapper parent.
 * lsof enumerates all host PIDs before applying -p, which the wall forbids. */
#include <arpa/inet.h>
#include <errno.h>
#include <libproc.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/proc_info.h>
#include <unistd.h>

static int inspect(pid_t pid) {
    int size = proc_pidinfo(pid, PROC_PIDLISTFDS, 0, NULL, 0);
    if (size <= 0) return -1;
    size += 64 * (int)sizeof(struct proc_fdinfo);
    struct proc_fdinfo *fds = malloc((size_t)size);
    if (!fds) return -1;
    int bytes = proc_pidinfo(pid, PROC_PIDLISTFDS, 0, fds, size);
    if (bytes <= 0 || bytes % (int)sizeof(*fds)) { free(fds); return -1; }
    int result = 0;
    for (unsigned i = 0; i < (unsigned)bytes / sizeof(*fds); ++i) {
        int fd = fds[i].proc_fd;
        if (fds[i].proc_fdtype == PROX_FDTYPE_SOCKET) {
            struct socket_fdinfo socket;
            int count = proc_pidfdinfo(pid, fd, PROC_PIDFDSOCKETINFO, &socket, sizeof(socket));
            if (count != sizeof(socket)) { if (errno != EBADF) result = -1; continue; }
            if (socket.psi.soi_kind == SOCKINFO_TCP) {
                struct in_sockinfo *info = &socket.psi.soi_proto.pri_tcp.tcpsi_ini;
                char address[INET6_ADDRSTRLEN];
                const void *value = socket.psi.soi_family == AF_INET
                    ? (const void *)&info->insi_laddr.ina_46.i46a_addr4
                    : (const void *)&info->insi_laddr.ina_6;
                if (!inet_ntop(socket.psi.soi_family, value, address, sizeof(address))) result = -1;
                else printf("n%s:%u\n", address, ntohs((unsigned short)info->insi_lport));
            } else if (socket.psi.soi_kind == SOCKINFO_UN) {
                printf("n%s\n", socket.psi.soi_proto.pri_un.unsi_addr.ua_sun.sun_path);
            }
        } else if (fds[i].proc_fdtype == PROX_FDTYPE_VNODE) {
            struct vnode_fdinfowithpath vnode;
            int count = proc_pidfdinfo(pid, fd, PROC_PIDFDVNODEPATHINFO, &vnode, sizeof(vnode));
            if (count != sizeof(vnode)) { if (errno != EBADF) result = -1; continue; }
            printf("n%s\n", vnode.pvip.vip_path);
        }
    }
    free(fds);
    return result;
}

int main(void) {
    if (inspect(getpid()) || inspect(getppid())) { perror("descriptor inspection"); return 1; }
    return 0;
}
