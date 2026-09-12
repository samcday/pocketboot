set -eu
printf "SOAK_ITERATION_V1\n"
printf "uptime="; cat /proc/uptime
printf "clocksource="; cat /sys/devices/system/clocksource/clocksource0/current_clocksource
/tmp/pb-smp-v2/work
/tmp/pb-coherency-v1/work
printf "SOAK_ITERATION_PASS\n"
