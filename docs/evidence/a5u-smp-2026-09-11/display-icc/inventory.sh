set -eu
cat /proc/cmdline
cat /sys/devices/system/cpu/online
for p in /sys/class/drm/card*-DSI-*/status /sys/class/drm/card*-DSI-*/enabled; do printf '%s=' "$p";cat "$p";done
cat /sys/kernel/debug/interconnect/interconnect_summary
