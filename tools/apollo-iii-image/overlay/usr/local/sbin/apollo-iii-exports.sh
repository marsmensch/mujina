#!/bin/sh
# Apollo III boot-time GPIO/PWM exports.
#
# Exports the sysfs interfaces the board needs before the miner starts:
#   GPIO 148 (heartbeat clock, out)  - raised high at bring-up
#   GPIO 115 (ASIC reset, out)       - pulsed 0 -> 1 at bring-up
#   GPIO 100 (ASIC rail power, out)  - active_low=0, value 1 = ON
#   GPIO 138 (thermal trip, in)      - fault -> emergency stop
#   pwmchipN/pwm0 (fan,  pwmchip0)   - fd8b0010.pwm on the vendor board
#   pwmchipN/pwm0 (PSU,  pwmchip1)   - febf0000.pwm on the vendor board
#
# Idempotent: safe to run repeatedly (systemctl restart apollo-iii-exports,
# or at every boot). Already-exported lines are skipped, not errors.
#
# This is a best-effort precondition: mujina-minerd re-exports and drives
# everything itself at runtime (boot-contract §6), so a failure here is a
# health signal (wrong DTB/kernel for the board) but does not stop the
# miner — the unit is Wants=, not Requires=.
#
# The PWM chip indices are looked up by platform device name
# (fd8b0010.pwm = fan, febf0000.pwm = PSU) so the script survives kernel
# renumbering; the vendor board exposes them as pwmchip0/pwmchip1.
set -eu

GPIO_BASE=/sys/class/gpio
PWM_BASE=/sys/class/pwm

# --- GPIO -----------------------------------------------------------------

gpio_export() {
    # $1 = line number, $2 = direction (in|out), $3 = active_low (0|1, optional)
    line="$1"
    dir="$2"
    active_low="${3:-}"
    node="$GPIO_BASE/gpio$line"
    if [ ! -e "$node" ]; then
        # EBUSY (already exported by the kernel or a previous run) is fine.
        echo "$line" >"$GPIO_BASE/export" 2>/dev/null || true
    fi
    [ -e "$node" ] || { echo "apollo-iii-exports: GPIO $line missing after export" >&2; return 1; }
    echo "$dir" >"$node/direction" 2>/dev/null || true
    if [ -n "$active_low" ] && [ -e "$node/active_low" ]; then
        echo "$active_low" >"$node/active_low"
    fi
}

gpio_export 148 out
gpio_export 115 out
# Rail power: active_low=0 (value 1 = ON). The board code holds it high and
# dips it ~100 ms every 1-4 s as the power-MCU watchdog kick once the chain
# is live; boot-time export only, value is left alone here.
gpio_export 100 out 0
# Thermal trip: input; fault (high) escalates to an emergency stop.
gpio_export 138 in

# --- PWM -------------------------------------------------------------------

# Resolve a PWM chip index from its platform device name
# (e.g. "febf0000.pwm"). Exits non-zero if no chip matches.
pwm_chip_for() {
    want="$1"
    for d in "$PWM_BASE"/pwmchip*; do
        [ -d "$d" ] || continue
        if [ "$(basename "$(readlink -f "$d/device")")" = "$want" ]; then
            echo "${d##*/pwmchip}"
            return 0
        fi
    done
    echo "apollo-iii-exports: no PWM chip for platform device $want" >&2
    return 1
}

# Export channel 0 on a chip; already-exported channels are skipped.
pwm_export_channel0() {
    chip="$1"
    ch="$PWM_BASE/pwmchip$chip/pwm0"
    if [ ! -e "$ch" ]; then
        echo 0 >"$PWM_BASE/pwmchip$chip/export" 2>/dev/null || true
    fi
    if [ ! -d "$ch" ]; then
        echo "apollo-iii-exports: failed to export pwmchip$chip channel 0" >&2
        return 1
    fi
}

FAN_CHIP="$(pwm_chip_for fd8b0010.pwm)" || FAN_CHIP="0"
PSU_CHIP="$(pwm_chip_for febf0000.pwm)" || PSU_CHIP="1"
pwm_export_channel0 "$FAN_CHIP"
pwm_export_channel0 "$PSU_CHIP"

echo "apollo-iii-exports: GPIO 148/115/100/138, pwm chip $FAN_CHIP (fan) and $PSU_CHIP (PSU) ready"
