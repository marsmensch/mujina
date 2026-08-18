# Apollo III — flasheable mujina image

This directory builds and documents a **miner-only microSD image** for the
FutureBit Apollo III (Radxa ROCK 5B+ / RK3588, 21 Auradine Aura ASICs).
The image is Armbian-based (the same base family as the vendor's
`armbi_root`, see [`docs/apollo-iii-boot-contract.md`](../../docs/apollo-iii-boot-contract.md))
and carries exactly:

- `mujina-minerd` (aarch64) — the mujina daemon driving the Aura chain
- `apollo-iii-mujina.service` — one systemd unit, SIGTERM-graceful stop
- `apollo-iii-exports.service` + `/usr/local/sbin/apollo-iii-exports.sh` —
  boot-time GPIO/PWM exports
- `/usr/local/etc/mujina/apollo-iii.env` — pool + board config (user-editable)

**No bitcoin node, no ckpool, no apolloapi/UI.** The miner connects
directly to a public pool with mujina's own Stratum v1 client. This is
the project's end state: flash → boot → mine.

- Board support docs: [`mujina-miner/src/board/apollo_iii.md`](../../mujina-miner/src/board/apollo_iii.md)
- ASIC reference: [`mujina-miner/src/asic/aura/REFERENCE.md`](../../mujina-miner/src/asic/aura/REFERENCE.md)
- Image contract: [`docs/apollo-iii-boot-contract.md`](../../docs/apollo-iii-boot-contract.md)

## Layout

```
tools/apollo-iii-image/
├── build-image.sh          # reproducible Armbian build recipe (heavy)
├── overlay/                # rootfs additions, mirrors the target filesystem
│   ├── etc/systemd/system/apollo-iii-mujina.service
│   ├── etc/systemd/system/apollo-iii-exports.service
│   ├── usr/local/sbin/apollo-iii-exports.sh
│   └── usr/local/etc/mujina/apollo-iii.env
└── out/                    # build output (created by build-image.sh)
```

## Prerequisites

- An Apollo III and a microSD card (16 GB or larger; the image is ~4 GB).
- A host with **Docker or Podman** (Apple Silicon Macs run the arm64
  build natively; x86_64 hosts need qemu user emulation for the Rust
  container step).
  - Podman on macOS: `podman machine start` (the project standard is
    podman 6 via Homebrew).
- ~30 GB free disk and a few hours of build time for the Armbian build.
- A USB serial adapter + console client for first boot (Armbian's
  RK3588 console is 1500000 8-N-1 by default; check `/etc/armbianEnv.txt`
  `console=` if you change it).

## Build

```bash
tools/apollo-iii-image/build-image.sh
```

What it does (see the script header for the full sequence and pins):

1. Builds `mujina-minerd` for `aarch64-unknown-linux-gnu` in
   `rust:1.94-bookworm` (`--locked`, so `Cargo.lock` pins the deps).
2. Clones the Armbian build framework at a **pinned commit**
   (`ARMBIAN_COMMIT`, currently `2db577f8` = `v26.11.0-trunk.11`) and
   refuses to build on a mismatch.
3. Stages `overlay/` + the binary into Armbian's `userpatches/overlay`
   and enables the units via `customize-image.sh`.
4. Runs `./compile.sh docker` with `BOARD=rock-5b-plus BRANCH=current
   RELEASE=bookworm BUILD_MINIMAL=yes BUILD_DESKTOP=no`.

Overridable via environment: `BOARD`, `BRANCH`, `RELEASE`,
`ARMBIAN_TAG`/`ARMBIAN_COMMIT`, `RUST_IMAGE`, `CARGO_TARGET`, `OUT_DIR`.

Result: `out/Armbian_<…>_rock-5b-plus_<…>.img` + `.sha`.

## Flash to microSD

**Identify the target device first.** A wrong `of=` destroys the wrong
disk. On macOS, `diskutil list` and unmount the card
(`diskutil unmountDisk /dev/diskN`) before writing; use the raw device
(`/dev/rdiskN`) for speed. On Linux, `lsblk` and confirm the card's
model/size; the card must not be mounted.

```bash
# macOS (raw device is faster; adjust N to your card)
sudo dd if=out/Armbian_*.img of=/dev/rdiskN bs=4M status=progress conv=fsync

# Linux
sudo dd if=out/Armbian_*.img of=/dev/sdX bs=4M status=progress conv=fsync
sync
```

Verify the write: `sha256sum out/Armbian_*.img` against the `.sha` file,
then read back the card's first bytes (`sudo dd if=/dev/rdiskN bs=4M
count=4 | sha256sum`) — it will not match the whole-image hash, but the
card must boot; a spot check of the GPT header is a good smoke test.

Insert the card, connect power + network, and boot.

## First-boot checklist

1. **Console/SSH:** Armbian's first-run wizard asks for a root password
   (serial console 1500000 8-N-1, or connect on the LAN once the DHCP
   lease appears). The miner unit starts regardless.
2. **Configure the pool:** edit `/usr/local/etc/mujina/apollo-iii.env`
   and set `MUJINA_POOL_USER` to
   `<your-bitcoin-address>.<worker-name>` (the shipped value is a
   placeholder). Keep `MUJINA_POOL_URL` on a pool you can accept shares
   from, then `systemctl restart apollo-iii-mujina`.
3. **Unit state:** `systemctl status apollo-iii-mujina
   apollo-iii-exports`. The exports unit may legitimately show failed
   (see below) — the miner still runs.
4. **Logs:** `journalctl -u apollo-iii-mujina -f`. Watch for
   discovery ACKs (`Aura chip ACKed discovery`), the ramp reaching full
   rate, and `Fan startup RPM check passed` (a dead fan refuses ASIC
   work).
5. **Hashrate / shares:** `mujina-cli` or the REST API
   (`curl http://127.0.0.1:7785/api/v0/…`, loopback by default). The
   board telemetry publishes per-chip hashrate, board temp, PSU voltage
   and fan RPM.
6. **Acceptance:** first accepted share on the pool, then let it climb
   to the `MUJINA_APOLLO_MODE` target (12.1 TH/s = full rate, PLL 491).
   Lower `MUJINA_APOLLO_MODE` for a cooler/quieter run; the ramp simply
   stops early.

## Safety notes

- **Never `kill -9` the miner.** SIGTERM is the graceful path (the
  daemon resets the ASIC, drops the rail, stops the fan — mirroring the
  vendor's own stop). The unit's `ExecStop` sends SIGTERM and waits up
  to 60 s; systemd escalates to SIGKILL only after `TimeoutStopSec`.
  A wedged chain is cleared by **reboot only**.
- **Reboot between board-state experiments.** One clean reboot →
  vendor-to-full-rate → graceful stop → settle is the lab discipline for
  any bring-up work (see the project plan §8 / apollo-oss-miner RUNBOOK).
- **Vendor recovery is the board-health gate.** Keep the stock Apollo
  image handy; if the flashed image misbehaves, restore the vendor image
  and confirm the board reaches full rate before blaming hardware.
- **Thermal:** the board refuses ASIC work if the fan tachometer reads
  0 RPM at startup. The fan PI loop and thermal-trip input (gpio138)
  are the on-board safety envelope; keep the fan connector seated.
- **The REST API has no authentication.** Leave `MUJINA_API_LISTEN` on
  loopback unless you need LAN access and accept the exposure.

## Troubleshooting

| Symptom | Likely cause / action |
|---|---|
| `systemctl status apollo-iii-exports` failed | DTB/kernel missing a GPIO/PWM controller. The miner re-exports at runtime and reports the real error; check `journalctl -u apollo-iii-mujina`. If the sysfs path is genuinely absent, rebuild with `BRANCH=vendor` (Radxa BSP kernel) or patch the DTS. |
| No `Aura chip ACKed discovery` lines | Serial link or bring-up problem. Check `/dev/ttyS4` exists and is free (`fuser /dev/ttyS4`), and gpio148/gpio115 exported (`/sys/class/gpio`). |
| Miner restarts every ~10 s | Unit failure loop; after 5 failures in 10 min the unit goes `failed` (deliberate — a wedged board needs a reboot, not restarts). Reboot, then read `journalctl -u apollo-iii-mujina -b -1`. |
| No shares accepted | Wrong `MUJINA_POOL_USER` format or pool URL; shares are also rejected if the board is below target while ramping — wait for the ramp. |

## Open questions (need the real build or device — G6)

- **DTB/PWM exposure:** whether the Armbian `rock-5b-plus` mainline DTB
  exposes both PWM controllers (`fd8b0010.pwm` as pwmchip0, `febf0000.pwm`
  as pwmchip1) with the same numbering as the vendor board. The exports
  script resolves chips by platform device name, but if a controller is
  disabled in the DTS the build needs `BRANCH=vendor` or a DTS patch.
- **Console baud:** Armbian RK3588 defaults to 1500000; confirm the
  vendor's service console is the same before relying on it.
- **First-boot UX:** the image ships Armbian's first-run password
  wizard; a future iteration may pre-seed credentials via
  `userpatches/` (out of scope for the initial image).
- **Fan plant response / PSU duty↔voltage curve linearity:** on-device
  characterization is G6 work (see the board guide's open items).
