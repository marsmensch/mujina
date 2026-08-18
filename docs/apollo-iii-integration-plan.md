# Apollo III (Aura ASIC) Support — Mujina Integration Plan

> **For Hermes:** implement via delegated subagents (DeepSeek V4 Flash 0731 via
> delegate_task per session rule; Kimi K3 for critical analysis and code
> verification). One PR/commit per logical surface. Follow `CONTRIBUTING.md`,
> `CODE_STYLE.md`, `CODING_GUIDELINES.md` of this repo.

**Goal:** Let mujina-miner drive FutureBit Apollo III hardware — 21 Auradine
"Aura" (Treasure-family) ASICs — replacing the closed `futurebit-miner-v3`
blob, using the protocol ground truth already recovered in the
`apollo-oss-miner` project.

**Architecture:** Add a new ASIC family driver (`asic/aura/`) — the Aura wire
protocol is NOT BM13xx (different frame magic, CRC, register map) — plus a new
env-driven virtual board (`board/apollo_iii.rs`, mirroring `board/cpu.rs`) and
Linux-host hardware backends (sysfs GPIO, i2c-dev, sysfs PWM, gpiochip tach).
Reuse existing seams: `SerialStream` (runtime baud switch 115200→921600),
`HashThread` scheduler interface, stratum v1, REST API, `hw_trait` traits.

**Tech Stack:** Rust (tokio, tokio_serial/rustix, tokio-util codecs), inventory
board registry, Linux sysfs/chardev (gpio, i2c-dev, pwm), CRC-32 (IEEE 802.3).

---

## 1. Context — verified ground truth

Everything below was verified on a live Apollo III (Radxa ROCK 5B+ / RK3588,
SSH root access) during the `apollo-oss-miner` project (2026-08-08 → 2026-08-18).
Sources: vendor blob disasm (`futurebit-miner-v3`, unstripped Go), `-debug=2`
protocol traces, full-rate `strace` captures, and an Auradine datasheet
(2026-08-14). The RE writeups live in `/Users/mars/code/mars-llm/apollo-oss-miner`:
`docs/apollo-iii-aura-reverse-engineering.md`, `docs/CERTAINTY.md`,
`docs/MEASURED_DVFS_LAW.md`, `docs/PARITY_GOALS.md`, `evidence/` (captures),
`tools/aura_miner.py` (reference implementation), `tools/aura_proto.py`
(frame codec).

### 1.1 Hardware

| Item | Fact |
|---|---|
| Board | Radxa ROCK 5B+ (RK3588), Linux; miner runs as root |
| ASIC link | `/dev/ttyS4`, direct SoC↔ASIC UART, **no on-board MCU** (founder-confirmed) |
| ASICs | 21 Auradine "Aura" (Treasure-family); chip IDs decimal `0..10` + `128..137` (hex `0x00..0x0a` + `0x80..0x89`); max 25 TH/s/chain |
| Baud | discovery 115200 8-N-1 → mining 921600 8-N-1 (vendor strace: zero PARENB/PARODD) |
| PSU | Infineon SIC450; **voltage = PWM duty** on `pwmchip1/pwm0` (period 40000; duty 20000≈5.0 V, 36000≈6.1 V); PMBus telemetry live (I2C_RDWR combined 0x0707, repeated START, PEC off) |
| GPIO (sysfs) | 148 = heartbeat clock line; 115 = ASIC reset (0→1 pulse); 100 = ASIC rail power (active_low=0, value 1 = ON, watchdog dip ~100 ms every 1–4 s); 138 = thermal trip input; config straps 139/110/111/112/113/103 |
| Fan | tach on gpiochip0 line 14 (chardev ioctls, PPR=2); PWM duty control via sysfs (channel TBD — see Open Questions) |
| Temp sensor | i2c bus 3 @ 0x49 reg 0x00 (exact device vs SIC450 topology — see Open Questions) |
| Pool | local ckpool `127.0.0.1:3333`; vendor service `apollo-miner.service` |
| Modes | supereco 8.0 TH/s/55 °C, eco 12.0/59 °C, balanced 15.0/65 °C, turbo 18.0/69 °C, custom; ≈ +51 MHz per TH/s; SIC450 5.0–9.0 V / 450 W |
| Full rate | 12.1–12.23 TH/s sustained (N=491, 6.095 V, `state NORMAL`, hitrate 1.000) |

### 1.2 Wire protocol (Aura ≠ BM13xx)

- **Command frame (16 B):** `magic 78 56 34 12` (LE 0x12345678) | `chip(1)` |
  `reg(1)` | `len/cmd(2, LE)` | `data(4)` | `crc32(4)`. Each command is preceded
  by a **20-byte zero preamble** (separate write syscall → 36 B on the wire).
- **Response frame (16 B):** same layout, magic `54 76 c0 da` (LE 0x5476C0DA).
- **CRC:** CRC-32/IEEE-802.3, reflected poly `0xEDB88320`, init `0xFFFFFFFF`,
  xorout `0x00000000`, computed over the 12-byte prefix, stored LE.
- **Job frame (92 B, flat):** `magic 78563412 | chip | reg=0x84 |
  lcmd=slot*0x400+0x2d | 0x20 | prevhash(32) | merkle(32) | ntime(4 LE) |
  nbits(4 LE) | nonce_start(4) | crc32`. The ASIC computes the header hash from
  these fields — **no midstate template**.
- **Hit return:** command modifier `0x40`, per-chip, non-broadcast. Queued hit →
  **92-byte response**: `magic | chip | cmd+0x40 | nbits | id-hi+seq |
  complete 80-byte winning header | crc32 over bytes[0:88]`. **Nonce at bytes
  [84:88]** — read directly, no counter reconstruction. Empty FIFO → no response.
- **Key registers:** 0x00 CHIP_ID · 0x01 per-chip init (PLL freq, duty, version
  bound) · 0x02 telemetry (`lcmd 0x1200`) · 0x04/0x06 clock counters
  (`Fhash_MHz = 2·Δ0x06/Δ0x04·25`) · 0x10 VERSION_BOUND (per-chip window
  `0x0c30`: `lower=i*0x0c30, upper=lower+0x0c2f`, packed `lower|upper<<16`;
  inclusive, width multiple of 4, non-overlapping) · 0x11 VERSION_SHIFT = 13 ·
  0x13 HITCONFIG (bit0 = auto-report) · 0x14 HASHCONFIG (must be rewritten after
  0x68 or engines never see the duty change) · 0x18 PLL_CONFIG
  (`0x00502411`: pllen=1, bypass=0, postdiven=1, div1=1, div2=1, refdiv=5) ·
  0x19 PLL_FREQ (`N<<20`, 12-bit multiplier; `Fhash = 5·N/4 MHz`; N=491 →
  613.75 MHz, wire `0x1eb00000`) · 0x20 voltage ADC (`V = raw×0.0001011035
  −0.276029`) · 0x25 SMALL_NONCE · 0x60/0x61 stochastic hit counters
  (`GH/s = Δ0x61·2³²/Δt/1e9`; 0x60 preliminary, 0x61 authoritative; ratio
  Δ0x61/Δ0x60 = engine health) · 0x68 DUTY_CYCLE (`0x8080` ≤600 MHz, `0x8088`
  >600 MHz) · 0x81 DVFS (InitialSetup 18-write program; then `0x1f00` heartbeat
  `[0x02, 0x03, 0x01, 0x5074, 0x05]` every ~2.1 s) · 0x84 job write.
- **Bring-up (blob's exact order):** gpio148 high (+0.00 s) → gpio115 reset
  pulse (+0.65 s) → first discovery sweep +0.88 s. Only those two GPIOs before
  discovery — do NOT touch gpio100/139/138 or run an SIC450 I2C init first
  (disturbs a good board; caused the 0-chip discovery bug).
- **Discovery is multi-pass:** chips ACK the `reg=0x02` sweep probabilistically
  — a different subset each pass. Blast sweep, read once (~3.5 s drain),
  accumulate unique `"Aura"` ACKs across ~16–24 passes (blob paces ~10.8 s
  apart, re-strobes config frame between passes). Single pass never reaches 21.
- **DVFS InitialSetup (18 writes, `reg=0x81`, verbatim from full-rate strace):**
  `0x1800=11245000, 0x1900=0000000a, 0x2500=00f0ff00, 0x1400=02000200,
  0x6800=80800000, 0x1400=02000200, 0x1c00=01000000, 0x1d00=0d000000,
  0x2500=00f0ff00, 0x1400=02000200, 0x6800=80800000, 0x1400=02000200,
  0x1900=00000005, 0x2300=44080000, 0x6800=80800000, 0x1900=0000000a,
  0x2400=00000000, 0x2300=44080000` (register address = low byte of lcmd).
- **Ramp order (critical):** hold PSU at 5.0 V until a stratum job is on the
  chips → ramp PLL_FREQ N 80→491 in +20 steps (~50 MHz, ~50 ms each, DUTY_CYCLE
  + HASHCONFIG write per step) → climb SIC450 voltage 5.0→6.095 V as the
  closed-loop variable watching per-chip hitrate. Ramping before work = zero
  hitrate.

### 1.3 apollo-oss-miner readiness (the dependency)

`aura_miner.py` is a working reference implementation (full vendor rate reached
12.1 TH/s in SG3) but is **not yet a production drop-in**: backup-pool
failover, factory-test handler, closed-loop fan PID, and full drop-in
acceptance (service swap + web UI reads it) are open (see
`apollo-oss-miner/docs/STATUS.md`). **None of that blocks this plan:** all
mujina-side code (protocol core, chain driver, backends, board, tests against
recorded captures) can be written and verified without touching the device.
Only Phase 6 needs live hardware.

---

## 2. Integration strategy

| Mujina surface | Apollo approach |
|---|---|
| `asic/bm13xx/` | New sibling `asic/aura/` — different framing/CRC/registers (1.2) |
| `board/cpu.rs` + `VirtualBoardDescriptor` | New `board/apollo_iii.rs`, `device_type: "apollo_iii"`, env-configured |
| `backplane.rs` `handle_cpu_event` | New `handle_apollo_event` (or generalize virtual spawn) fed by a startup-only transport event |
| `transport/serial.rs` `SerialStream` | **Reuse as-is** — runtime baud switch already implemented (115200→921600) |
| `hw_trait::{Gpio, I2c}` | New impls: sysfs GPIO (`/sys/class/gpio`), i2c-dev (`/dev/i2c-N` ioctls) |
| `peripheral/` | New `pwm_sysfs.rs` (SIC450 Vout + fan duty), SIC450 PMBus telemetry driver; reuse `peripheral/pmbus` if generic |
| `env_help.rs` | Register `MUJINA_APOLLO_*` vars (single doc source) |
| `HashThread` / scheduler / stratum_v1 / API | Unchanged — Aura chain exposes one `HashThread` per board |

---

## 3. Phased plan

### Phase 0 — Fork & groundwork (done + docs)

- [x] Fork `256foundation/mujina` → `marsmensch/mujina`
- [x] Branch `apollo-iii-integration` (this branch; never commit to `main`)
- [ ] Add `docs/apollo-iii-integration-plan.md` (this file) — commit
- [ ] `git remote add upstream https://github.com/256foundation/mujina.git` (done
  locally; keep `upstream/main` fetched before every PR)

**Verification:** `cargo build` passes on the pristine branch.

---

### Phase 1 — Aura protocol core (pure Rust, no hardware)

**Objective:** frame codec + CRC + register map + job/hit encoders, 100%
unit-tested against captured wire bytes.

**Files:**
- Create `mujina-miner/src/asic/aura/mod.rs` (module root, re-exports)
- Create `mujina-miner/src/asic/aura/crc.rs` — CRC-32/IEEE (reflected
  0xEDB88320, init 0xFFFFFFFF, xorout 0) + test vectors from
  `apollo-oss-miner/tools/aura_proto.py` (already verified against live frames)
- Create `mujina-miner/src/asic/aura/protocol.rs` — `Command`, `Response`
  enums; `FrameCodec` (tokio-util `Decoder`/`Encoder`); constants
  `CMD_MAGIC = 0x12345678`, `RESP_MAGIC = 0x5476C0DA`, `PREAMBLE_LEN = 20`,
  `FRAME_LEN = 16`, `JOB_FRAME_LEN = 92`, `HIT_FRAME_LEN = 92`; register
  addresses per §1.2; `Register::*` wrappers
- Create `mujina-miner/src/asic/aura/error.rs` — `ProtocolError` (mirror
  `bm13xx/error.rs`)
- Create `mujina-miner/src/asic/aura/test_data.rs` — test vectors generated
  from `apollo-oss-miner/run_*.bin` captures (use `tools/wiretap_read.py` to
  emit hex; extract: command frames, response frames, one full bring-up
  sequence, one job frame, one hit frame with known nonce)
- Create `mujina-miner/src/asic/aura/reference_tests.rs` — `mod reference_tests;`
  wired from `mod.rs` exactly like `bm13xx/mod.rs:12`
- Modify `mujina-miner/src/asic/mod.rs` — `pub mod aura;`

**Steps (TDD per task):**
1. `crc.rs`: failing test → implement → pass (vectors from `aura_proto.py`).
2. `protocol.rs` frame codec: encode/decode round-trip + known-byte vectors
   (frame + preamble behavior — codec must emit/consume preamble per frame).
3. Register enum + `Frequency`/`PllConfig` equivalents (N<<20 packing;
   `Fhash = 5N/4`; wire word for N=491 = 0x1eb00000).
4. Job-frame encoder: from stratum job fields → 92 B; verify against a captured
   job frame.
5. Hit-frame parser: nonce at [84:88], full 80-byte header reconstruction.
6. `cargo test` + `cargo clippy -- -D warnings` green.

**Verification:** `cargo test -p mujina-miner asic::aura` (or `cargo test`);
all vectors from real captures pass. This phase is fully doable today.

---

### Phase 2 — Aura chain driver (chip discovery, init, DVFS, telemetry)

**Objective:** multi-pass discovery, chain init, DVFS loops, job dispatch, share
handling — implemented as one `HashThread` impl; unit-testable with an
in-memory fake transport.

**Files:**
- Create `mujina-miner/src/asic/aura/thread.rs` — `AuraThread: HashThread`
  (mirror `bm13xx/thread.rs` structure: per-chain, one thread per board)
- Create `mujina-miner/src/asic/aura/chain.rs` — discovery (multi-pass
  accumulate, pacing ~10.8 s), version-bound partitioning (0x0c30 windows),
  chip init (PLL_CONFIG → PLL_FREQ → DUTY_CYCLE → HASHCONFIG), DVFS InitialSetup
  (18-write program), continuous `0x1f00` heartbeat loop with per-command lock
  (serialize all wire ops — see Pitfalls below)
- Create `mujina-miner/src/asic/aura/telemetry.rs` — hashrate from Δ0x61·2³²/Δt,
  die temp + voltage decode (0x20 ADC formula), clock check via 0x04/0x06
- Modify `mujina-miner/src/asic/hash_thread.rs` if needed — check the trait
  covers "job with flat header fields" (Aura needs prevhash/merkle/ntime/nbits
  intact; BM13xx consumes midstate) — likely no trait change, just a different
  internal job encoder
- Tests: fake transport (in-memory bytes + scripted ACK pattern) driving
  discovery-accumulation and hit handling

**Key behaviors to port from `apollo-oss-miner/tools/aura_miner.py`:**
- Discovery: sweep `reg=0x02`, read ~3.5 s, accumulate unique IDs until 21 or
  pass cap (~24)
- Ramp state machine: `TUNE_INIT → TUNE_SET_FREQ → TUNE_STEPPING_UP → NORMAL`,
  then per-chip `floor_hunt/hold/recover` (see `docs/MEASURED_DVFS_LAW.md`);
  voltage steps: +20×4 (5.0→5.4 V), +10×5 (→5.65 V), +4×15 (→5.97 V), settle
  6.095 V
- Hit handling: poll `0x40` per chip, parse 92-byte frame, extract nonce
  [84:88], re-hash header to compute real difficulty, forward to scheduler
- **Pitfall (from RE):** every wire op must take ONE lock (DVFS burst as one
  critical section) or the DVFS thread corrupts the shared rx buffer; check for
  nested-lock deadlock after.
- **Pitfall:** 8-N-1 at both bauds; the OSS history of literal-baud parity bits
  leaking — `SerialStream` already handles this correctly (rustix termios).

**Verification:** unit tests with fake transport: discovery accumulates 21/21
across scripted passes; job dispatch emits correct 92-byte frame; hit parse
recovers the recorded nonce; DVFS heartbeat emits the exact 5-word payload.
Also `cargo clippy`, `cargo fmt --check`.

---

### Phase 3 — Linux host hardware backends

**Objective:** sysfs/chardev adapters behind existing traits + new peripherals.

**Files:**
- Create `mujina-miner/src/hw_trait/sysfs_gpio.rs` — `SysfsGpio`/`SysfsGpioPin`
  (`Gpio`/`GpioPin` impls over `/sys/class/gpio`: export, direction, value;
  handle active_low via line names/config; gpio100 polarity: active_low=0,
  value=1 = ON)
- Create `mujina-miner/src/hw_trait/linux_i2c.rs` — `LinuxI2c` (`I2c` impl over
  `/dev/i2c-N` with `I2C_RDWR` 0x0707 combined transfers, repeated START, PEC
  off — the vendor's exact access; see memory: OSS bug was plain
  write+read-with-STOP + PEC on)
- Create `mujina-miner/src/peripheral/pwm_sysfs.rs` — `PwmSysfs`: export chip,
  set period/duty/enable via `/sys/class/pwm/pwmchip<N>/pwm<M>/`; used for both
  SIC450 Vout (pwmchip1/pwm0, period 40000, duty 20000→36000) and fan duty
- Create `mujina-miner/src/peripheral/sic450.rs` — SIC450 PMBus telemetry
  (Vout/Iout/status) over `LinuxI2c`; reuse `peripheral/pmbus` machinery if it
  fits, else standalone (check first)
- Create `mujina-miner/src/hw_trait/fan_tach.rs` — fan RPM counter from
  gpiochip0 line 14 (chardev via `gpiod` crate or raw ioctls; PPR=2),
  matching the vendor's 123k-ioctl measurement pattern
- Modify `mujina-miner/src/peripheral/mod.rs` (+ `hw_trait/mod.rs`) to export
- Board temp: read i2c-3 @ 0x49 reg 0x00 (resolve device identity first — see
  Open Questions; tmp1075/tmp451 drivers may already fit)

**Steps:** each backend = trait test with a temp-dir/mock sysfs tree (unit) +
integration test against real paths gated on the device being present
(`#[ignore]`-style or a `MUJINA_APOLLO_TEST_*` env gate).

**Verification:** `cargo test` — mock-sysfs tests prove export/direction/value,
PWM duty writes produce expected sysfs strings, i2c-dev adapter encodes
I2C_RDWR messages correctly (compare against a captured SIC450 exchange).

---

### Phase 3.5 — Apollo OS image extraction & boot-environment mapping (offline)

**Objective:** mine the full firmware image (`upstream/apollo-3_080626.img.xz` —
the complete Apollo 3 OS, already secured in `apollo-oss-miner` but not yet
extracted) for the boot-time environment mujina will inherit, and close the two
open hardware-topology questions without touching the device.

**Context:** the mining-critical blob (`futurebit-miner-v3`, unstripped Go ELF)
is already fully mined; the OS image is secured but NOT yet mined. The vendor
blob assumes boot already exported GPIOs, held gpio100=1 (ASIC rail power), and
exported the PWM/fan — `apollo-hw-setup`/`apollo-helper` do that at boot. Mujina
on stock Apollo OS inherits this; mujina must know exactly what it inherits and
what it must re-establish itself when the vendor service is disabled.

**Files:**
- Extract `upstream/apollo-3_080626.img.xz` → rootfs (xz → raw image; ext4 via
  a Linux helper — podman container per the macOS setup, or 7z)
- Create `docs/apollo-iii-boot-contract.md` — the boot-time contract (below)
- Modify `mujina-miner/src/board/apollo_iii.md` (Phase 5) to reference it

**Steps:**
1. Extract the rootfs offline; no device access needed.
2. Inventory: systemd units (`apollo-miner.service`, `apollo-hw-setup`,
   `node.service`), boot scripts, GPIO/PWM export commands, `/dev/ttyS4`
   permissions + udev rules, ckpool config, web-UI surface.
3. Resolve Open Question #3 (fan PWM path) and #2 (I2C topology) from the image
   + `apollo-helper` disasm + `evidence/vendor_dvfs_boot.strace` I2C_RDWR addrs.
4. Record the boot-time contract: what mujina relies on (boot exports) vs must
   do itself (when replacing the service / running standalone).
5. Optionally RE `apollo-helper` (Rust, `apollo-board-detector` 0.3.0 — partially
   mapped: RD6 / Apollo-BTC / MsPacket handshake) for the board identity the
   web UI expects.

**Verification:** extraction produces a browsable rootfs; every boot-time
GPIO/PWM export and udev rule is listed in the boot-contract doc; the fan-PWM
and I2C-topology questions are answered with file/line evidence from the image.

---

### Phase 4 — Apollo III board composition

**Objective:** wire everything into a board mujina can spawn, drive, and report.

**Files:**
- Create `mujina-miner/src/board/apollo_iii.rs` —
  `inventory::submit! { VirtualBoardDescriptor { device_type: "apollo_iii", … } }`
  (exact `cpu.rs` pattern); `ApolloBoardConfig::from_env()` reading
  `MUJINA_APOLLO_ENABLED`, `MUJINA_APOLLO_SERIAL` (default `/dev/ttyS4`),
  `MUJINA_APOLLO_BAUD_INIT` (115200), `MUJINA_APOLLO_BAUD_MINING` (921600),
  `MUJINA_APOLLO_MODE` (supereco/eco/balanced/turbo/custom), expected chip
  count + ID ranges (21; 0–10, 128–137)
- Modify `mujina-miner/src/board/mod.rs` — `pub(crate) mod apollo_iii;`
- Modify `mujina-miner/src/transport/` — add a startup-only `apollo` transport
  (or reuse the cpu-transport shape) that emits one
  `ApolloDeviceConnected` event when `MUJINA_APOLLO_ENABLED=1`
- Modify `mujina-miner/src/backplane.rs` — `handle_apollo_event` →
  `virtual_registry.find("apollo_iii")`, same shape as `handle_cpu_event`
  (backplane.rs:255–284)
- Modify `mujina-miner/src/env_help.rs` — register every `MUJINA_APOLLO_*` var
- Board bring-up in `create_apollo_board()`:
  1. Open `/dev/ttyS4` via `SerialStream` @115200
  2. gpio148 high; gpio115 reset pulse (0→1); sweep @115200 (multi-pass)
  3. Switch `SerialStream` to 921600
  4. Version-bound partition, chip init, DVFS InitialSetup
  5. Hand thread to scheduler; board loop: gpio100 watchdog kick, DVFS 0x1f00
     heartbeat, telemetry publish (hashrate, per-chip voltage, die temp, PSU
     Vout/Iout, fan RPM, board temp), fan PID against chip temp, thermal trip
     GPIO 138 monitoring → emergency stop path, temp_limit enforcement
  6. Ramp gating: PSU 5.0 V until first job arrives on the chain, then PLL ramp
     + voltage climb per mode preset
- `BoardTelemetry` mapping (api_client::types) — model "FutureBit Apollo III",
  serial from board, one HashThread

**Verification:** `cargo build`; board-level unit tests with the fake transport:
bring-up sequence emits gpio148/gpio115 + discovery sweep in blob order; job
arrival triggers ramp start; thermal trip fires shutdown. Manual smoke on a
stub serial path (e.g. `MUJINA_APOLLO_SERIAL=/tmp/fake` with a pty pair) —
verifiable without hardware.

---

### Phase 5 — System integration & docs

**Objective:** deployable on the Apollo OS and documented for users/contributors.

**Files:**
- Create `mujina-miner/src/board/apollo_iii.md` — board guide (mirror
  `bitaxe_gamma.md`): hardware, wiring, env config, deployment
- Create `mujina-miner/src/asic/aura/REFERENCE.md` — Aura chip reference
  (mirror `bm13xx/REFERENCE.md`): frame format, register map, bring-up,
  DVFS law, sources — the RE-derived constants from §1.2, cross-referenced to
  the Auradine datasheet
- Create `docs/apollo-iii.md` (or extend `docs/cpu-mining.md`-style doc):
  running mujina on Apollo III — systemd unit replacing `apollo-miner.service`
  (sample unit), ckpool wiring, mode presets, safety notes (never kill -9,
  reboot between experiments, vendor recovery as board-health gate)
- Modify `README.md` — add Apollo III to "Landing now" / supported hardware
- Optional Phase-5 stretch (post bring-up): FutureBit web-UI parity — serve the
  vendor's status JSON schema (see `apollo-oss-miner/docs/PARITY_GOALS.md`
  G2.3) so the stock web UI keeps working

**Verification:** docs render; sample unit passes `systemd-analyze verify`
on a Linux host; env vars all listed in `mujina-minerd --help`.

---

### Phase 6 — On-device bring-up & validation (hardware-gated)

**Blocker:** needs a live Apollo III + a settled board (reboot → vendor to full
rate → graceful stop → ≥15–18 s settle before each experiment; `sudo fuser
/dev/ttyS4` free). Run under `apollo-oss-miner` lab discipline (RUNBOOK.md).

**Steps:**
1. Cross-compile or build-on-device `mujina-minerd` (aarch64; check Rust
   target availability on the board or cross toolchain)
2. Discovery bring-up: 21/21 chips, multi-pass timing vs blob
3. Job dispatch + first accepted share on local ckpool (low diff for fast hits)
4. Ramp to full rate: per-mode TH/s vs vendor (supereco 8.0 / eco 12.0 /
   balanced 15.0 / turbo 18.0); sustained-efficiency gap target ≤2.4% (the
   vendor's A1CTRL derate/floor-hunt is the known efficiency lever)
5. Safety: fan PID, thermal trip abort, temp_limit, PSU fault monitoring
6. Drop-in acceptance: replace `apollo-miner.service` with mujina unit; web UI
   (or API) reads live telemetry; 24 h soak at balanced mode

**Verification:** real numbers only — `mujina-cli`/REST shows TH/s, shares,
temps; compare against `apollo-oss-miner` SG3 numbers (12.1 TH/s eco).

---

## 4. Milestones & acceptance

| M | Milestone | Exit criteria |
|---|---|---|
| M1 | Aura protocol core lands | codec/CRC/job/hit unit tests green on captured vectors (no hardware) |
| M2 | Chain driver lands | discovery+init+DVFS+share logic green on fake transport |
| M3 | Backends land | sysfs/i2c/pwm/tach adapters green on mock trees |
| M4 | Board + wiring lands | `mujina-minerd` spawns Apollo board on env, telemetry flows, no-hw smoke passes |
| M5 | Docs + deployment | users can install unit on Apollo OS; README lists Apollo III |
| M6 | On-device full rate | ≥12.1 TH/s eco, shares accepted, safety gates proven (hardware) |

Each PR = one logical surface (protocol / chain / backends / board / docs) —
never mixed. Never commit to `main`; PRs go to `upstream/main` when the
Foundation wants them (after M1/M2 discussion with maintainers is wise — the
Aura protocol facts should be shared early since the BM13xx REFERENCE.md is
explicitly RE-derived and welcomes this).

---

## 5. Risks, tradeoffs, open questions

1. **Datasheet provenance (HIGH).** Constants came from an Auradine datasheet
   obtained through a personal channel (2026-08-14). Publishing derived
   constants (PLL config, ADC formulas, register map) in a public GPL repo is
   almost certainly fine — RE-derived facts are not copyrightable expression —
   but **confirm with the datasheet holder before upstreaming the REFERENCE.md
   verbatim**. Prefer stating derived facts + measured evidence over copying
   datasheet tables.
2. **I2C topology (MEDIUM).** Board temp is read at i2c-3 @ 0x49 reg 0x00 and
   the SIC450 config also appears at 0x49. Resolve from
   `evidence/vendor_dvfs_boot.strace` (I2C_RDWR addrs) and the OS image
   extraction (Phase 3.5) whether 0x49 is one device (SIC450 with temp sense)
   or a separate temp sensor; adjust the temp driver accordingly.
3. **Fan PWM path (MEDIUM).** Fan duty sysfs channel not yet pinned down
   (vendor's fan control PWM chip). Find it in the capture/`-debug=2` logs or
   the OS image boot scripts / `apollo-helper` disasm (Phase 3.5); until then
   fan control is open.
4. **Backplane generalization (LOW).** `handle_cpu_event` is cpu-specific
   (backplane.rs:255). Adding Apollo as a second virtual transport is fine;
   a generalized "virtual board transport" refactor is optional — don't
   over-abstract before a third consumer exists (YAGNI per CODING_GUIDELINES).
5. **apollo-oss-miner dependency (LOW for M1–M5, MEDIUM for M6).** The drop-in
   being "not ready" does not block any mujina-side work. M6 (on-device
   validation) needs only the device + discipline, not the drop-in.
6. **Licensing.** Both projects are GPL-3.0 — importing protocol constants and
   algorithms from `apollo-oss-miner` into mujina is compatible. Keep
   attribution in REFERENCE.md ("derived from apollo-oss-miner RE work").
7. **Environment vs config.** Mujina config is env-var-driven today
   (TOML is stubbed — `config.rs`). Follow the existing env pattern; when TOML
   lands, board config moves there.
8. **Serial exclusivity.** `apollo-miner.service` must be stopped before
   mujina runs (device is exclusive-ish); the systemd unit in Phase 5 must
   enforce ordering, mirroring the OSS service's lifecycle discipline.

---

## 6. What can be done right now (no drop-in, no hardware)

Phases 0–3 + Phase 3.5 (OS image extraction — the image
`upstream/apollo-3_080626.img.xz` is already secured) + the Phase-4 board/
backplane code (with fake-transport and mock-sysfs tests) are all executable
immediately: they are pure Rust + recorded captures + mock trees + offline
rootfs mining. Only Phase 6 needs the physical device. The fork is at
`https://github.com/marsmensch/mujina`, branch `apollo-iii-integration` — the
first PR (Phase 1, protocol core) can start today.
