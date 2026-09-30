#!/bin/busybox sh
# Lab-only /init overlay. Never source this script or run it in a PID namespace.
# PocketBoot remains PID1; the diagnostic children do not own its lifetime.
if [ "$$" -ne 1 ]; then
    printf '%s\n' 'TBX304F-PROBE: refusing to run: this wrapper must be /init (PID1)' >&2
    exit 1
fi

log()
{
    printf '<6>TBX304F-PROBE: %s\n' "$*" > /dev/kmsg
}

bounded_lines()
{
    # At most 4096 bytes, 32 lines, 256 characters per line. HEAD -c needs a
    # BusyBox option absent from PocketBoot's minimal config; DD does not.
    /bin/busybox dd bs=4096 count=1 2>/dev/null |
        /bin/busybox head -n 32 |
        /bin/busybox cut -c 1-256 |
        while IFS= read -r line || [ -n "$line" ]; do
            log "$line"
        done
}

snapshot_file()
{
    if [ -r "$1" ]; then
        log "file=$1 (bounded)"
        bounded_lines < "$1"
    else
        log "file=$1 unavailable"
    fi
}

snapshot()
{
    log "snapshot nominal-boot-seconds=$1"
    snapshot_file /sys/kernel/debug/devices_deferred
    snapshot_file /proc/partitions
    # ChipIdea's device entry reports software state, not raw registers.
    snapshot_file /sys/kernel/debug/usb/ci_hdrc.0/device
    snapshot_file /sys/class/usb_role/ci_hdrc.0-role-switch/role
    snapshot_file /sys/kernel/debug/pm_genpd/pm_genpd_summary
    for platform_device in remoteproc:smd-edge:rpm-requests:power-controller \
        7824900.mmc 7864900.mmc; do
        driver_link=/sys/bus/platform/devices/$platform_device/driver
        if [ -L "$driver_link" ]; then
            log "driver-link=$driver_link"
            /bin/busybox readlink "$driver_link" 2>/dev/null | bounded_lines
        else
            log "driver-link=$driver_link unbound"
        fi
    done

    udc_count=0
    for udc in /sys/class/udc/*; do
        [ -d "$udc" ] || continue
        udc_count=$((udc_count + 1))
        if [ "$udc_count" -gt 4 ]; then
            log 'UDC list capped at 4'
            break
        fi
        for attribute in state function current_speed maximum_speed; do
            snapshot_file "$udc/$attribute"
        done
        if [ -L "$udc/device/driver" ]; then
            log "driver-link=$udc/device/driver"
            /bin/busybox readlink "$udc/device/driver" 2>/dev/null | bounded_lines
        else
            log "driver-link=$udc/device/driver unavailable"
        fi
    done
    [ "$udc_count" -ne 0 ] || log 'no UDCs'

    gadget_count=0
    for gadget in /sys/kernel/config/usb_gadget/*; do
        [ -d "$gadget" ] || continue
        gadget_count=$((gadget_count + 1))
        if [ "$gadget_count" -gt 4 ]; then
            log 'gadget list capped at 4'
            break
        fi
        snapshot_file "$gadget/UDC"
    done
    [ "$gadget_count" -ne 0 ] || log 'no configfs USB gadgets'
}

request_device_role()
{
    probe_role=/sys/class/usb_role/ci_hdrc.0-role-switch
    probe_udc=/sys/class/udc/ci_hdrc.0
    if [ ! -w "$probe_role/role" ] || [ ! -d "$probe_udc/device" ]; then
        log 'optional device-role request unavailable'
        return
    fi
    probe_role_device=$(cd -P "$probe_role/device" && pwd -P) || return
    probe_udc_device=$(cd -P "$probe_udc/device" && pwd -P) || return
    if [ "$probe_role_device" != "$probe_udc_device" ]; then
        log 'refusing device-role request: role switch and UDC parents differ'
        return
    fi
    log 'optional device-role request via the matched ci_hdrc.0 role-switch API'
    if printf '%s\n' device > "$probe_role/role"; then
        log 'device-role request accepted'
    else
        log 'device-role request failed'
    fi
}

reprobe_emmc()
{
    probe_rpmpd=/sys/bus/platform/devices/remoteproc:smd-edge:rpm-requests:power-controller/driver
    probe_emmc=/sys/bus/platform/devices/7824900.mmc
    if [ ! -d "$probe_rpmpd" ] || [ ! -d "$probe_emmc" ] ||
        [ ! -w /sys/bus/platform/drivers_probe ]; then
        log 'optional eMMC re-probe unavailable: provider or device missing'
        return
    fi
    probe_provider=$(cd -P "$probe_rpmpd" && pwd -P) || return
    if [ "$probe_provider" != /sys/bus/platform/drivers/qcom-rpmpd ]; then
        log 'refusing eMMC re-probe: unexpected power-domain driver'
        return
    fi
    if [ -L "$probe_emmc/driver" ]; then
        log 'eMMC already bound; not re-probing or unbinding'
        return
    fi
    log 'optional one-time eMMC re-probe after RPMPD is bound'
    if printf '%s\n' 7824900.mmc > /sys/bus/platform/drivers_probe; then
        log 'eMMC re-probe request accepted'
    else
        log 'eMMC re-probe request failed'
    fi
}

deadline()
{
    # Independent of snapshot reads and the optional debugfs mount. This can
    # recover a live kernel without USB, not a hung kernel.
    /bin/busybox sleep 30 || return
    if [ -e /run/tbx304f-probe.keep ]; then
        log 'diagnostic deadline cancelled: /run/tbx304f-probe.keep exists'
        return
    fi
    log 'intentional diagnostic panic: deadline expired without cancellation'
    printf c > /proc/sysrq-trigger
}

probe()
{
    # Give the real init time to mount proc, sysfs, devtmpfs and /run.
    /bin/busybox sleep 3 || return
    [ -r /sys/firmware/devicetree/base/compatible ] || return
    /bin/busybox tr '\000' '\n' < /sys/firmware/devicetree/base/compatible |
        /bin/busybox grep -F -x -q 'lenovo,tbx304x' || return
    [ -r /proc/cmdline ] || return
    IFS= read -r cmdline < /proc/cmdline || return
    # Match whole tokens, with the last panic= value winning like the kernel.
    # Ignore init arguments after -- and disable glob expansion while splitting.
    set -f
    enabled=0
    role_device=0
    reprobe_mmc=0
    panic=
    for token in $cmdline; do
        case "$token" in
            --) break ;;
            pocketboot.probe) enabled=1 ;;
            pocketboot.probe-role-device) role_device=1 ;;
            pocketboot.probe-reprobe-mmc) reprobe_mmc=1 ;;
            panic=*) panic=${token#panic=} ;;
        esac
    done
    set +f
    [ "$enabled" -eq 1 ] && [ "$panic" = -1 ] || return

    # Nothing is logged, mounted or armed until ALL three guards pass.
    deadline &
    log 'armed: diagnostic panic in 30s; cancel with /run/tbx304f-probe.keep'

    # Reuse PocketBoot's existing standard debugfs mount, without remounting.
    # Only mount if absent; never write a debugfs file or a device register.
    if [ -d /sys/kernel/debug ] &&
        ! /bin/busybox mountpoint -q /sys/kernel/debug; then
        if /bin/busybox mount -t debugfs -o ro,nosuid,nodev,noexec \
            debugfs /sys/kernel/debug >/dev/null 2>&1; then
            log 'mounted diagnostic debugfs read-only'
        else
            log 'debugfs mount unavailable'
        fi
    fi
    snapshot 3
    [ "$role_device" -eq 0 ] || request_device_role
    /bin/busybox sleep 7 || return
    snapshot 10
    [ "$reprobe_mmc" -eq 0 ] || reprobe_emmc
    /bin/busybox sleep 10 || return
    snapshot 20
}

probe &
exec /pocketboot.real-init "$@"
