/*
 * udpsniff: LD_PRELOAD syscall-level capture of BeaterCore's UDP traffic.
 *
 * The game's own relay is useless for observing the initial handshake: the
 * client only starts transmitting once the server answers from the *address it
 * dialed*. Interposing sendto/recvfrom below the app sees every datagram in
 * both directions regardless of how the game routes them internally.
 *
 * Log format (one line per datagram, space-delimited, newline-terminated):
 *   <monotonic_ns> <S|R> <fd> <nbytes> <peer> <hex>
 * S = send/sendto/sendmsg, R = recv/recvfrom/recvmsg.
 * <peer> is "ip:port" for sendto, and the source for recvfrom; "-" if unknown.
 *
 * Env:
 *   BC_SNIFF_LOG   output path (default: no capture)
 *   BC_SNIFF_PORT  if set, only record datagrams whose peer port matches
 *                  (filters out Steam's own sdping/relay sockets)
 *   BC_SNIFF_MAX   stop capturing after N datagrams
 *
 * Build:  gcc -O2 -shared -fPIC -o udpsniff.so udpsniff.c -ldl
 * Use:    BC_SNIFF_LOG=/tmp/cap.txt LD_PRELOAD=./udpsniff.so ./beaterCore
 */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <sys/socket.h>
#include <sys/types.h>

static ssize_t (*real_sendto)(int, const void *, size_t, int,
                              const struct sockaddr *, socklen_t);
static ssize_t (*real_recvfrom)(int, void *, size_t, int,
                                struct sockaddr *, socklen_t *);
static ssize_t (*real_sendmsg)(int, const struct msghdr *, int);
static ssize_t (*real_recvmsg)(int, struct msghdr *, int);
static ssize_t (*real_send)(int, const void *, size_t, int);
static ssize_t (*real_recv)(int, void *, size_t, int);

static FILE *log_fp;
static int initialized;
static int capture_enabled = 1;
static long long seq;
static int want_port;

static void init_once(void)
{
    if (initialized)
        return;
    initialized = 1;

    real_sendto = dlsym(RTLD_NEXT, "sendto");
    real_recvfrom = dlsym(RTLD_NEXT, "recvfrom");
    real_sendmsg = dlsym(RTLD_NEXT, "sendmsg");
    real_recvmsg = dlsym(RTLD_NEXT, "recvmsg");
    real_send = dlsym(RTLD_NEXT, "send");
    real_recv = dlsym(RTLD_NEXT, "recv");

    const char *path = getenv("BC_SNIFF_LOG");
    if (path && *path) {
        log_fp = fopen(path, "w");
        if (!log_fp)
            log_fp = stderr;
        setvbuf(log_fp, NULL, _IOFBF, 1 << 20);
    }

    const char *p = getenv("BC_SNIFF_PORT");
    if (p && *p)
        want_port = atoi(p);
}

static long long now_ns(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (long long)ts.tv_sec * 1000000000LL + ts.tv_nsec;
}

/* Render a sockaddr as "ip:port"; returns "-" when it is not IPv4. */
static const char *peer_str(const struct sockaddr *sa, socklen_t len,
                            char *out, size_t outlen)
{
    if (!sa || len < sizeof(struct sockaddr_in) || sa->sa_family != AF_INET)
        return "-";
    const struct sockaddr_in *in = (const struct sockaddr_in *)sa;
    char ip[INET_ADDRSTRLEN];
    if (!inet_ntop(AF_INET, &in->sin_addr, ip, sizeof(ip)))
        return "-";
    snprintf(out, outlen, "%s:%u", ip, ntohs(in->sin_port));
    return out;
}

static int peer_port(const struct sockaddr *sa, socklen_t len)
{
    if (!sa || len < sizeof(struct sockaddr_in) || sa->sa_family != AF_INET)
        return -1;
    return ntohs(((const struct sockaddr_in *)sa)->sin_port);
}

static void dump(char dir, int fd, const void *buf, ssize_t n,
                 const struct sockaddr *peer, socklen_t plen)
{
    if (!log_fp || !capture_enabled || n <= 0)
        return;

    /* Only UDP is interesting, and only non-empty payloads. */
    int type = 0;
    socklen_t tl = sizeof(type);
    if (getsockopt(fd, SOL_SOCKET, SO_TYPE, &type, &tl) != 0)
        return;
    if (type != SOCK_DGRAM)
        return;

    if (want_port && peer_port(peer, plen) != want_port)
        return;

    static const char hexd[] = "0123456789abcdef";
    const unsigned char *p = buf;
    size_t limit = (size_t)n > 65535 ? 65535 : (size_t)n;
    char hex[65536 * 2 + 1];
    for (size_t i = 0; i < limit; i++) {
        hex[i * 2] = hexd[p[i] >> 4];
        hex[i * 2 + 1] = hexd[p[i] & 15];
    }
    hex[limit * 2] = 0;

    char pb[64];
    const char *ps = peer_str(peer, plen, pb, sizeof(pb));

    flockfile(log_fp);
    fprintf(log_fp, "%lld %c %d %zd %s %s\n", now_ns(), dir, fd, n, ps, hex);
    funlockfile(log_fp);

    const char *maxs = getenv("BC_SNIFF_MAX");
    if (maxs && *maxs && ++seq >= atoll(maxs))
        capture_enabled = 0;
}

ssize_t sendto(int fd, const void *buf, size_t len, int flags,
               const struct sockaddr *addr, socklen_t alen)
{
    init_once();
    ssize_t r = real_sendto(fd, buf, len, flags, addr, alen);
    if (r > 0)
        dump('S', fd, buf, r, addr, alen);
    return r;
}

ssize_t recvfrom(int fd, void *buf, size_t len, int flags,
                 struct sockaddr *addr, socklen_t *alen)
{
    init_once();
    ssize_t r = real_recvfrom(fd, buf, len, flags, addr, alen);
    if (r > 0)
        dump('R', fd, buf, r, addr, alen ? *alen : 0);
    return r;
}

ssize_t sendmsg(int fd, const struct msghdr *msg, int flags)
{
    init_once();
    ssize_t r = real_sendmsg(fd, msg, flags);
    if (r > 0 && msg->msg_iovlen > 0)
        dump('S', fd, msg->msg_iov[0].iov_base, r,
             (const struct sockaddr *)msg->msg_name, msg->msg_namelen);
    return r;
}

ssize_t recvmsg(int fd, struct msghdr *msg, int flags)
{
    init_once();
    ssize_t r = real_recvmsg(fd, msg, flags);
    if (r > 0 && msg->msg_iovlen > 0)
        dump('R', fd, msg->msg_iov[0].iov_base, r,
             (const struct sockaddr *)msg->msg_name, msg->msg_namelen);
    return r;
}

ssize_t send(int fd, const void *buf, size_t len, int flags)
{
    init_once();
    ssize_t r = real_send(fd, buf, len, flags);
    if (r > 0)
        dump('S', fd, buf, r, NULL, 0);
    return r;
}

ssize_t recv(int fd, void *buf, size_t len, int flags)
{
    init_once();
    ssize_t r = real_recv(fd, buf, len, flags);
    if (r > 0)
        dump('R', fd, buf, r, NULL, 0);
    return r;
}
