set -eu
cat /proc/cmdline
cat /proc/uptime
cat /sys/devices/system/cpu/online
/tmp/pb-smp-v2/work
/tmp/pb-coherency-v1/work
cat /proc/interrupts
mkdir -p /tmp/pb-pstore
mount -t pstore pstore /tmp/pb-pstore
ls -l /tmp/pb-pstore
