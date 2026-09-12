/* Four-core MSM8916 coherency diagnostic. Writes only anonymous RAM.
 * Build: aarch64-linux-musl-gcc -O2 -static -std=c11 -Wall -Wextra \
 *          -o smp-coherency tools/smp-coherency.c
 * Usage: smp-coherency [ring_rounds [migration_rounds [phase_timeout_seconds]]]
 * Defaults: 10000 rounds per CPU, 1000 migration rounds, 12 seconds per phase.
 * A functioning scheduler and clock are required for userspace timeouts.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <inttypes.h>
#include <limits.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#define CPUS 4
#define WORDS 256
#define BYTES (WORDS * sizeof(uint64_t))
_Static_assert(ATOMIC_INT_LOCK_FREE == 2, "process-shared token must be lock-free");

struct shared {
    _Alignas(64) atomic_uint turn;
    _Alignas(64) volatile uint64_t payload[WORDS];
};

enum outcome { PASS, AFFINITY, TIMEOUT, PATTERN, CPU_MOVED, CLOCK_ERROR };
struct result {
    unsigned cpu, completed, code, bad_word;
    int before, after;
    uint64_t expected, actual;
};
/* POSIX guarantees atomic pipe writes of at least 512 bytes. */
_Static_assert(sizeof(struct result) <= 512, "result write must be atomic");

static int64_t now_ms(void)
{
    struct timespec t;
    if (clock_gettime(CLOCK_MONOTONIC, &t)) return -1;
    return (int64_t)t.tv_sec * 1000 + t.tv_nsec / 1000000;
}

static int remaining_ms(int64_t deadline)
{
    int64_t now = now_ms();
    if (now < 0 || now >= deadline) return 0;
    return (int)(deadline - now);
}

static int pin(unsigned cpu)
{
    cpu_set_t mask;
    CPU_ZERO(&mask);
    CPU_SET(cpu, &mask);
    return sched_setaffinity(0, sizeof(mask), &mask);
}

static uint64_t pattern(uint64_t sequence, unsigned word)
{
    uint64_t x = sequence * UINT64_C(0x9e3779b97f4a7c15) ^
                 (word + 1) * UINT64_C(0xd6e8feb86659fd93);
    return x ^ (x >> 29);
}

/* Volatile payload accesses ensure these are real memory tests at -Os too.
 * Ordering between processes is provided by the token, not by volatile.
 */
static __attribute__((noinline)) void fill(volatile uint64_t *data, uint64_t seq)
{
    for (unsigned i = 0; i < WORDS; i++) data[i] = pattern(seq, i);
}

static __attribute__((noinline)) int verify(volatile uint64_t *data, uint64_t seq,
                                          struct result *r)
{
    for (unsigned i = 0; i < WORDS; i++) {
        uint64_t actual = data[i], expected = pattern(seq, i);
        if (actual != expected) {
            r->code = PATTERN;
            r->bad_word = i;
            r->actual = actual;
            r->expected = expected;
            return -1;
        }
    }
    return 0;
}

static struct result ring_worker(struct shared *s, unsigned cpu, unsigned rounds,
                                 int64_t deadline)
{
    struct result r = { .cpu = cpu, .before = -1, .after = -1 };
    if (pin(cpu)) { r.code = AFFINITY; return r; }
    r.before = sched_getcpu();
    if (r.before != (int)cpu) { r.code = CPU_MOVED; return r; }
    for (unsigned round = 0; round < rounds; round++) {
        unsigned turn = round * CPUS + cpu;
        while (atomic_load_explicit(&s->turn, memory_order_acquire) != turn) {
            if (!remaining_ms(deadline)) { r.code = TIMEOUT; goto done; }
            /* The token ring deliberately spins on separate physical CPUs. */
        }
        if (!remaining_ms(deadline)) { r.code = TIMEOUT; goto done; }
        if (verify(s->payload, turn, &r)) goto done;
        fill(s->payload, turn + 1);
        atomic_store_explicit(&s->turn, turn + 1, memory_order_release);
        r.completed++;
    }
done:
    r.after = sched_getcpu();
    if (r.code == PASS && r.after != (int)cpu) r.code = CPU_MOVED;
    return r;
}

static void print_result(const char *phase, const struct result *r)
{
    static const char *const names[] = {
        "PASS", "AFFINITY", "TIMEOUT", "PATTERN", "CPU_MOVED", "CLOCK_ERROR"
    };
    printf("phase=%s cpu=%u completed=%u before=%d after=%d result=%s",
           phase, r->cpu, r->completed, r->before, r->after,
           r->code <= CLOCK_ERROR ? names[r->code] : "INVALID_RESULT");
    if (r->code == PATTERN)
        printf(" word=%u expected=%016" PRIx64 " actual=%016" PRIx64,
               r->bad_word, r->expected, r->actual);
    putchar('\n');
}

static int read_result(int fd, struct result *r, int64_t deadline)
{
    size_t got = 0;
    while (got < sizeof(*r)) {
        struct pollfd p = { .fd = fd, .events = POLLIN };
        int left = remaining_ms(deadline), ready;
        if (!left) return -1;
        ready = poll(&p, 1, left);
        if (ready < 0 && errno == EINTR) continue;
        if (ready <= 0) return -1;
        ssize_t n = read(fd, (char *)r + got, sizeof(*r) - got);
        if (n < 0 && errno == EINTR) continue;
        if (n <= 0) return -1;
        got += n;
    }
    return 0;
}

static int ring(struct shared *s, unsigned rounds, unsigned seconds)
{
    pid_t children[CPUS] = { 0 };
    int pipefd[2], failed = 0;
    unsigned seen = 0;
    int64_t start = now_ms();
    if (start < 0 || pipe(pipefd)) return -1;
    int64_t deadline = start + seconds * 1000;
    fill(s->payload, 0);
    atomic_init(&s->turn, 0);
    for (unsigned cpu = 0; cpu < CPUS; cpu++) {
        children[cpu] = fork();
        if (children[cpu] < 0) { children[cpu] = 0; failed = 1; break; }
        if (!children[cpu]) {
            close(pipefd[0]);
            struct result r = ring_worker(s, cpu, rounds, deadline);
            ssize_t n;
            do { n = write(pipefd[1], &r, sizeof(r)); } while (n < 0 && errno == EINTR);
            _exit(n != sizeof(r) || r.code != PASS);
        }
    }
    close(pipefd[1]);
    while (!failed && seen != 0xf) {
        struct result r;
        if (read_result(pipefd[0], &r, deadline)) { failed = 1; break; }
        print_result("ring", &r);
        if (r.cpu >= CPUS || (seen & (1U << r.cpu)) || r.code != PASS ||
            r.completed != rounds || r.before != (int)r.cpu || r.after != (int)r.cpu)
            failed = 1;
        else
            seen |= 1U << r.cpu;
    }
    close(pipefd[0]);
    /* Even a worker that reports success must exit successfully. Reaping is
     * nonblocking and bounded; failure stops every remaining worker.
     */
    for (;;) {
        unsigned pending = 0;
        for (unsigned cpu = 0; cpu < CPUS; cpu++) {
            int status;
            if (!children[cpu]) continue;
            if (failed) kill(children[cpu], SIGKILL);
            pid_t got = waitpid(children[cpu], &status, WNOHANG);
            if (got == children[cpu]) {
                children[cpu] = 0;
                if (!WIFEXITED(status) || WEXITSTATUS(status)) failed = 1;
            } else if (got < 0 && errno != EINTR) {
                children[cpu] = 0;
                failed = 1;
            } else {
                pending++;
            }
        }
        if (!pending) break;
        if (!remaining_ms(deadline)) {
            for (unsigned cpu = 0; cpu < CPUS; cpu++)
                if (children[cpu]) kill(children[cpu], SIGKILL);
            failed = 1;
            break;
        }
        struct timespec pause = { .tv_nsec = 1000000 };
        nanosleep(&pause, NULL);
    }
    if (failed || seen != 0xf) {
        printf("phase=ring result=FAIL received_mask=%x elapsed_ms=%" PRId64 "\n",
               seen, now_ms() - start);
        return -1;
    }
    struct result final = { 0 };
    if (atomic_load_explicit(&s->turn, memory_order_acquire) != rounds * CPUS ||
        verify(s->payload, rounds * CPUS, &final)) return -1;
    printf("phase=ring handoffs=%u payload_bytes=%zu elapsed_ms=%" PRId64 " PASS\n",
           rounds * CPUS, BYTES, now_ms() - start);
    return 0;
}

static int migration(struct shared *s, unsigned rounds, unsigned seconds)
{
    struct result r = { .before = -1, .after = -1 };
    int64_t start = now_ms(), deadline = start + seconds * 1000;
    uint64_t sequence = 0;
    if (start < 0) { r.code = CLOCK_ERROR; goto done; }
    if (pin(CPUS - 1)) { r.code = AFFINITY; goto done; }
    fill(s->payload, sequence);
    for (unsigned round = 0; round < rounds; round++) {
        for (unsigned cpu = 0; cpu < CPUS; cpu++) {
            r.cpu = cpu;
            if (!remaining_ms(deadline)) { r.code = TIMEOUT; goto done; }
            r.before = sched_getcpu();
            if (pin(cpu)) { r.code = AFFINITY; goto done; }
            r.after = sched_getcpu();
            if (r.before != (int)((cpu + CPUS - 1) % CPUS) || r.after != (int)cpu) {
                r.code = CPU_MOVED;
                goto done;
            }
            if (verify(s->payload, sequence, &r)) goto done;
            fill(s->payload, ++sequence);
            r.completed++;
        }
    }
    if (!remaining_ms(deadline)) r.code = TIMEOUT;
done:
    print_result("migration", &r);
    printf("phase=migration transitions=%u payload_bytes=%zu elapsed_ms=%" PRId64 "\n",
           r.completed, BYTES, now_ms() - start);
    return r.code != PASS || r.completed != rounds * CPUS ? -1 : 0;
}

static unsigned argument(const char *text, unsigned maximum)
{
    char *end;
    errno = 0;
    unsigned long n = strtoul(text, &end, 10);
    if (errno || !*text || *end || !n || n > maximum) return 0;
    return (unsigned)n;
}

int main(int argc, char **argv)
{
    unsigned rounds = 10000, migrations = 1000, seconds = 12;
    if (argc > 4 || (argc > 1 && !(rounds = argument(argv[1], 1000000))) ||
        (argc > 2 && !(migrations = argument(argv[2], 100000))) ||
        (argc > 3 && !(seconds = argument(argv[3], 120)))) {
        fprintf(stderr, "usage: %s [ring_rounds:1..1000000 [migration_rounds:1..100000 [phase_timeout_seconds:1..120]]]\n", argv[0]);
        return 2;
    }
    struct shared *s = mmap(NULL, sizeof(*s), PROT_READ | PROT_WRITE,
                           MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (s == MAP_FAILED) { perror("mmap"); return 1; }
    printf("SMP_COHERENCY_V1 cpus=0,1,2,3 ring_rounds_per_cpu=%u migration_rounds=%u "
           "payload_bytes=%zu phase_timeout_seconds=%u\n", rounds, migrations, BYTES, seconds);
    fflush(stdout);
    int failed = ring(s, rounds, seconds) || migration(s, migrations, seconds);
    munmap(s, sizeof(*s));
    puts(failed ? "SMP_COHERENCY_FAIL" : "SMP_COHERENCY_PASS all_four_cpus_shared_memory_and_migration");
    return failed;
}
