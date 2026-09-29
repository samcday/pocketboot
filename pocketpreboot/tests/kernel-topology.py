#!/usr/bin/env python3
"""Compile the retained patch's actual index/topology checks with host DT stubs."""
import os
from pathlib import Path
import subprocess
import tempfile

patch = Path(__file__).resolve().parents[2] / (
    "patches/kernel/msm8939/0001-arm64-pocketboot-spin-table-kexec.patch"
)
source = "\n".join(
    line[1:] for line in patch.read_text().splitlines()
    if line.startswith("+") and not line.startswith("+++")
)
index = source[source.index("static unsigned int pb_index"):source.index(
    "static void __iomem *pb_slot")]
checks = source[source.index("\tif ((num_possible_cpus()"):source.index(
    "\tpb_region = ioremap_cache")]
harness = r"""
#include <assert.h>
#include <stdbool.h>
#include <stdint.h>
#include <string.h>
typedef uint64_t u64;
#define PB_CLUSTER_CPUS 4
#define PB_MAX_CPUS 8
#define PB_SLOTS 0x400
#define PB_STRIDE 0x80
static unsigned int n;
static bool msm8939;
static u64 ids[8];
static int smp_spin_table_ops;
struct device_node { unsigned int cpu; };
static struct device_node nodes[8];
#define num_possible_cpus() n
#define cpu_logical_map(cpu) ids[cpu]
#define for_each_possible_cpu(cpu) for (cpu = 0; cpu < n; cpu++)
#define of_machine_is_compatible(s) msm8939
#define get_cpu_ops(cpu) (&smp_spin_table_ops)
static struct device_node *of_get_cpu_node(unsigned int cpu, void *unused)
{ return &nodes[cpu]; }
static void of_node_put(struct device_node *dn) {}
""" + index + r"""
static int of_property_read_u64(struct device_node *dn, const char *key, u64 *value)
{
    *value = PB_SLOTS + pb_index(ids[dn->cpu]) * PB_STRIDE;
    return 0;
}
static bool topology(void)
{
    unsigned int cpu, seen = 0;
    struct { u64 start; } res = {0};
""" + checks + r"""
    return true;
out:
    return false;
}
int main(void)
{
    const u64 valid[] = {0x100, 1, 2, 3, 0, 0x101, 0x102, 0x103};
    for (unsigned int i = 0; i < 8; i++) nodes[i].cpu = i;
    n = 8; msm8939 = true;
    memcpy(ids, valid, sizeof(ids));
    assert(topology());
    for (unsigned int i = 0; i < 8; i++) {
        assert(pb_index(valid[i]) == (valid[i] >> 8) * 4 + (valid[i] & 0xff));
        const u64 invalid[] = {4, 0x10, 0x110, 0x200, 0x10000, 1ULL << 32};
        for (unsigned int j = 0; j < sizeof(invalid) / sizeof(*invalid); j++) {
            ids[i] = invalid[j];
            assert(!topology());
        }
        ids[i] = valid[(i + 1) % 8];
        assert(!topology()); /* duplicated slot, missing original */
        ids[i] = valid[i];
    }
    for (n = 1; n < 8; n++) assert(!topology());
    n = 4;
    for (unsigned int i = 0; i < n; i++) ids[i] = i;
    assert(!topology()); /* MSM8939 cannot silently become a four-core system */
    msm8939 = false;
    assert(topology()); /* shared four-core path */
    ids[3] = 0x100;
    assert(!topology());
}
"""
with tempfile.TemporaryDirectory(prefix="pb-topology-") as directory:
    c = Path(directory) / "topology.c"
    binary = Path(directory) / "topology"
    c.write_text(harness)
    subprocess.run([os.environ.get("CC", "cc"), "-std=c11", "-Wall",
                    "-Werror", str(c), "-o", str(binary)], check=True)
    subprocess.run([str(binary)], check=True)
print("kernel patch topology checks: PASS")
