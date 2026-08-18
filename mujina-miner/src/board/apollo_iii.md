# Apollo III Board Support

This document describes mujina-miner's support for the FutureBit Apollo
III: the 21-chip Auradine Aura ASIC chain, the board's power/thermal
envelope, and how to deploy it. For the Aura chip protocol itself, see
the [Aura chip reference](../asic/aura/REFERENCE.md); for the image that
ships this board support, see
[`tools/apollo-iii-image/README.md`](../../../tools/apollo-iii-image/README.md).

## Overview

The Apollo III is a Radxa ROCK 5B+ (RK3588) single-board miner with 21
Auradine "Aura" (Treasure-family) ASICs on a direct SoC UART — there is
**no on-board MCU** between the SoC and the ASIC chain. Mujina drives the
chain over `/dev/ttyS4`, controls the rail and fan through sysfs GPIO/PWM,
and reads board/PSU telemetry from the SIC450 PMBus controller on i2c-3.

The board is a `VirtualBoardDescriptor` (`device_type: "apollo_iii"`)
enabled by environment, exactly like the CPU board — no USB hotplug, no
discovery events. The daemon injects an `ApolloDeviceConnected` transport
event at startup when the env config is present.

## Hardware Components

| Component | Path / address | Role |
|---|---|---|
| ASIC chain (21× Aura) | `/dev/ttyS4`, 115200 → 921600 8-N-1 | hashing; discovery at 115200, mining at 921600 |
| ASIC reset | GPIO 115 (sysfs) | `0 → 1` pulse at bring-up, asserted low on shutdown |
| Heartbeat clock | GPIO 148 (sysfs) | held high at bring-up |
| ASIC rail power | GPIO 100 (sysfs), `active_low=0`, `1` = ON | held high once the chain is live; dipped ~100 ms every 1–4 s as the power-MCU watchdog kick |
| Thermal trip | GPIO 138 (sysfs, input) | sustained high → emergency stop (debounced over 3 monitor ticks) |
| Fan duty | `pwmchip0/pwm0` (`fd8b0010.pwm`), period 40000 ns | PI-controlled against board temp; safe 40 % duty before ASIC work |
| Fan tach | `gpiochip0` line 14 (chardev), PPR=2 | startup RPM check (0 RPM refuses ASIC work) + RPM telemetry |
| PSU voltage | `pwmchip1/pwm0` (`febf0000.pwm`), period 40000 ns | duty 20000 ≈ 5.0 V → 36000 ≈ 6.1 V; ramped with the frequency ramp |
| SIC450 PMBus / board temp | i2c-3 @ 0x49 | board temp (reg 0x00, 1-byte read, LM75-style 0.5 °C/LSB decode); full SIC450 telemetry is a follow-on |

The vendor's own blob self-reported "fan = pwmchip1"; live board evidence
(CERTAINTY A19e, boot-contract §5) shows fan = `pwmchip0`, PSU =
`pwmchip1`.

## Configuration

The board is enabled by the **presence** of `MUJINA_APOLLO_SERIAL` (an
empty value falls back to `/dev/ttyS4`). All variables are registered in
`env_help.rs` and shown by `mujina-minerd --help`.

| Variable | Default | Meaning |
|---|---|---|
| `MUJINA_APOLLO_SERIAL` | `/dev/ttyS4` | serial device for the Aura ASIC link; presence enables the board |
| `MUJINA_APOLLO_BAUD_INIT` | `115200` | discovery baud (8-N-1) |
| `MUJINA_APOLLO_BAUD_MINING` | `921600` | baud switched to after discovery |
| `MUJINA_APOLLO_MODE` | `12.1` | hashrate target in TH/s; the DVFS ramp stops at the PLL N for this target (12.1 TH/s = PLL 491 = full rate) |
| `MUJINA_APOLLO_EXPECTED_CHIPS` | `21` | expected chain size; discovery stops early once this many chips have ACKed |

Pool config uses the daemon-wide variables (`MUJINA_POOL_URL`,
`MUJINA_POOL_USER`, `MUJINA_POOL_PASS`); on the flasheable image these
live in `/usr/local/etc/mujina/apollo-iii.env`. `MUJINA_USB_DISABLE=1`
is recommended on this platform (no USB boards).

## Bring-up Sequence

The board's `create_apollo_board()` follows the vendor-proven order
(boot-contract §5 — only the two listed GPIOs are touched before
discovery):

1. Open `/dev/ttyS4` at the discovery baud.
2. GPIO 148 high (heartbeat clock), GPIO 115 reset pulse `0 → 1`
   (650 ms hold) — nothing else touches hardware.
3. Fan: export `pwmchip0/pwm0`, set period, 40 % safe duty, enable;
   sample the tachometer for 1 s and refuse ASIC work on 0 RPM.
4. Export `pwmchip1/pwm0` and set its period (not enabled, not driven —
   the PSU is not touched before discovery).
5. Hand the chain to the Aura thread. Discovery itself is lazy (first job
   assignment): multi-pass probabilistic sweep (see the Aura reference),
   then the post-discovery hook flips the monitor's `chain_ready` latch
   and switches the serial link to 921600.
6. After the chain is live: PSU held at baseline 5.0 V duty; the monitor
   starts the gpio100 watchdog dips, the DVFS heartbeat (~2.1 s) and the
   fan PI loop.
7. On the first pool job the frequency ramp starts (PLL N 80 → 491 in
   +20 steps, duty + HASHCONFIG rewritten per step) and the PSU duty
   climbs 5.0 → 6.1 V with it. **Ramping before work = zero hitrate** —
   the ramp is gated on live work.

Shutdown (SIGTERM path) mirrors the vendor's graceful stop: signal the
thread, assert gpio115 low (ASIC reset), drop gpio100 (rail off), stop
the fan.

## Monitoring

The board monitor (2 s tick) publishes `BoardTelemetry`: fan RPM + duty,
board temperature (SIC450 reg 0x00, LM75 decode), PSU voltage (from the
commanded duty readback), and the Aura chain hashrate/activity. The
thermal-trip input is debounced across 3 consecutive ticks before an
emergency stop.

## Safety Notes

- **Never `kill -9` the miner.** SIGTERM is the graceful shutdown path;
  only a reboot clears a wedged chain.
- **Reboot between board-state experiments** (project plan §8 / the
  apollo-oss-miner RUNBOOK discipline); keep the vendor image for
  recovery as the board-health gate.
- The fan startup RPM check, thermal-trip stop, and the watchdog-kicked
  rail are the on-board safety envelope; keep the fan seated and the
  thermal path clear.

## Deployment Reference

- Flashing guide + image recipe: `tools/apollo-iii-image/README.md`
- Image boot contract (what the image does/doesn't carry):
  `../../../docs/apollo-iii-boot-contract.md`
- Aura chip protocol: `mujina-miner/src/asic/aura/REFERENCE.md`

## Open Items (G6)

- PSU duty↔voltage curve linearity between the 5.0 V / 6.1 V anchors.
- Board-temp scaling (LM75 0.5 °C/LSB assumption).
- Thermal-trip polarity on the live board.
- Fan PI constants (plant response not yet characterized).
- gpio100 boot state (value before the monitor takes over).
