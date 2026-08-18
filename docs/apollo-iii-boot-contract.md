# Apollo III Boot Contract (from the vendor OS image)

> **Source:** `apollo-3_080626.img.xz` (Apollo 3 firmware image, 2026-08-06
> build) + live-board evidence collected 2026-08-08 → 2026-08-17.
> **Extraction method:** xz → raw GPT disk image → single ext4 partition
> (`primary`, LBA 32768..end, volume `armbi_root`) read via `debugfs`
> (e2fsprogs in a podman alpine container). No device access needed for
> anything in this document.
> **Why it exists:** mujina runs on the stock Apollo OS, which boots a
> specific environment (services, exports, permissions). Mujina must know
> exactly what it inherits and what it must re-establish itself.

---

## 1. OS / image facts

- **Distribution:** Armbian-based (ext4 volume name `armbi_root`); Radxa
  ROCK 5B+ (RK3588, aarch64); `BOARD_NAME="Apollo 3"` from
  `/etc/armbian-release`.
- **Disk layout:** GPT, one partition `primary` (Linux fs GUID), LBA 32768 →
  end of image (~9.4 GB). Backup GPT header absent — the vendor ships a
  truncated image (partition fills the file). Don't use tools that demand a
  valid backup GPT (`parted` fails; `fdisk`/`debugfs` work).
- **App tree:** `/opt/apolloapi` (git repo present, public upstream
  `apolloapi-v2`) — web UI (`apolloui-v2`, Next.js) + backend scripts +
  binaries (`backend/`).
- **Boot-time board config:** `/etc/rc.local` → firewall, `set_UI_mode.sh`
  (sets `NEXT_PUBLIC_CHASSIS=apollo-iii` in the UI `.env`), `first_run`
  (NVMe format/mount, swap, `/home/futurebit/.bitcoin` symlink).

## 2. systemd boot stack (from the image)

| Unit | User | ExecStart | Notes |
|---|---|---|---|
| `apollo-miner.service` | root | `backend/apollo-miner/miner_start.sh` | `Type=forking`; `Restart=always`, `RestartSec=30`; `ExecStop=miner_stop.sh` |
| `ckpool.service` | futurebit | `backend/ckpool/ckpool_start.sh` | `After=node.service`; `TimeoutStopSec=300` |
| `node.service` | futurebit | `backend/node/node_start.sh` | bitcoind; `After=apollo-ui-v2.service` + `dev-nvme0n1.device` |
| `apollo-api.service` / `apollo-ui-v2.service` | futurebit | backend API + Next.js UI | |
| `rc-local.service` | root | `/etc/rc.local` | see §1 |
| `futurebit-rtw89-init.service` | root | WiFi firmware loader | not mining-relevant |

- **No udev rules for `/dev/ttyS4`** — the miner runs as root and opens it
  directly. Mujina's unit must also run as root (or own a udev rule).
- No mining-related scripts in `/etc/init.d`.

## 3. Miner start/stop contract (what mujina replaces)

**Start** (`miner_start.sh`, Apollo 3 case):
```
screen -dmS miner ./futurebit-miner-v3 $settings3
```
- `$settings3` comes from a runtime-generated file `miner_config3` in the
  miner's working dir (written by the web UI / apollo-api: mode preset,
  pool, etc.).
- **Apollo 3 does NOT run `apollo-helper` board detection at start** — that
  handshake (`RD6` / `Apollo-BTC` / `Apollo-2`) is only for external USB
  boards (`/dev/ttyACM*`) and the old internal `/dev/ttyS1` boards.

**Stop** (`miner_stop.sh`, Apollo 3 case):
```
pkill -TERM -f '^(\./|/opt/apolloapi/backend/apollo-miner/)futurebit-miner-v3( |$)'
# wait up to 60 s for exit; no GPIO reset on Apollo 3
```
- The blob performs its own hardware shutdown on SIGTERM. No screen-quit
  (SIGHUP), no gpio re-reset for the internal Apollo 3 chain.

**Status surface:**
- Blob writes `apollo-miner-3.json` in its working dir — schema
  `statVersion 1.3`: `master` (uptime, diff, boards, intervals with
  bySol/byDiff/byPool/byJobs, solutions, errors, chipSpeed), `pool`
  (host/port/user, sharesSent/Accepted/Rejected), `fans` (`{"0": {"rpm":
  [2300]}}`), `temperature` (count=21, min/avr/max), `slots` (per-slot:
  spiNum, btcNum=21, chips=21, pwrOn, currents, ghs, wattPerGHs, osc),
  `slaves`.
- Blob also writes fan state under `/tmp/fan/` (`speed_psu_overcurrent`,
  `fan_failure`, …) — read by the web UI.

## 4. Pool contract (ckpool)

`backend/ckpool/ckpool.conf` (verbatim semantics):
```json
{
  "btcd":     [{ "url": "127.0.0.1:8332", "auth": "futurebit", "pass": "", "notify": true }],
  "logdir":   "/opt/apolloapi/backend/ckpool/logs",
  "btcsig":   "/FutureBit-mined by Solo Apollo/",
  "zmqblock": "tcp://127.0.0.1:28332",
  "startdiff": 1024,
  "mindiff":  1
}
```
- ckpool is a **solo pool against the local bitcoind** and serves Stratum v1
  on its default port **3333** (`127.0.0.1:3333`).
- The miner connects to a stratum endpoint — the UI's default snapshot
  config points at `stratum.braiins.com:3333` (public pool); the OSS miner
  uses `127.0.0.1:3333`. Mujina's Stratum v1 client is pool-agnostic: both
  work.

## 5. Hardware control facts (live-board verified, 2026-08-17)

| Path | Purpose | Values / notes |
|---|---|---|
| `/sys/class/pwm/pwmchip1/pwm0` (`febf0000.pwm`) | **PSU/SIC450 voltage** | period **40000**; duty 20000≈5.0 V → 36000≈6.1 V; ramp 20000→21600→…→36000 |
| `/sys/class/pwm/pwmchip0/pwm0` (`fd8b0010.pwm`, npwm=1) | **Fan duty** | PID-controlled in the DVFS loop; duty↔RPM response not yet characterized |
| gpiochip0 line 14 (chardev) | Fan tach | PPR=2; startup RPM check (blob aborts ASIC work if tach is busy) |
| GPIO 148 (sysfs) | heartbeat clock line | high at bring-up (+0.00 s) |
| GPIO 115 (sysfs) | ASIC reset | 0→1 pulse (+0.65 s), then discovery sweep +0.88 s |
| GPIO 100 (sysfs) | ASIC rail power | `active_low=0`; value 1 = ON; watchdog dip ~100 ms every 1–4 s |
| GPIO 138 (sysfs) | thermal trip | input; fault → shutdown |
| `/dev/i2c-3` @ 0x49 | SIC450 PMBus + board temp | telemetry via `I2C_RDWR` (0x0707) combined, repeated START, **PEC off**; board temp = reg 0x00, 1-byte read; dual-rail telemetry (master/slave) |
| `/dev/ttyS4` | ASIC UART | 115200 8-N-1 discovery → 921600 8-N-1 mining; direct SoC↔ASIC, no MCU |
| `/sys/class/gpio/export`, `/sys/class/pwm/pwmchipN/export` | runtime exports | the blob exports GPIOs/PWM itself at bring-up (root) |

- Fan init order: "Fans initialized at safe startup duty" → startup RPM
  check → ASIC work. The fan PID runs inside the DVFS loop
  (`DVFS: fan %d PWM %d%% speed %d RPM`).
- The blob's `-debug` self-report "fan = pwmchip1/pwm0" is **stale/wrong** —
  live sysfs shows fan = pwmchip0, PSU = pwmchip1 (CERTAINTY A19e).

## 6. The contract for mujina

1. **Deploy as a drop-in for `apollo-miner.service` only.** Keep
   `ckpool.service`, `node.service`, `apollo-api`, `apollo-ui-v2` running.
2. **Unit semantics:** run as root; `Type=simple` (mujina daemonizes itself
   or not — no screen); `ExecStop` must SIGTERM + wait ≤60 s (mirror vendor
   graceful shutdown); `After=ckpool.service network.target` so stratum is
   up before the board brings up the chips (the ramp is gated on pool work).
3. **Boot inheritance:** GPIO bases, PWM controllers, I2C bus, and the
   nvme/node environment come from the boot image/DTB. Mujina must export
   and drive GPIO 148/115/100, PWM chips, and ttyS4 **itself** (root) —
   same as the blob.
4. **Fan:** write `pwmchip0/pwm0` duty; read tach `gpiochip0` line 14;
   startup RPM check + safe duty before ASIC work; closed-loop PID against
   chip temp (vendor does this inside the DVFS loop).
5. **PSU:** voltage via `pwmchip1/pwm0` duty (NOT PMBus `VOUT_COMMAND`);
   telemetry (Vout/Iout/temp per rail) via PMBus reads at 0x49; board temp
   via reg 0x00 at 0x49.
6. **UI parity (optional but cheap):** write `apollo-miner-3.json`
   (statVersion 1.3 schema, §3) + `/tmp/fan/*` so the stock web UI keeps
   reading live state; mujina's own REST API remains the primary surface.
7. **Safety:** never `kill -9` (only reboot clears a wedge); reboot between
   board-state experiments; vendor-to-full-rate recovery as the board-health
   gate.

## 7. Provenance

- Image: `apollo-3_080626.img.xz` — 2,582.6 MiB compressed / 8,977.4 MiB
  raw (9,413,493,248 B), CRC64. SHA-256 of raw image:
  (compute at extraction time, see `apollo-iii-image-extract/`).
- Files read: `/etc/rc.local`, `/etc/systemd/system/{apollo-miner,
  ckpool,node,apollo-api,apollo-ui-v2,rc-local}.service`,
  `backend/apollo-miner/{miner_start.sh,miner_stop.sh,apollo-miner-3.json,
  futurebit-miner-v3,apollo-helper}`,
  `backend/ckpool/{ckpool.conf,ckpool_start.sh}`,
  `backend/utils/set_UI_mode.sh`, `backend/first_run`.
- Cross-checked against live-board evidence in `apollo-oss-miner`:
  `docs/CERTAINTY.md` (A7, A19, A19d, A19e), `docs/HANDOVER.md`,
  `docs/apollo-iii-aura-reverse-engineering.md`,
  `evidence/vendor_dvfs_boot.strace` (3078× i2c-3 accesses; 0x49 slave),
  and the vendor blob strings (`futurebit-miner-v3.vendor`).
- Extraction artifacts: `/Users/mars/code/mars-llm/apollo-iii-image-extract/`
  (raw image + `mined/` digests) — outside the apollo-oss-miner repo.
