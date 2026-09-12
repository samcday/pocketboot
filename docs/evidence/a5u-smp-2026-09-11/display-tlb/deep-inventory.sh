set -eu
cat /proc/cmdline
cat /sys/devices/system/cpu/online
cat /proc/interrupts
for p in /sys/kernel/debug/dri/0/fb /sys/kernel/debug/dri/0/state; do printf '\nFILE %s\n' "$p";cat "$p";done
