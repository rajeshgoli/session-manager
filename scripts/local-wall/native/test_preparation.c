#include <arpa/inet.h>
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

static unsigned port(const char *url) {
    assert(url);
    const char *number = strrchr(url, ':');
    assert(number);
    return (unsigned)atoi(number + 1);
}
static int connection(unsigned number) {
    int fd = socket(AF_INET, SOCK_STREAM, 0);
    assert(fd >= 0);
    struct sockaddr_in address = { .sin_len = sizeof(address), .sin_family = AF_INET,
        .sin_port = htons((uint16_t)number), .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    if (connect(fd, (struct sockaddr *)&address, sizeof(address)) == 0) return fd;
    int error = errno; close(fd); errno = error; return -1;
}
static void exchange(unsigned number, const char *request, const char *expected) {
    int fd = connection(number); assert(fd >= 0);
    size_t length = strlen(request);
    assert(write(fd, request, length) == (ssize_t)length);
    char response[8192]; size_t used = 0;
    while (used < sizeof(response) - 1) {
        ssize_t bytes = read(fd, response + used, sizeof(response) - 1 - used);
        if (bytes < 0 && errno == EINTR) continue;
        assert(bytes >= 0);
        if (!bytes) break;
        used += (size_t)bytes;
    }
    response[used] = 0; close(fd);
    assert(strstr(response, expected));
}
int main(int argc, char **argv) {
    alarm(15);
    if (argc == 2 && !strcmp(argv[1], "hold")) {
        assert(fcntl(198, F_GETFD) < 0 && errno == EBADF);
        sleep(2); puts("durable-wall-ok"); return 0;
    }
    if (argc == 2 && (!strcmp(argv[1], "provider") || !strcmp(argv[1], "provider-extra"))) {
        if (!strcmp(argv[1], "provider-extra")) {
            assert(!strcmp(getenv("HOST_PROVIDER_PASSWORD"), "host-selected-password"));
            assert(!strcmp(getenv("LOCAL_AGENT_ID"), "wall-a"));
        }
        assert(fcntl(198, F_GETFD) >= 0);
        struct sockaddr_in address;
        socklen_t size = sizeof(address);
        assert(getsockname(198, (struct sockaddr *)&address, &size) == 0);
        int listener = socket(AF_INET, SOCK_STREAM, 0); assert(listener >= 0);
        assert(bind(listener, (struct sockaddr *)&address, size) == 0);
        assert(listen(listener, 1) == 0);
        puts("provider-ready"); fflush(stdout);
        int client = accept(listener, NULL, NULL); assert(client >= 0);
        assert(write(client, "provider", 8) == 8);
        close(client); close(listener); close(198); return 0;
    }
    assert(argc == 5);
    assert(!getenv("ANTHROPIC_API_KEY") && !getenv("GH_TOKEN"));
    assert(fcntl(198, F_GETFD) < 0 && errno == EBADF);
    assert(!strcmp(getenv("CARGO_NET_OFFLINE"), "false"));
    char credential[4096];
    snprintf(credential, sizeof(credential), "%s/hosts.yml", getenv("GH_CONFIG_DIR"));
    FILE *file = fopen(credential, "r"); assert(file);
    char contents[128]; assert(fgets(contents, sizeof(contents), file)); fclose(file);
    assert(strstr(contents, "fixture-token"));
    exchange(port(getenv("SM_API_URL")), "GET /health HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n", getenv("LOCAL_AGENT_ID"));
    exchange(port(getenv("HTTPS_PROXY")), "CONNECT localhost:443 HTTP/1.1\r\nHost: localhost:443\r\n\r\n", "403");
    exchange(port(getenv("LOCAL_JUDGE_URL")), "GET /health HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n", "200");
    exchange((unsigned)atoi(argv[3]), "GET /health HTTP/1.1\r\nHost: fixture\r\nConnection: close\r\n\r\n", "model");
    for (int i = 1; i <= 2; ++i) {
        assert(connection((unsigned)atoi(argv[i])) < 0);
        assert(errno == EPERM || errno == EACCES);
    }
    assert(unlink(argv[4]) < 0 && (errno == EPERM || errno == EACCES));
    int fd = socket(AF_INET, SOCK_STREAM, 0); assert(fd >= 0);
    struct sockaddr_in address = { .sin_len = sizeof(address), .sin_family = AF_INET,
        .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    assert(bind(fd, (struct sockaddr *)&address, sizeof(address)) == 0);
    assert(listen(fd, 1) == 0);
    socklen_t size = sizeof(address); assert(getsockname(fd, (struct sockaddr *)&address, &size) == 0);
    printf("%u\n", ntohs(address.sin_port)); fflush(stdout);
    pid_t child = fork(); assert(child >= 0);
    if (!child) {
        assert(setsid() < 0 && errno == EPERM);
        sleep(30); _exit(0);
    }
    return 0;
}
