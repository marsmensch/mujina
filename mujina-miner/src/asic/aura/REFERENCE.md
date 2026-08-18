# Aura Chip Reference

This document describes the Auradine "Aura" (Treasure-family) Bitcoin
mining ASIC used by the FutureBit Apollo III: the wire protocol, the
register map, job/hit framing, bring-up, DVFS, and telemetry formulas.

> **Provenance (project risk R1).** The facts below were derived from
> clean-room reverse engineering of vendor traffic — the unstripped
> `futurebit-miner-v3` Go blob (disassembly), its `-debug` protocol
> traces, full-rate `strace` captures, and on-device captures — cross-
> checked against an Auradine datasheet obtained through a personal
> channel (2026-08-14). Publishing **derived facts** (protocol constants,
> register semantics, measured values) is fine: RE-derived facts are not
> copyrightable expression, and every constant below is also committed in
> this repository's code with byte-exact test vectors. **Verbatim
> datasheet tables are NOT reproduced here** — copying them is gated on
> the datasheet holder's approval (see `../../../docs/apollo-iii-project-plan.md`
> R1 / D3). Nothing in this file is a datasheet table.

Contents:

- [Overview]
- [Conventions]
- [The Serial Link]
- [Frame Format]
    - [Command Frames]
    - [Response Frames]
    - [Job Frames]
    - [Hit Frames]
    - [The Preamble]
    - [CRC]
    - [Byte Order]
- [Command Types]
- [Register Map]
    - [0x00 - CHIP_ID]
    - [0x01 - WORK_DATA]
    - [0x02 - TELEMETRY]
    - [0x04 / 0x06 - Clock Counters]
    - [0x10 - VERSION_BOUND]
    - [0x11 - VERSION_SHIFT]
    - [0x13 - HITCONFIG]
    - [0x14 - HASHCONFIG]
    - [0x18 - PLL_CONFIG]
    - [0x19 - PLL_FREQ]
    - [0x20 - VOLTAGE_ADC]
    - [0x25 - SMALL_NONCE]
    - [0x60 / 0x61 - Hit Counters]
    - [0x68 - DUTY_CYCLE]
    - [0x81 - DVFS]
    - [0x84 - JOB]
- [Chip Discovery]
- [Bring-up Sequence]
- [DVFS]
    - [InitialSetup]
    - [Heartbeat]
    - [Frequency Ramp and PSU Climb]
- [Telemetry Formulas]
- [Sources]

## Overview

The Aura ASIC is a frame-based serial chip. The host sends 16-byte
register commands and 92-byte flat job frames; the chip answers register
reads with 16-byte response frames and reports winning nonces as 92-byte
hit frames. Unlike the BM13xx family there is **no midstate template**:
the chip computes the header hash itself from prevhash, merkle, ntime,
nbits and a starting nonce written flat into the job frame.

A full-rate Apollo III chain is 21 chips (IDs `0x00..0x0a` and
`0x80..0x89`) at PLL N = 491 (`Fhash = 5·491/4 = 613.75 MHz`), about
12.1 TH/s.

## Conventions

- Byte and word numbers refer to serialized data and count transmission
  order from zero: byte 0 is sent first.
- Bytes written as space-separated pairs (`78 56 34 12 ...`) are
  hexadecimal; elsewhere hexadecimal values carry the 0x prefix and
  unprefixed numbers are decimal.
- Multi-byte fields are little-endian unless a section says otherwise
  (the one exception is the response magic, which appears big-endian on
  the wire — see [Response Frames]).
- **chain**: the ASICs daisy-chained on one serial link, driven by one
  host. The Apollo III chain is direct SoC↔ASIC with no MCU.

## The Serial Link

- The chain is driven from the SoC's UART: `/dev/ttyS4` on the Apollo
  III (Radxa ROCK 5B+ / RK3588).
- Discovery runs at 115200 8-N-1; after discovery the link switches to
  921600 8-N-1 for mining. Both bauds are 8-N-1 (vendor strace shows no
  PARENB/PARODD).
- Command frames are preceded by a 20-byte zero preamble (see
  [The Preamble]); job frames are written as a single 92-byte write with
  no inline preamble.

## Frame Format

### Command Frames (Host -> Chip)

Every register command is a 16-byte frame:

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 4 | magic | `78 56 34 12` (LE `0x12345678`) |
| 4 | 1 | chip | `0x80` = broadcast, else the chip address |
| 5 | 1 | reg | register byte (0x84 for jobs, `0x84 | 0x40` for hit polls) |
| 6 | 2 | lcmd | low-level command word, LE (see [Command Types]) |
| 8 | 4 | data | command payload, LE |
| 12 | 4 | crc32 | CRC-32 over bytes `[0..12]`, LE |

### Response Frames (Chip -> Host)

16 bytes, same layout:

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 4 | magic | wire bytes `54 76 c0 da` (BE `0x5476C0DA`) |
| 4 | 1 | chip | chip address that answered |
| 5 | 1 | reg | register echoed |
| 6 | 2 | lcmd | low-level command word echoed, LE |
| 8 | 4 | data | response payload, LE |
| 12 | 4 | crc32 | CRC-32 over bytes `[0..12]`, LE |

The response magic is the one big-endian field in the protocol: it is
read/written with `to_be_bytes`/`from_be_bytes` and the decoder
frame-syncs on those exact wire bytes.

### Job Frames (Host -> Chip, 92 bytes)

A job is written in a single 92-byte write, no preamble:

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 4 | magic | `78 56 34 12` (LE) |
| 4 | 1 | chip | per-chip or broadcast address |
| 5 | 1 | reg | `0x84` |
| 6 | 2 | lcmd | `slot * 0x400 + 0x2a`, LE |
| 8 | 4 | job_id | LE |
| 12 | 32 | prevhash | block header prevhash, as-is |
| 44 | 32 | merkle | block header merkle root, as-is |
| 76 | 4 | ntime | LE |
| 80 | 4 | nbits | LE |
| 84 | 4 | nonce_start | LE |
| 88 | 4 | crc32 | CRC-32 over bytes `[0..88]`, LE |

The chip hashes the 80-byte header assembled from these fields — there is
no midstate.

### Hit Frames (Chip -> Host, 92 bytes)

A chip reports a found nonce when polled (see [Command Types], hit poll;
empty hit FIFO yields no response at all):

| Offset | Size | Field | Notes |
|---|---|---|---|
| 0 | 4 | magic | `78 56 34 12` (LE) — **hit frames carry the command magic** |
| 4 | 1 | chip | chip that found the nonce |
| 5 | 1 | reg | `0xc4` (`0x84 | 0x40`) |
| 6 | 1 | nbits | echo |
| 7 | 1 | id-hi + seq | job/slot identity |
| 8 | 80 | header | complete winning 80-byte block header |
| 88 | 4 | crc32 | CRC-32 over bytes `[0..88]`, LE |

The **nonce is the last 4 bytes of the header, absolute frame bytes
`[84..88]`** — read directly, no counter reconstruction. *Open (G6): the
hit frame's magic field (COMMAND vs RESPONSE) is assumed COMMAND from
the committed vectors; confirm on device.*

### The Preamble

Every command frame is preceded on the wire by a 20-byte all-zero
preamble, written as a **separate write syscall** (36 bytes on the wire
for a 16-byte command). Job frames have no preamble. The receive side
drops preamble/junk bytes while frame-syncing on the response magic.

### CRC

CRC-32/IEEE-802.3 style: reflected polynomial `0xEDB88320`, initial
value `0xFFFFFFFF`, no final XOR (`xorout 0`), transmitted
little-endian. Computed over the frame bytes preceding the CRC word —
`[0..12]` for 16-byte frames, `[0..88]` for 92-byte frames. The check
value for `"123456789"` is `0x340bc6d9` (this variant's standard
self-check).

### Byte Order

Everything is little-endian except the response magic (big-endian on the
wire, see above): multi-byte fields (lcmd, data, job fields, CRC) are
sent least significant byte first. The block-header material inside job
and hit frames is sent as-is (the header's own little-endian
serialization).

## Command Types

Register commands use the frame's `reg` byte (the register address) and
an `lcmd` word that selects the operation class:

| Operation | reg byte | lcmd | Notes |
|---|---|---|---|
| Register read | register | e.g. `0x1200` telemetry, `0x1000` ordinary | response arrives asynchronously; per-chip reads are non-broadcast |
| Register write | register | `0x1000` ordinary (REG_LCMD) | broadcast for chain-wide config |
| Hit poll | `0x84 \| 0x40` = `0xc4` | `0x0000` | per-chip, fire-and-forget; one queued hit per poll |
| Job write | `0x84` | `slot * 0x400 + 0x2a` | see [Job Frames] |
| DVFS heartbeat | `0x81` | `0x1f00` | 5 payload words, broadcast |
| DVFS InitialSetup | `0x81` | per-write lcmd (see [InitialSetup]) | 18 writes, broadcast |

The hit return is a command modifier: polling with `0x84 | 0x40` asks
the chip to emit one queued 92-byte hit frame per nonce found. An empty
FIFO yields no response, which is normal.

## Register Map

All registers are 4-byte values unless noted.

### 0x00 - CHIP_ID

Chip identity; the discovery sweep reads this register's telemetry path
via [0x02 - TELEMETRY]. Apollo III chips report IDs `0x00..0x0a` and
`0x80..0x89`.

### 0x01 - WORK_DATA

Per-chip initialization (PLL frequency, duty, version bound) written at
chain init.

### 0x02 - TELEMETRY

Telemetry register used for discovery: a broadcast sweep
(`lcmd 0x1200`) is answered by a **subset** of chips per pass (see
[Chip Discovery]).

### 0x04 / 0x06 - Clock Counters

Reference and hash clock counters. Hash frequency:
`Fhash_MHz = 2·Δ0x06 / Δ0x04 · 25`.

### 0x10 - VERSION_BOUND

Per-chip private nonce window. Window width is `0x0c30`; for chip index
`i`: `lower = i·0x0c30`, `upper = lower + 0x0c2f`, packed as
`lower | (upper << 16)`. Windows are inclusive, non-overlapping, width a
multiple of 4; 21 chips tile `0x0000..=0xffef`.

### 0x11 - VERSION_SHIFT

Version shift value, written `13` at chain init.

### 0x13 - HITCONFIG

Hit configuration; bit 0 = auto-report (whether hits are queued for
polling / reported automatically).

### 0x14 - HASHCONFIG

Hash configuration, value `0x02000200` at init. **Must be rewritten
after every duty-cycle change** (0x68) or the engines never see the duty
change.

### 0x18 - PLL_CONFIG

PLL configuration, initial value `0x00502411` (pllen=1, bypass=0,
postdiven=1, div1=1, div2=1, refdiv=5).

### 0x19 - PLL_FREQ

PLL multiplier, packed as `N << 20` (12-bit multiplier N). Hash
frequency `Fhash = 5·N/4 MHz`; N = 491 → 613.75 MHz, wire word
`0x1eb00000`.

### 0x20 - VOLTAGE_ADC

Core voltage ADC: `V = raw · 0.0001011035 − 0.276029` volts.

### 0x25 - SMALL_NONCE

Small-nonce control (purpose not yet mapped; present in the register
map and committed code).

### 0x60 / 0x61 - Hit Counters

Stochastic hit counters: `0x60` is preliminary (leads), `0x61` is the
authoritative hit count. Each count increment represents 2^32 hashes.
`Δ0x61/Δ0x60` is the engine health ratio (1.0 = healthy).

### 0x68 - DUTY_CYCLE

Duty-cycle register: `0x8080` while the hash clock is at/below 600 MHz,
`0x8088` above 600 MHz. Boundary: `Fhash(480) = 600 MHz` exactly → low
duty; `Fhash(491) = 613.75 MHz` → high duty.

### 0x81 - DVFS

DVFS control block: the 18-write InitialSetup program and the continuous
heartbeat (see [DVFS]).

### 0x84 - JOB

Job write register (see [Job Frames]).

## Chip Discovery

Discovery is **multi-pass and probabilistic**: a broadcast telemetry
sweep (`reg 0x02`, `lcmd 0x1200`) is answered by only a few chips per
pass — a different subset each pass. The driver:

1. Broadcasts the sweep, then drains responses for ~3.5 s.
2. Accumulates unique ACKing chip IDs across passes.
3. Paces passes ~10.8 s apart (start-to-start), up to 24 passes.
4. Stops early once `MUJINA_APOLLO_EXPECTED_CHIPS` (default 21) unique
   chips have ACKed.

A single pass never reaches 21. The vendor blob re-strobes a config
frame between passes; mujina's driver relies on the sweep alone and
accumulates unique IDs. A short chain is a warning, not an error — the
caller decides.

## Bring-up Sequence

The blob's exact order, and mujina's:

1. GPIO 148 high (heartbeat clock) — only hardware touch at +0.00 s.
2. GPIO 115 reset pulse `0 → 1` at +0.65 s.
3. First discovery sweep at +0.88 s (115200 baud).
4. Version bounds per chip, DVFS InitialSetup, chain init
   (PLL_CONFIG → PLL_FREQ → DUTY_CYCLE → HASHCONFIG) at the ramp start.
5. Switch the link to 921600; hold PSU at baseline 5.0 V.
6. On the first live job, run the frequency ramp (below).

Do **not** touch gpio100/gpio139/gpio138 or run an SIC450 I2C init
before discovery — disturbing the board early caused a 0-chip discovery
bug during the RE project.

## DVFS

### InitialSetup

The one-time 18-write program to register 0x81 (broadcast), verbatim
from the full-rate vendor strace and locked as ground truth. The frame
register byte is always 0x81; the sub-register address is the low byte
of the lcmd word (0x00 for all locked writes — the lcmd high byte
selects the sub-block).

| # | lcmd | data |
|---|---|---|
| 1 | 0x1800 | 11245000 |
| 2 | 0x1900 | 0x0000000a |
| 3 | 0x2500 | 0x00f0ff00 |
| 4 | 0x1400 | 0x02000200 |
| 5 | 0x6800 | 0x80800000 |
| 6 | 0x1400 | 0x02000200 |
| 7 | 0x1c00 | 0x01000000 |
| 8 | 0x1d00 | 0x0d000000 |
| 9 | 0x2500 | 0x00f0ff00 |
| 10 | 0x1400 | 0x02000200 |
| 11 | 0x6800 | 0x80800000 |
| 12 | 0x1400 | 0x02000200 |
| 13 | 0x1900 | 0x00000005 |
| 14 | 0x2300 | 0x44080000 |
| 15 | 0x6800 | 0x80800000 |
| 16 | 0x1900 | 0x0000000a |
| 17 | 0x2400 | 0x00000000 |
| 18 | 0x2300 | 0x44080000 |

### Heartbeat

Every ~2.1 s the host writes the five payload words to register 0x81
with lcmd `0x1f00` (broadcast):

`[0x02, 0x03, 0x01, 0x5074, 0x05]`

`0x5074` is the corrected vendor full-rate payload (not the older
`0x5009`). All wire operations in a heartbeat burst take a single lock —
a DVFS thread corrupting the shared RX buffer was a real failure mode in
the reference implementation.

### Frequency Ramp and PSU Climb

- The ramp raises PLL_FREQ N from 80 to 491 in +20 steps (~25 MHz per
  step, ~50 ms apart), rewriting DUTY_CYCLE (and HASHCONFIG) on every
  step.
- The ramp **only runs once pool work is live on the chips**. Ramping
  before work = zero hitrate. The driver gates it on the first job.
- In parallel, the board's PSU PWM duty climbs from 20000 ns (~5.0 V) to
  36000 ns (~6.1 V) — one duty step per ramp step, linear between the
  vendor anchors (linearity to be confirmed on device, G6).
- `MUJINA_APOLLO_MODE` (TH/s) maps to a ramp stop: full rate 12.1 TH/s →
  N 491; lower targets stop the ramp early at the corresponding N,
  clamped to the ramp range.

## Telemetry Formulas

Per-chip, from counter deltas between two samples (counters wrap at
32 bits; deltas are exact mod 2^32):

- Hashrate: `GH/s = Δ0x61 · 2^32 / Δt / 1e9` (0x61 = authoritative hit
  count).
- Hash clock: `Fhash_MHz = 2 · Δ0x06 / Δ0x04 · 25`.
- Core voltage: `V = raw0x20 · 0.0001011035 − 0.276029`.
- Engine health: `Δ0x61 / Δ0x60` (1.0 = healthy engine).

Board-level (Apollo III): board temperature = SIC450 PMBus reg 0x00 at
i2c-3 @ 0x49, 1-byte read decoded LM75-style (0.5 °C/LSB, two's
complement); PSU voltage = from the commanded PWM duty readback
(20000 ns ≈ 5.0 V, 36000 ns ≈ 6.1 V). See the
[board guide](../../board/apollo_iii.md).

## Sources

- **Reverse engineering of vendor traffic** (clean-room, apollo-oss-miner
  project, 2026-08-08 → 2026-08-18): unstripped `futurebit-miner-v3`
  Go blob disassembly, `-debug=2` protocol traces, full-rate `strace`
  captures (3078× i2c-3 accesses), and on-device wire captures.
  Writeups: `docs/apollo-iii-aura-reverse-engineering.md`,
  `docs/CERTAINTY.md`, `docs/MEASURED_DVFS_LAW.md` in apollo-oss-miner.
- **Auradine datasheet** (personal channel, 2026-08-14): cross-check for
  register semantics. Derived facts only; verbatim tables gated on the
  holder's approval (project risk R1 / dependency D3).
- **This repository**: `mujina-miner/src/asic/aura/` — committed code
  and byte-exact test vectors (`reference_tests.rs`, `test_data.rs`)
  generated from the captures. The 92-byte job frame is byte-exact
  against an on-device capture.
- **Board context**: `../../../docs/apollo-iii-boot-contract.md`
  (hardware paths and the image contract),
  `mujina-miner/src/board/apollo_iii.rs`.

[Overview]: #overview
[Conventions]: #conventions
[The Serial Link]: #the-serial-link
[Frame Format]: #frame-format
[Command Frames]: #command-frames-host---chip
[Response Frames]: #response-frames-chip---host
[Job Frames]: #job-frames-host---chip-92-bytes
[Hit Frames]: #hit-frames-chip---host-92-bytes
[The Preamble]: #the-preamble
[CRC]: #crc
[Byte Order]: #byte-order
[Command Types]: #command-types
[Register Map]: #register-map
[0x00 - CHIP_ID]: #0x00---chip_id
[0x01 - WORK_DATA]: #0x01---work_data
[0x02 - TELEMETRY]: #0x02---telemetry
[0x04 / 0x06 - Clock Counters]: #0x04--0x06---clock-counters
[0x10 - VERSION_BOUND]: #0x10---version_bound
[0x11 - VERSION_SHIFT]: #0x11---version_shift
[0x13 - HITCONFIG]: #0x13---hitconfig
[0x14 - HASHCONFIG]: #0x14---hashconfig
[0x18 - PLL_CONFIG]: #0x18---pll_config
[0x19 - PLL_FREQ]: #0x19---pll_freq
[0x20 - VOLTAGE_ADC]: #0x20---voltage_adc
[0x25 - SMALL_NONCE]: #0x25---small_nonce
[0x60 / 0x61 - Hit Counters]: #0x60--0x61---hit-counters
[0x68 - DUTY_CYCLE]: #0x68---duty_cycle
[0x81 - DVFS]: #0x81---dvfs
[0x84 - JOB]: #0x84---job
[Chip Discovery]: #chip-discovery
[Bring-up Sequence]: #bring-up-sequence
[DVFS]: #dvfs
[InitialSetup]: #initialsetup
[Heartbeat]: #heartbeat
[Frequency Ramp and PSU Climb]: #frequency-ramp-and-psu-climb
[Telemetry Formulas]: #telemetry-formulas
[Sources]: #sources
