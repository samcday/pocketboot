/* Bounded, concurrent CPU-affinity workload for the four-core MSM8916 lab.
 * Build: aarch64-linux-musl-gcc -O2 -static -o smp-work tools/smp-work.c
 * Run from the target's RAM filesystem. No device or disk writes are made.
 */
#define _GNU_SOURCE
#include <errno.h>
#include <inttypes.h>
#include <sched.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

/* A volatile seed keeps the compiler from reusing a pre-fork calculation. */
static volatile uint64_t work_seed = UINT64_C(14695981039346656037);

static uint64_t work(void)
{
    uint64_t hash = work_seed;
    for (uint64_t i = 0; i < UINT64_C(64) * 1024 * 1024; i++)
        hash = (hash ^ (i & 255)) * UINT64_C(1099511628211);
    return hash;
}

static double seconds(struct timespec t)
{
    return t.tv_sec + t.tv_nsec / 1e9;
}

int main(int argc, char **argv)
{
    (void)argv;
    struct result {
        int cpu, before, after;
        uint64_t hash;
        double elapsed;
    };
    int gate[2], results[2], status, failed = 0;
    unsigned seen = 0;
    pid_t children[4];
    const uint64_t expected = UINT64_C(0x292d74d0d0222325);

    if (argc > 1) {
        uint64_t actual = work();
        printf("expected=%016" PRIx64 " actual=%016" PRIx64 "\n", expected, actual);
        return actual != expected;
    }
    if (pipe(gate) || pipe(results)) { perror("pipe"); return 1; }
    printf("SMP_WORK_V2 expected=%016" PRIx64 " bytes_per_cpu=67108864\n", expected);
    fflush(stdout);
    for (int cpu = 0; cpu < 4; cpu++) {
        children[cpu] = fork();
        if (children[cpu] < 0) { perror("fork"); return 1; }
        if (!children[cpu]) {
            cpu_set_t mask;
            struct timespec start, end;
            char token;
            CPU_ZERO(&mask);
            CPU_SET(cpu, &mask);
            if (sched_setaffinity(0, sizeof(mask), &mask)) {
                perror("sched_setaffinity"); _exit(1);
            }
            close(gate[1]);
            close(results[0]);
            if (read(gate[0], &token, 1) != 1) _exit(1);
            int before = sched_getcpu();
            clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &start);
            uint64_t hash = work();
            clock_gettime(CLOCK_PROCESS_CPUTIME_ID, &end);
            int after = sched_getcpu();
            struct result result = { cpu, before, after, hash, seconds(end) - seconds(start) };
            /* One small pipe write is atomic; only the parent prints results. */
            _exit(write(results[1], &result, sizeof(result)) != sizeof(result));
        }
    }
    close(gate[0]);
    close(results[1]);
    if (write(gate[1], "go!!", 4) != 4) { perror("gate"); return 1; }
    close(gate[1]);
    for (int i = 0; i < 4; i++) {
        struct result result;
        size_t got = 0;
        while (got < sizeof(result)) {
            ssize_t count = read(results[0], (char *)&result + got, sizeof(result) - got);
            if (count < 0 && errno == EINTR) continue;
            if (count <= 0) { failed = 1; break; }
            got += count;
        }
        if (got != sizeof(result)) break;
        int okay = result.cpu >= 0 && result.cpu < 4 &&
            result.before == result.cpu && result.after == result.cpu &&
            result.hash == expected && result.elapsed > 0.02 &&
            !(seen & (1U << result.cpu));
        if (result.cpu >= 0 && result.cpu < 4) seen |= 1U << result.cpu;
        if (!okay) failed = 1;
        printf("cpu=%d start_cpu=%d end_cpu=%d hash=%016" PRIx64
               " cpu_seconds=%.6f %s\n", result.cpu, result.before, result.after,
               result.hash, result.elapsed, okay ? "PASS" : "FAIL");
    }
    close(results[0]);
    for (int cpu = 0; cpu < 4; cpu++) {
        if (waitpid(children[cpu], &status, 0) != children[cpu] ||
            !WIFEXITED(status) || WEXITSTATUS(status)) failed = 1;
    }
    if (seen != 0xf) failed = 1;
    puts(failed ? "SMP_WORK_FAIL" : "SMP_WORK_PASS all_four_cpus");
    return failed;
}
