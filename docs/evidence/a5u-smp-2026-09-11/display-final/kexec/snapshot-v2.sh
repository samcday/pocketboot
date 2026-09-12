set -eu
printf 'PB_SMP_SNAPSHOT_V2\n'
uname -a
printf 'boot_id='; if test -r /proc/sys/kernel/random/boot_id; then cat /proc/sys/kernel/random/boot_id; else printf 'unavailable\n'; fi
printf 'uptime='; cat /proc/uptime
printf 'cmdline='; cat /proc/cmdline
printf 'online='; cat /sys/devices/system/cpu/online
test "$(cat /sys/devices/system/cpu/online)" = '0-3'
for pb_cpu in /sys/firmware/devicetree/base/cpus/cpu@*; do
    printf 'cpu_node=%s reg=' "$pb_cpu"; hexdump -v -e '1/1 "%02x"' "$pb_cpu/reg"; printf '\n'
    printf 'method='; tr '\000' ' ' < "$pb_cpu/enable-method"; printf '\n'
    if test -r "$pb_cpu/cpu-release-addr"; then
        printf 'release='; hexdump -v -e '1/1 "%02x"' "$pb_cpu/cpu-release-addr"; printf '\n'
    fi
done
printf 'stat_before\n'; cat /proc/stat
/tmp/pb-smp-v2/work
printf 'stat_after\n'; cat /proc/stat
printf 'PB_SMP_SNAPSHOT_PASS\n'
