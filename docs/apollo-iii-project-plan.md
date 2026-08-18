# Mujina on Apollo III — Project Plan

| | |
|---|---|
| **Project** | Mujina on Apollo III: flasheable miner-only image (Auradine ASIC support) replacing the closed `futurebit-miner-v3` firmware |
| **Version** | 1.1 (2026-08-18) |
| **Sponsor / approver** | mars |
| **Execution lead** | Hermes (this agent) |
| **Repos** | Fork: `marsmensch/mujina` (branch `apollo-iii-integration`) · Source of truth: `apollo-oss-miner` (READ/COPY ONLY until approved) |
| **Upstream** | `256foundation/mujina` (GPL-3.0) |

## 1. Charter

**Objective.** Flash a **mujina OS image to the Apollo III's microSD card and
have the miner working** — mujina-minerd driving the 21 Auradine "Aura" ASICs
at **≥ vendor hashrate and efficiency**, mining to a public pool. Changes to
the fork are limited to the **absolute essentials** for Auradine ASIC support
and mujina's included mining stack (stratum v1 client, scheduler, API).
**Bitcoin node implementation is OUT OF SCOPE.**

**Why now.** The protocol ground truth already exists from the apollo-oss-miner
RE project (vendor blob disassembly, live captures, datasheet, OS image). The
entire mujina-side implementation is doable **without** the drop-in being ready
and **without** hardware — only on-device validation is hardware-gated.

**In scope (essentials).** Aura ASIC driver (new chip family — NOT BM13xx),
minimal Linux backends (ttyS4 UART, sysfs GPIO, PWM for PSU + fan, fan tach,
board temp), Apollo III board composition, a **flasheable Armbian-based image**
(mujina-minerd + boot-time exports + pool config), documentation, upstream PRs.

**Out of scope (explicit).** **Bitcoin node** (bitcoind) and everything around
it (ckpool, NVMe node disk, block sync); **apolloapi / stock web UI and
status-JSON parity** (apollo-miner-3.json, /tmp/fan) — follow-on work, not
part of initial support; **drop-in service swap on the stock Apollo OS** (the
deliverable is our own flasheable image, not a modification of the vendor's
running system); apollo-oss-miner drop-in readiness (separate project — RE
intel source only); WiFi and non-mining chassis features; non-Apollo boards;
mujina TOML config (env-var conventions stand).

**Repository rules.** Never commit to `main`. One commit/PR per logical surface
(protocol / chain / backends / board / docs). Fork stays in sync with
`upstream/main` before every PR.

## 2. Goals & subgoals

**G0 — Governance & intel secured — ✅ DONE**
- 0.1 Fork created; branch `apollo-iii-integration`; upstream remote wired.
- 0.2 apollo-oss-miner read/copy-only guardrail in force (no modifications without approval).
- 0.3 OS image extracted; boot contract documented (`docs/apollo-iii-boot-contract.md`).
- 0.4 Open hardware questions resolved (fan PWM = `pwmchip0`, PSU = `pwmchip1`, I2C 0x49 = SIC450).

**G1 — Aura protocol core lands in mujina — ⏳ NOT STARTED (doable now, no hardware)**
- 1.1 Frame codec + CRC-32 + register map, unit-tested against captured wire bytes.
- 1.2 Job-frame encoder (92 B flat) + hit-frame parser (nonce @ [84:88]).
- 1.3 Test-vector corpus generated from `apollo-oss-miner` captures.
- *Acceptance:* `cargo test` green on real vectors; `cargo clippy -D warnings` clean. **M1.**

**G2 — Aura chain driver (discovery → init → DVFS → shares) — ⏳ NOT STARTED**
- 2.1 Multi-pass discovery (21 chips, IDs 0–10 + 128–137, probabilistic ACK accumulation).
- 2.2 Chain init (version bounds, PLL, duty+HASHCONFIG) + 18-write DVFS InitialSetup.
- 2.3 Continuous DVFS heartbeat + hashrate telemetry (Δ0x61·2³²/Δt); single-lock wire serialization.
- 2.4 Share handling via 0x40 hit return; fake-transport tests.
- *Acceptance:* fake-transport tests green (21/21 discovery, exact 5-word DVFS payload, recorded-nonce hit recovery). **M2.**

**G3 — Linux host hardware backends — ⏳ NOT STARTED**
- 3.1 Sysfs GPIO (export/direction/value; gpio100 polarity).
- 3.2 i2c-dev (`I2C_RDWR` combined, repeated START, PEC off — the SIC450 path).
- 3.3 Sysfs PWM peripheral (PSU `pwmchip1` + fan `pwmchip0`).
- 3.4 Fan tach (gpiochip0 line 14, PPR=2) + SIC450 PMBus telemetry driver.
- *Acceptance:* mock-sysfs/i2c unit tests green; captured SIC450 exchange reproduced. **M3.**

**G4 — Apollo III board composition — ⏳ NOT STARTED**
- 4.1 `board/apollo_iii.rs` virtual board + `MUJINA_APOLLO_*` env config + backplane wiring.
- 4.2 Bring-up sequence in blob order (gpio148 → gpio115 pulse → sweep @115200 → 921600).
- 4.3 Board loop: gpio100 watchdog, DVFS heartbeat, telemetry, fan PID, thermal-trip shutdown.
- 4.4 Ramp gating: PSU 5.0 V until stratum job → PLL ramp + voltage climb per mode preset.
- *Acceptance:* no-hw smoke passes (pty stub); `mujina-minerd` spawns board on env; telemetry flows. **M4.**

**G5 — Flasheable mujina image (microSD) — ⏳ NOT STARTED (skeleton doable now)**
- 5.1 Image skeleton early: Armbian base (RK3588), `mujina-minerd`, systemd
     unit, boot-time GPIO/PWM exports — boots and mines with the CPU backend
     as a smoke gate (de-risks the flash story before ASIC code lands).
- 5.2 Pool config: stratum to a public pool (vendor default snapshot:
     `stratum.braiins.com:3333`); user/pool set via env / first-run.
- 5.3 Docs: board guide, Aura REFERENCE.md, flashing/deployment guide, README.
- *Acceptance:* image flashes to microSD, boots, runs `mujina-minerd`; env vars
  in `mujina-minerd --help`. **M5.**

**G6 — On-device bring-up to full rate on the flashed image — 🔒 BLOCKED (hardware)**
- 6.1 aarch64 `mujina-minerd` on the flashed image; 21/21 discovery.
- 6.2 First accepted share on a public pool; ramp to full rate per mode target.
- 6.3 Sustained-efficiency gap ≤ 2.4% vs vendor (A1CTRL derate/floor-hunt is
     the known lever).
- 6.4 Safety: fan PID, thermal-trip abort, temp_limit, PSU fault monitoring.
- *Acceptance:* ≥ 12.1 TH/s eco, shares accepted, safety gates proven;
  24 h soak at balanced. **M6.**

**G7 — Upstream contribution — 🔒 BLOCKED (needs G1/G2 review engagement)**
- 7.1 Maintainer engagement at M1/M2 (Aura protocol facts shared early —
     BM13xx REFERENCE.md is RE-derived and welcomes this).
- 7.2 Per-surface PRs to `256foundation/mujina` (protocol → chain → backends →
     board → docs).
- 7.3 Datasheet-provenance gate cleared before upstreaming REFERENCE.md verbatim.
- *Acceptance:* ≥ 1 PR merged upstream; review feedback incorporated. **M7.**

**Follow-on (explicitly NOT in initial support scope):** stock web-UI /
apolloapi parity (apollo-miner-3.json, /tmp/fan), ckpool / node integration on
the image, vendor mode-preset table (start with a single hashrate/temp target),
full SIC450 multi-rail telemetry.

## 3. Workstreams

| WS | Covers | Lead routing |
|---|---|---|
| WS-ASIC | G1, G2 | Implementation: DeepSeek V4 Flash 0731 via `delegate_task` (standing rule). Critical analysis/verification: Kimi K3. NOT Codex CLI / Claude Code. |
| WS-PLATFORM | G3, G4 | Same routing as WS-ASIC. |
| WS-IMAGE | G5 | Image assembly + docs; implementation/delegation as WS-ASIC. |
| WS-HW | G6 | Hardware-lab discipline per `apollo-oss-miner` RUNBOOK (reboots mandatory; never kill -9). |
| WS-UPSTREAM | G7 | Hermes + mars (maintainer comms; no unsolicited external contact). |

## 4. Dependencies & constraints

| # | Dependency | Effect | Status |
|---|---|---|---|
| D1 | apollo-oss-miner (RE intel source) | Source of protocol/boot ground truth (read/copy-only). NOT a delivery dependency; drop-in readiness is a separate project | Intel done; drop-in separate |
| D2 | Live Apollo III + settled board for each experiment | G6 hard-gate; reboot → vendor full rate → graceful stop → settle ≥ 15–18 s per run | Device available |
| D3 | Datasheet holder approval | Gate on upstreaming REFERENCE.md verbatim (G7.3); derived facts publishable regardless | Open — needs mars |
| D4 | 256 Foundation maintainers | G7 engagement; start at M1/M2 | Not engaged yet |
| D5 | Codex OAuth usage limit (until ~Aug 20) | Delegation routing already avoids Codex for this project | No impact |
| D6 | Model API 503 upstream-capacity | Transient; wait 3–5 min, retry same op (standing directive) | N/A |

## 5. Timeline (estimates — hardware-gated items shift on device availability)

| Window | Deliverable | Milestone |
|---|---|---|
| W1–W2 (Aug 18 – Aug 29) | G1 Aura protocol core + test corpus | M1 |
| W2–W4 | G2 chain driver | M2 |
| W3–W4 (parallel) | G3 backends | M3 |
| W4–W5 | G4 board + wiring | M4 |
| W4–W6 (parallel) | G5 image skeleton → flasheable image | M5 |
| W6+ | G6 on-device bring-up on flashed image (hardware window) | M6 |
| Rolling | G7 upstream PRs | M7 |

All estimates ±; plan is execution-velocity-driven, not date-driven. G0 done 2026-08-18.

## 6. Milestones & acceptance

| M | Exit criteria | Verification (no self-reports) |
|---|---|---|
| M1 | Codec/CRC/job/hit green on captured vectors | `cargo test` output; vectors traced to `run_*.bin` |
| M2 | Discovery/init/DVFS/shares green on fake transport | `cargo test`; byte-exact frame assertions |
| M3 | Backends green on mock trees | `cargo test`; captured SIC450 exchange reproduced |
| M4 | Board spawns on env; telemetry flows | `mujina-minerd` run + REST/CLI output; pty-stub smoke |
| M5 | Flasheable image boots and mines (CPU smoke → Aura board) | `systemd-analyze verify`; image written + boots on device (or arm64 VM/QEMU smoke) |
| M6 | ≥ 12.1 TH/s eco; shares accepted; safety proven | On-device numbers vs vendor (12.1–12.23 TH/s baseline) |
| M7 | ≥ 1 upstream PR merged | GitHub PR state |

## 7. Risk register

| # | Risk | P | I | Mitigation | Owner |
|---|---|---|---|---|---|
| R1 | Datasheet provenance blocks upstream REFERENCE.md | M | H | Publish derived facts + measured evidence; get mars to clear D3 before verbatim tables | mars |
| R2 | Board wedge / vendor crash-loop during HW work | H | H | Mandatory reboot between experiments; vendor full-rate recovery as board-health gate; never kill -9 | Hermes + RUNBOOK |
| R3 | Image-build issues (Armbian version, RK3588 DTB, kernel, rootfs) | M | M | Build the image skeleton early (G5.1) with CPU-miner smoke; pin a known-good Armbian RK3588 base; validate boot in an arm64 VM before device | Hermes |
| R4 | Upstream rejects scope / maintainers unresponsive | M | M | Engage at M1/M2 with the protocol facts; per-surface PRs; fall back to long-lived fork branch | mars |
| R5 | Aura protocol misread (RE error) | L | H | Every constant cross-checked vs blob disasm + captures + datasheet (skill 7b/7c method); byte-level test vectors | Hermes |
| R6 | Delegation drift (self-reported success) | M | M | Verification rule: inspect diffs, run real gates, never trust child summaries | Hermes |
| R7 | Narration-loop / unplanned-stop failure mode | M | H | Guardrail in memory; self-contained handoffs; wait for explicit "go" when flagged | Hermes |
| R8 | Scope creep (TOML, other boards, node work) | M | L | Out-of-scope list in §1; YAGNI per CODING_GUIDELINES | mars |

## 8. Team & delegation

- **mars** — sponsor, approver, hardware-lab operator, external contacts (datasheet holder, 256 Foundation).
- **Hermes** — execution lead: routing, delegation, review, verification, commits (branch only).
- **DeepSeek V4 Flash 0731** (via `delegate_task`) — code + doc implementation (standing rule; not Codex/Claude for this project).
- **Kimi K3** — critical analysis and code verification.
- **256 Foundation maintainers** — upstream reviewers (external; engaged at M1/M2).
- **Verification policy:** every deliverable backed by real tool output (diffs, test runs, on-device numbers). No self-report accepted.

## 9. Communication & review cadence

- Status report at the end of each milestone (M1–M8) in this session.
- Review gates: **M1/M2** (pre-upstream engagement), **M6 numbers**, **M7 acceptance**.
- Decision log (§10) updated on every material decision; plan version bumped.
- External contact is never made without mars's approval (standing rule).

## 10. Decision log

| Date | Decision | Rationale | Status |
|---|---|---|---|
| 2026-08-18 | Fork `marsmensch/mujina`; work on `apollo-iii-integration`, never `main` | Repo rule; one surface per commit | ✅ |
| 2026-08-18 | Aura = new `asic/aura/` driver, not a bm13xx variant | Different frame magic, CRC-32, register map (verified) | ✅ |
| 2026-08-18 | Board = env-driven `VirtualBoardDescriptor` (cpu.rs pattern) | Matches mujina architecture; no USB hotplug on Apollo | ✅ |
| 2026-08-18 | Reuse `SerialStream` for 115200→921600 switch | Already implemented; zero changes needed | ✅ |
| 2026-08-18 | Voltage via PWM (`pwmchip1`), telemetry via PMBus 0x49; fan on `pwmchip0` | Live-board evidence (CERTAINTY A19e) beats stale blob self-report | ✅ |
| 2026-08-18 | Phase 3.5 (OS image mining) completed; boot contract documented | Boot environment is the deployment contract | ✅ |
| 2026-08-18 | **Scope:** miner-only flasheable image; bitcoin node (bitcoind/ckpool), stock UI parity, drop-in service swap all OUT of scope for initial support | End state = flash microSD → mine; minimal footprint for Auradine support | ✅ |
| 2026-08-18 | Datasheet provenance is an explicit gate (D3/R1) | Personal-channel source; confirm before verbatim upstreaming | ⏳ mars |

## 11. Status summary

| Area | Status |
|---|---|
| G0 Governance & intel | ✅ Done |
| G1 Protocol core | ⏳ Not started — doable now |
| G2 Chain driver | ⏳ Not started |
| G3 Backends | ⏳ Not started |
| G4 Board | ⏳ Not started |
| G5 Flasheable image | ⏳ Not started — skeleton doable now |
| G6 On-device bring-up | 🔒 Blocked (hardware) |
| G7 Upstream | 🔒 Blocked (needs G1/G2) |
| Follow-on (UI parity, node/ckpool, modes, telemetry) | ⛔ Out of scope for initial support |

## 12. Success criteria (project level)

1. Flash the mujina image to microSD → boots → miner reaches **≥ 12.1 TH/s eco**
   on a public pool with shares accepted; **24 h soak** at balanced.
2. **≥ 1 PR merged upstream** into `256foundation/mujina`.
3. **Zero safety incidents**; all safety gates (fan, thermal trip, temp_limit,
   PSU faults) proven on-device.
4. **Docs complete**: flashing guide, board guide, Aura REFERENCE.md,
   deployment guide.
5. **Scope discipline:** no bitcoin-node, ckpool, or stock-UI-parity work
   inside this project (follow-on list in §2).

---

*Technical execution detail (files, tasks, test vectors, byte-level constants): see
[`apollo-iii-integration-plan.md`](apollo-iii-integration-plan.md). The
implementation plan is the task-level companion to this document — this project
plan is the authoritative goals/status/risk view.*
