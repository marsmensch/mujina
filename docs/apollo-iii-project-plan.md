# Mujina on Apollo III — Project Plan

| | |
|---|---|
| **Project** | Run mujina-miner on FutureBit Apollo III as a drop-in replacement for the closed `futurebit-miner-v3` firmware |
| **Version** | 1.0 (2026-08-18) |
| **Sponsor / approver** | mars |
| **Execution lead** | Hermes (this agent) |
| **Repos** | Fork: `marsmensch/mujina` (branch `apollo-iii-integration`) · Source of truth: `apollo-oss-miner` (READ/COPY ONLY until approved) |
| **Upstream** | `256foundation/mujina` (GPL-3.0) |

## 1. Charter

**Objective.** Ship mujina-miner support for the FutureBit Apollo III (21 Auradine
"Aura" ASICs on a Radxa ROCK 5B+ control board) at **≥ vendor hashrate and
efficiency** with **stock web-UI status parity**, and land it **upstream** in the
256 Foundation repo.

**Why now.** The protocol ground truth already exists from the apollo-oss-miner
RE project (vendor blob disassembly, live captures, datasheet, OS image). The
entire mujina-side implementation is doable **without** the drop-in being ready
and **without** hardware — only on-device validation is hardware-gated.

**In scope.** Aura ASIC driver (new chip family — NOT BM13xx), Linux host
backends (sysfs GPIO, i2c-dev, sysfs PWM, gpiochip tach), Apollo III board
composition, deployment on the Apollo OS, documentation, upstream PRs.

**Out of scope (explicit).** apollo-oss-miner drop-in readiness itself (separate
project; a *dependency*, not work here); node/bitcoind/ckpool internals; WiFi
and non-mining chassis features; non-Apollo boards; mujina TOML config
(follows mujina's current env-var conventions).

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

**G5 — Deployment & documentation — ⏳ NOT STARTED**
- 5.1 systemd unit replacing `apollo-miner.service` (root, SIGTERM-graceful stop, `After=ckpool.service`).
- 5.2 Docs: board guide, Aura REFERENCE.md, deployment guide, README update.
- *Acceptance:* unit passes `systemd-analyze verify`; env vars in `mujina-minerd --help`. **M5.**

**G6 — On-device bring-up to full rate — 🔒 BLOCKED (hardware + settled board)**
- 6.1 Cross-compile/build aarch64 `mujina-minerd`; 21/21 discovery.
- 6.2 First accepted share on local ckpool; ramp to full rate per mode preset.
- 6.3 Sustained-efficiency gap ≤ 2.4% vs vendor (A1CTRL derate/floor-hunt is the known lever).
- 6.4 Safety: fan PID, thermal-trip abort, temp_limit, PSU fault monitoring.
- *Acceptance:* ≥ 12.1 TH/s eco, shares accepted, safety gates proven; 24 h soak at balanced. **M6.**

**G7 — Stock UI / status parity — 🔒 BLOCKED (needs G6)**
- 7.1 Write `apollo-miner-3.json` (statVersion 1.3 schema) + `/tmp/fan/*` from mujina telemetry.
- 7.2 Drop-in acceptance: web UI reads live state with `apollo-miner.service` replaced.
- *Acceptance:* stock UI renders live TH/s/temp/fan; zero manual service restores needed. **M7.**

**G8 — Upstream contribution — 🔒 BLOCKED (needs G1/G2 review engagement)**
- 8.1 Maintainer engagement at M1/M2 (Aura protocol facts shared early — BM13xx REFERENCE.md is RE-derived and welcomes this).
- 8.2 Per-surface PRs to `256foundation/mujina` (protocol → chain → backends → board → docs).
- 8.3 Datasheet-provenance gate cleared before upstreaming REFERENCE.md verbatim.
- *Acceptance:* ≥ 1 PR merged upstream; review feedback incorporated. **M8.**

## 3. Workstreams

| WS | Covers | Lead routing |
|---|---|---|
| WS-ASIC | G1, G2 | Implementation: DeepSeek V4 Flash 0731 via `delegate_task` (standing rule). Critical analysis/verification: Kimi K3. NOT Codex CLI / Claude Code. |
| WS-PLATFORM | G3, G4 | Same routing as WS-ASIC. |
| WS-SYSTEM | G5, G7 | Same routing; docs via same delegation. |
| WS-HW | G6 | Hardware-lab discipline per `apollo-oss-miner` RUNBOOK (reboots mandatory; never kill -9). |
| WS-UPSTREAM | G8 | Hermes + mars (maintainer comms; no unsolicited external contact). |

## 4. Dependencies & constraints

| # | Dependency | Effect | Status |
|---|---|---|---|
| D1 | apollo-oss-miner drop-in readiness | Not a blocker for G1–G5; G6 needs only the device + discipline | In progress (separate project) |
| D2 | Live Apollo III + settled board for each experiment | G6 hard-gate; reboot → vendor full rate → graceful stop → settle ≥ 15–18 s per run | Device available |
| D3 | Datasheet holder approval | Gate on upstreaming REFERENCE.md verbatim (G8.3); derived facts publishable regardless | Open — needs mars |
| D4 | 256 Foundation maintainers | G8 engagement; start at M1/M2 | Not engaged yet |
| D5 | Codex OAuth usage limit (until ~Aug 20) | Delegation routing already avoids Codex for this project | No impact |
| D6 | Model API 503 upstream-capacity | Transient; wait 3–5 min, retry same op (standing directive) | N/A |

## 5. Timeline (estimates — hardware-gated items shift on device availability)

| Window | Deliverable | Milestone |
|---|---|---|
| W1–W2 (Aug 18 – Aug 29) | G1 Aura protocol core + test corpus | M1 |
| W2–W4 | G2 chain driver | M2 |
| W3–W4 (parallel) | G3 backends | M3 |
| W4–W5 | G4 board + wiring | M4 |
| W5–W6 | G5 deployment + docs | M5 |
| W6+ | G6 on-device bring-up (hardware window) | M6 |
| W7–W8 | G7 UI parity + drop-in acceptance | M7 |
| Rolling | G8 upstream PRs | M8 |

All estimates ±; plan is execution-velocity-driven, not date-driven. G0 done 2026-08-18.

## 6. Milestones & acceptance

| M | Exit criteria | Verification (no self-reports) |
|---|---|---|
| M1 | Codec/CRC/job/hit green on captured vectors | `cargo test` output; vectors traced to `run_*.bin` |
| M2 | Discovery/init/DVFS/shares green on fake transport | `cargo test`; byte-exact frame assertions |
| M3 | Backends green on mock trees | `cargo test`; captured SIC450 exchange reproduced |
| M4 | Board spawns on env; telemetry flows | `mujina-minerd` run + REST/CLI output; pty-stub smoke |
| M5 | Deployable unit + docs | `systemd-analyze verify`; `--help` lists env vars |
| M6 | ≥ 12.1 TH/s eco; shares accepted; safety proven | On-device numbers vs vendor (12.1–12.23 TH/s baseline) |
| M7 | Stock UI reads live state | UI screenshot/JSON vs vendor schema; 24 h soak |
| M8 | ≥ 1 upstream PR merged | GitHub PR state |

## 7. Risk register

| # | Risk | P | I | Mitigation | Owner |
|---|---|---|---|---|---|
| R1 | Datasheet provenance blocks upstream REFERENCE.md | M | H | Publish derived facts + measured evidence; get mars to clear D3 before verbatim tables | mars |
| R2 | Board wedge / vendor crash-loop during HW work | H | H | Mandatory reboot between experiments; vendor full-rate recovery as board-health gate; never kill -9 | Hermes + RUNBOOK |
| R3 | apollo-oss-miner never reaches production drop-in | L | M | G1–G5 independent; G6 needs device only; mujina is the durable end-state | Hermes |
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
| 2026-08-18 | Datasheet provenance is an explicit gate (D3/R1) | Personal-channel source; confirm before verbatim upstreaming | ⏳ mars |

## 11. Status summary

| Area | Status |
|---|---|
| G0 Governance & intel | ✅ Done |
| G1 Protocol core | ⏳ Not started — doable now |
| G2 Chain driver | ⏳ Not started |
| G3 Backends | ⏳ Not started |
| G4 Board | ⏳ Not started |
| G5 Deployment & docs | ⏳ Not started |
| G6 On-device bring-up | 🔒 Blocked (hardware) |
| G7 UI parity | 🔒 Blocked (needs G6) |
| G8 Upstream | 🔒 Blocked (needs G1/G2) |

## 12. Success criteria (project level)

1. Mujina drives Apollo III at **≥ 12.1 TH/s eco** (vendor baseline 12.1–12.23), shares accepted, 24 h soak.
2. **Stock web UI** reads live miner state with the vendor service replaced.
3. **≥ 1 PR merged upstream** into `256foundation/mujina`.
4. **Zero safety incidents**; all safety gates (fan, thermal trip, temp_limit, PSU faults) proven on-device.
5. **Docs complete**: boot contract, board guide, Aura REFERENCE.md, deployment guide.

---

*Technical execution detail (files, tasks, test vectors, byte-level constants): see
[`apollo-iii-integration-plan.md`](apollo-iii-integration-plan.md). The
implementation plan is the task-level companion to this document — this project
plan is the authoritative goals/status/risk view.*
