# Mujina on Apollo III — Goals

> **What this is.** The project's persistent goal contract, modeled on Hermes
> Agent's **Persistent Goals** system (`/goal`, `/subgoal`, completion
> contracts, quality gates — see
> https://hermes-agent.nousresearch.com/docs/user-guide/features/goals).
> GOALS.md is the durable, reviewable artifact; the `/goal` mechanism is how
> the goal is enforced inside a Hermes session. "Load into Hermes" (§6) gives
> the exact commands. Authoritative project view (scope, risks, decisions,
> status): `docs/apollo-iii-project-plan.md` (v1.1).

---

## 1. Main goal

**Flash a mujina OS image to the Apollo III's microSD card and have the miner
working** — mujina-minerd driving the 21 Auradine "Aura" ASICs at ≥ vendor
hashrate and efficiency, mining to a public pool — and land the Aura ASIC
support upstream in `256foundation/mujina`.

### Completion contract

| Field | Value |
|---|---|
| **outcome** | A flasheable mujina image boots on the FutureBit Apollo III (Radxa ROCK 5B+, 21 Auradine Aura ASICs) and mines to a public pool at **≥ 12.1 TH/s eco** (vendor baseline 12.1–12.23) with accepted shares, sustained over a 24 h soak at balanced mode. At least one Aura-support PR is merged upstream. |
| **verification** | (a) Protocol/chain/backends/board: `cargo test`, `cargo clippy -- -D warnings`, `cargo fmt --check` all green — Aura tests run against captured wire vectors and fake transports, not mocks of the chip. (b) Image: flashes to microSD, boots, `systemd-analyze verify` passes, `mujina-minerd` runs. (c) On-device: REST/CLI reports `ghs ≥ 12.1` eco, shares accepted by the pool, fan/thermal-trip/temp_limit/PSU-fault gates proven; 24 h soak log. (d) Upstream: PR merged, GitHub state verified. |
| **constraints** | Never commit to `main` (fork branch `apollo-iii-integration` only); one commit/PR per logical surface (protocol / chain / backends / board / image / docs); **bitcoin node, ckpool, and stock web-UI/status parity stay out of scope** (follow-on list in project plan §2); `apollo-oss-miner` repo stays read/copy-only until approved; no `kill -9` on the device — reboot-only wedge recovery; datasheet-derived constants are published as derived facts, and the datasheet holder must approve before upstreaming REFERENCE.md tables verbatim. |
| **boundaries** | In scope: `mujina-miner/src/asic/aura/`, `src/board/apollo_iii.rs`, Linux backends under `src/hw_trait/` + `src/peripheral/`, `src/transport/` + `src/backplane.rs` wiring, `src/env_help.rs`, image tooling (`tools/` or `scripts/`), `docs/apollo-iii-*`, `README.md`. Out of scope: bitcoind, ckpool, apolloapi, vendor-OS modification, non-Apollo boards, mujina TOML config. |
| **stop_when** | The live Apollo III is unavailable for G6; the datasheet holder does not approve verbatim REFERENCE.md content and the derived-facts route is insufficient for upstream; 256 Foundation maintainers reject the direction (fall back to fork-only delivery); or a review gate (M1/M2) requires re-scope. |

---

## 2. Subgoals (the way there — ordered)

Each subgoal is one `/subgoal` criterion: the goal is not done until **all** are
met. Hardware-gated items (SG6) are BLOCKED until the device window opens.

| # | Subgoal (criterion) | Outcome | Verification |
|---|---|---|---|
| SG1 | **Aura protocol core lands** | Frame codec, CRC-32, register map, job encoder (92 B flat), hit parser (nonce @ [84:88]) in `asic/aura/` | `cargo test` green against byte vectors extracted from `apollo-oss-miner` `run_*.bin` captures |
| SG2 | **Aura chain driver lands** | Multi-pass discovery (21 chips), chain init, 18-write DVFS InitialSetup, continuous `0x1f00` heartbeat, share handling | Fake-transport tests: 21/21 discovery accumulation, byte-exact DVFS payload, recorded nonce recovered |
| SG3 | **Linux host backends land** | sysfs GPIO, i2c-dev (`I2C_RDWR` combined, PEC off), sysfs PWM (PSU `pwmchip1` / fan `pwmchip0`), fan tach, board temp (0x49) | Mock-sysfs/i2c unit tests green; captured SIC450 exchange reproduced |
| SG4 | **Apollo III board lands** | `board/apollo_iii.rs` env-driven virtual board; bring-up in blob order; board loop (watchdog, DVFS, telemetry, fan PID, thermal trip) | No-hw smoke via pty stub; `mujina-minerd` spawns board on env; telemetry flows |
| SG5 | **Flasheable image lands** | Armbian-based microSD image: `mujina-minerd` + systemd unit + boot exports + public-pool config | Image builds reproducibly, flashes, boots, runs miner (CPU-miner smoke first); `systemd-analyze verify` passes |
| SG6 | **On-device bring-up to full rate** | Flashed image reaches vendor-rate parity on the device | ≥ 12.1 TH/s eco on a public pool, shares accepted, safety gates proven, 24 h soak |
| SG7 | **Upstream contribution lands** | Aura support merged into `256foundation/mujina` | ≥ 1 PR merged (GitHub state); maintainer feedback incorporated |

---

## 3. Quality gates (deterministic — run before "done" is judged)

```text
/goal gate add cargo test -p mujina-miner
/goal gate add cargo clippy -- -D warnings
/goal gate add cargo fmt --check
```

Phase 5 adds: `systemd-analyze verify` on the image's unit file.
Phase 6 (on-device) adds: hashrate assertion `ghs >= 12.1` and a share-accept
assertion read from the pool/mujina status — real numbers, not self-reports.

---

## 4. Status board

| Goal | Status |
|---|---|
| Main goal | In progress (G0 done 2026-08-18) |
| SG1 Aura protocol core | ✅ Done (2026-08-18, `71b1b46`) |
| SG2 Aura chain driver | 🔄 In progress (delegated) |
| SG3 Linux backends | ⏳ Not started |
| SG4 Apollo III board | ⏳ Not started |
| SG5 Flasheable image | ⏳ Not started — skeleton doable now |
| SG6 On-device full rate | 🔒 Blocked (hardware) |
| SG7 Upstream | 🔒 Blocked (needs SG1/SG2 review engagement) |

---

## 5. How Hermes goals work (the model this doc follows)

- **`/goal <text>`** sets a standing objective that survives across turns. After
  each turn a lightweight **judge model** checks whether the goal is satisfied;
  if not, Hermes feeds a continuation prompt back into the same session and
  keeps working — until achieved, paused, cleared, or the turn budget
  (default 20, `goals.max_turns`) runs out.
- **Completion contracts** make judging precise: five optional fields —
  `outcome`, `verification`, `constraints`, `boundaries`, `stop_when` —
  set via `/goal draft` or inline field lines (`verify:`, `constraints:`,
  `boundaries:`, `stop when:`, …). The judge marks `done` only when the
  verification is met with concrete evidence.
- **`/subgoal <text>`** appends a numbered acceptance criterion mid-goal; the
  goal isn't done until the original objective **and every subgoal** are met.
  `/subgoal` (no args) lists them; `/subgoal remove <N>` / `/subgoal clear`
  manage them.
- **Quality gates** (`/goal gate add <command>`) are deterministic shell
  commands that must exit 0 before the judge is even called — mechanical
  "done" checks on top of the prose contract.
- **Persistence:** goals, contracts, subgoals, and gates live in
  `SessionDB.state_meta` and survive `/resume` and compression.
- **Scope note:** `/goal` is single-session. Multi-task, multi-profile work
  belongs on the Kanban board (`hermes kanban create … --goal`); this project
  tracks tasks in the plan docs and delegates per the standing routing rules.

---

## 6. Load into Hermes — paste one by one

Paste each command below in order into a Hermes session (CLI or gateway).
Step 1 starts the loop; each Step 2 line adds one numbered subgoal; Step 3
adds the quality gates. The loop runs until the goal and every subgoal are
judged met, the turn budget pauses it, or you `/goal pause` / `/goal clear`.

**Step 1 — set the main goal (with completion contract):**

```text
/goal Flash a mujina OS image to the Apollo III microSD card and have the miner working

verify: cargo test, cargo clippy -- -D warnings, and cargo fmt --check all green; image flashes and boots (systemd-analyze verify passes); on-device ghs >= 12.1 eco with accepted shares and safety gates proven; at least one Aura-support PR merged upstream

constraints: never commit to main; one commit/PR per logical surface; no bitcoin node, ckpool, or stock UI parity work; apollo-oss-miner stays read/copy-only; no kill -9 on the device

boundaries: mujina-miner/src/asic/aura, src/board/apollo_iii.rs, hw_trait and peripheral backends, transport and backplane wiring, env_help.rs, image tooling, docs/apollo-iii-*, README.md

stop when: device unavailable for on-device bring-up; datasheet holder withholds REFERENCE.md approval; upstream rejects the direction
```

**Step 2 — add each subgoal, one line at a time (paste in order):**

```text
/subgoal SG1: Aura protocol core lands — codec, CRC-32, register map, 92-byte job encoder, hit parser; verified by cargo test against captured wire vectors
```

```text
/subgoal SG2: Aura chain driver lands — multi-pass discovery (21 chips), chain init, DVFS InitialSetup + 0x1f00 heartbeat, share handling; verified by fake-transport tests
```

```text
/subgoal SG3: Linux host backends land — sysfs GPIO, i2c-dev, sysfs PWM, fan tach, board temp; verified by mock-sysfs tests and a captured SIC450 exchange
```

```text
/subgoal SG4: Apollo III board lands — env-driven virtual board, blob-order bring-up, board loop; verified by no-hw smoke
```

```text
/subgoal SG5: Flasheable image lands — Armbian image with mujina-minerd, systemd unit, boot exports, public-pool config; verified by image build, boot, and systemd-analyze verify
```

```text
/subgoal SG6: On-device bring-up to full rate — >= 12.1 TH/s eco on a public pool, shares accepted, safety gates proven, 24h soak
```

```text
/subgoal SG7: Upstream contribution — at least one Aura-support PR merged into 256foundation/mujina
```

**Step 3 — add the quality gates:**

```text
/goal gate add cargo test -p mujina-miner
/goal gate add cargo clippy -- -D warnings
/goal gate add cargo fmt --check
```

**Step 4 — control the loop (as needed):**

```text
/goal status
/goal show
/goal pause
/goal resume
/goal clear
```
