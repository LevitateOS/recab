# Untangle log

## Scope

- **Target:** the `recab` repository (`tools/recab`), all of it: `src/`, `src/services/`, `tests/`, `docs/`, `Cargo.toml`, `README.md`, `REQUIREMENTS.md`, `CLAUDE.md`, `AGENTS.md`.
- **Links across the boundary count as findings** (code elsewhere that copies or depends on recab's facts): `testing/install-tests`, `tools/recstrap` (`recsetup`, `target-root`), `tools/recpart`, `tools/recinit`, `distro/profile`, `distro/builder/recipes/install-tools.rhai`, `xtask/src/tasks/generate.rs`, `tui/apps/runtime/recab-tui`. Their own internals are out of scope.
- **Additions (Comments, Tests):** none. (Asked 2026-11-08; "none" was not offered, so the user picked "something else" and confirmed "none" in the redesign request.) The refactor does neither.
- **Relation to the superproject log:** `../../UNTANGLE.md` (the whole-LevitateOS untangle, G01–G32) also covered recab; its recab fixes (G04, G17–G20, G30) have landed. This log is recab-only and starts from recab as it is now.
- **Earlier input reused as evidence, not as findings:** `docs/recab-ideal-filetree/findings.md` in the superproject (same commit, 2026-11-08). Entries from it that fit this checklist are re-listed below in change-together terms, re-checked against the code.

## Baseline

- **Commit:** recab `main` `d4365a9e582310a5bb8fc33f6d1d5889f1e7c97c` ("G30: stage refuses an image of the other flavour"), clean. Superproject `master` `eea341c6` pins it.
- **Build and test:** `cargo test -p recab` from the superproject. Result: `tests/lifecycle.rs` 20 passed, 0 failed, 0 ignored; no unit tests, no doc tests. Full list: `.artifacts/untangle-recab/baseline/test.txt` (superproject).
- **Generated contract:** `cargo xtask gen --check` → "generated files are current".
- **CLI:** `--help` of `recab` and each subcommand saved in `.artifacts/untangle-recab/baseline/help.txt`. On this build host (not an A/B system, `/boot` unreadable to the user) `recab status` exits 1, unclassified: `recab: '/boot/loader/entries/rocky-a.conf': Permission denied (os error 13)`; `status --json` prints `{"schema_version":1,"class":null,"code":1,…}`.
- **VM behaviour:** README records a seven-boot `cargo xtask test ab-update levitate|acorn` pass on 2026-11-08 (not rerun for this baseline). It is the only evidence for `stage`'s write path and the carry.

## Map

Scripts (rerun them in later phases) are in the superproject at `.artifacts/untangle-recab/`: `skeleton.py` (→ `skeleton.rs`, 570 lines for 5.0k of code), `deps.py`, `cochange.sh`, `cross-cochange.sh`.

**Areas**

| Area | Files | Lines | What it is |
|---|---|---:|---|
| Command surface | `main.rs`, `lib.rs`, `config.rs`, `error.rs`, `contract.rs` | 1019 | CLI, public API, the `System` view and `change()` transaction, `trial()` and `stage()` orchestration, env config, exit codes, contract export |
| State machine | `state.rs` | 333 | pure planning of set-next/commit/rollback/stage and trial verdicts |
| Boot selection and ESP | `systemd_boot.rs` | 955 | EFI variables, loader.conf, entry text, root/slot detection, slot kernel dirs, loader install, status views |
| Persistence | `store.rs` | 163 | state file, audit log, lock, durable-write wrappers |
| Writing systems | `init.rs`, `stage.rs`, `carried.rs`, `services/carried` | 838 | install-time layout; staging a slot; carrying machine state with a baseline |
| Trial and packaging | `init_system.rs`, `package.rs`, `services/recab-trial.*` | 396 | startup health on systemd/OpenRC, reboot, trial service, `install-unit` |
| Verification and docs | `tests/lifecycle.rs`, `README.md`, `REQUIREMENTS.md`, `docs/*.schema.json` | 1819 | scratch-tree CLI tests; operator contract; generated schemas |

**Dependency graph** (`deps.py`): no cycles between modules except through `lib.rs`. `lib.rs` uses every module and every module imports crate-root items from it (`Slot`, `TrialRecord`, `StageRequest`, `InitRequest`, `tagged_err`, `ErrorClass`), so `lib.rs` is both the top and a shared leaf. `systemd_boot` imports `init_system` (for `crash_option`) and `store` (for durable-write helpers). `state` (pure) imports `TrialRecord` and `Slot` from `lib.rs`, the most volatile file.

**Change history** (`cochange.sh`; 50 commits, many of them large rewrites, so pairs are weak evidence):

- Churn: `lib.rs` 27, `README.md` 23, `REQUIREMENTS.md` 17, `main.rs` 16, `stage.rs` 14, `tests/lifecycle.rs` 13, `systemd_boot.rs` 9, `init.rs` 8; `state.rs` only 5.
- Co-change: `lib.rs`+`main.rs` 15, `README.md`+`lib.rs` 12, `stage.rs`+`lifecycle.rs` 10, `lib.rs`+`stage.rs` 10, `lib.rs`+`systemd_boot.rs` 9, `init.rs`+`stage.rs` 8, `stage.rs`+`systemd_boot.rs` 8.
- Across the boundary (`cross-cochange.sh`): 45 superproject commits moved the recab pointer, together with `testing/install-tests` (21), `docs/content` (20), `distro/variants` (19), `tools/recstrap` (16), `tools/recpart` (16), `tui` (13). These are batch commits, so this shows which neighbours share recab's facts, not exact pairs.

**Hotspots and leverage**

- `lib.rs`: highest churn, co-changes with almost everything, holds public types, orchestration of `stage` and `trial`, and the transaction. Most change goes through it.
- `systemd_boot.rs`: the largest file, seven duties; its pure text helpers are the vocabulary `init` and `stage` write with.
- `stage.rs` + carry: fastest-moving behaviour (baseline merge and flavour check landed today), proven only in the VM.
- The boundary: recab's facts (entry names, ESP paths, state path, planning rules, exit codes, EROFS check, init argv) are restated in install-tests, recsetup, recab-tui and docs/content.

Start looking at `lib.rs`, the stage/carry path, and the boundary copies.

## Problems

All 334 problems (195 hidden, 43 needless, 26 surplus, 70 special) are filed under `## Fixes`, each under exactly one fix or under *Leave as is*. Numbers (P001…) are stable references.

## Fixes

### Requirements the special cases reveal

1. **recab looks at one system per run, named once.** Every path, program and fact it reads (root, ESP, state, os-release, efivars, `/proc`, stage directory, mkfs/mount/reboot, profile, entry prefix) belongs to the system it is looking at: the running one, or one installed under a root, or a scratch tree. Scratch trees must be that same value, not branches in production code (P076, P077, P078, P137, P138, P187, P236).
2. **Whether a machine is an A/B system is a fact, not an inference.** Live and mutable images run the trial service too; today "no state file" stands in for "not A/B" (P071, P125, P130, P279, P302, P321).
3. **A trial is one lifecycle:** scheduled (state) → booted as a one-shot (firmware) → judged once → committed or failed. Each step is a planned transition, and the boot selection is written before the state at every step, including init and a failed trial (P067–P070, P126–P133, P232, P272).
4. **A mutating command decides every refusal before its first write,** and a failure after the first write says exactly what changed (P073, P084, P186, P189, P191, P192).
5. **Every slot knows what its own image shipped,** so an update carries only the machine's changes, never escapes the slot, and keeps mode, owner and accounts consistent (P183–P185, P188, P190, P273).
6. **Boot entries are written only by recab and read by the same rule;** recab accepts the forms it writes (P134, P135, P136, P141, P142).
7. **Errors carry their class from where they arise;** nothing is reclassified or swallowed by accident, and the class never travels only in text (P070, P074, P075, P079, P081, P235, P237, P238).
8. **The init system is one adapter:** how startup ends, what failed, how to reboot, the crash option, the service and how it is enabled, and logging (P233, P234, P236–P239, P306).

### Review status and decisions

Fixes written 2026-11-08 and reviewed with the user the same day; nothing is applied yet. The user chose the recommended answer for every decision below (answer in parentheses):

- **D-3 (F09):** `status --json` carries recab's verdict for each action, and recab-tui renders it instead of re-deriving the rules. (Yes.)
- **D-4 (F07):** what an update keeps: `/root` (give it the same `/var` link as homes) and which further `/etc` paths to carry (the systemd/OpenRC config directories listed in P157). (Link `/root`; add the listed directories.)
- **D-5 (F03):** when `recab init` runs on a machine that is not the one that will boot the target (installing from an installed system), it does not rewrite this machine's firmware variables. (Skip the reset and say so.)
- **D-6 (F07):** a slot's baseline is written when the image is built, so slot A has one from the start. (Image build.)
- **Scope across repositories:** F07, F09, F10, F11 and F13 change `tui/apps/runtime/recab-tui`, `testing/install-tests`, `tools/recstrap` (recsetup), `distro/builder` and `docs/content`, each committed in its own repository. F11 renames `install-unit --package` to `--init` and updates its one caller (`install-tools.rhai:111`) in the same change.

### Fix list

| Order | Fix | Problems | Special cases | Depends on |
|---:|---|---:|---:|---|
| 1 | F12 Layering and the public surface | 28 | 0 | nothing |
| 2 | F08 One error model | 17 | 7 | nothing (lands beside F12) |
| 3 | F01 The system recab looks at (`Target`) | 44 | 10 | F12 |
| 4 | F02 One owner of the boot partition | 12 | 2 | F01 |
| 5 | F03 Boot selection through one operation | 24 | 4 | F01, F02 |
| 6 | F11 The init-system adapter | 33 | 5 | F01 |
| 7 | F04 One trial lifecycle in the state machine | 38 | 16 | F03, F11 |
| 8 | F05 One trial-options policy | 9 | 1 | F02, F11 |
| 9 | F06 Stage: a check phase, then one write phase | 26 | 4 | F01, F02, F03, F08 |
| 10 | F07 The image declares its machine state; every slot carries its own baseline | 25 | 6 | F06; decisions D-4, D-6 |
| 11 | F09 recab publishes its decisions; front ends render them | 16 | 1 | F04, F08; decision D-3 |
| 12 | F10 Other tools take recab's facts from recab | 22 | 0 | F02, F09 |
| 13 | F13 Tests and docs follow the product | 24 | 3 | all others |
| — | Leave as is | 16 | 11 | — |

### F12 Layering and the public surface

Leaf types (`Slot`, `TrialRecord`, `Startup`) live in the pure state module and depend on nothing above them; `lib.rs` is only the facade; unused `pub` items, pass-through wrappers (store's file helpers, `InitSystemName`) and clap derives on domain types go; the CLI takes recab's own `InitLayout`, and text output is derived from the same structs as JSON.

- **Done when:** `state.rs` imports nothing from `lib.rs`; `deps.py` shows no module importing a crate-root item; every `pub` item has an outside user or is `pub(crate)`; no pass-through wrapper is left in `store.rs`; `main.rs` derives text output from the same structs as JSON.
- **Depends on:** nothing
- **Problems (24):**
  - P002 [hidden] `src/main.rs:153-157` ends in `unreachable!`, true only because of the clap attributes at `main.rs:68-75`; change an attribute and it panics at runtime.
  - P003 [hidden] `src/main.rs:45,48,51` spell `required_if_eq("mode", "ab"|"mutable")` as literals that must match `recpart::types::InstallMode`'s names (`tools/recpart/src/types.rs:17-23`).
  - P004 [hidden] `src/main.rs:161-165` re-checks the mode/slot combinations clap already enforces (`main.rs:45-52`), with an unclassified `bail!` (exit 1): two copies of one rule.
  - P007 [hidden] `src/main.rs:194-216` `print_status` lists `Status` fields (`lib.rs:133-152`) one by one and silently omits `slots`; text and JSON already differ.
  - P009 [hidden] `src/lib.rs:57-95` spells a slot three ways (`#[value(name = "A")]`, `serde(rename_all = "UPPERCASE")`, `as_letter()`); `src/contract.rs:18` exports the clap spelling as `SLOTS`, the JSON uses serde's.
  - P042 [needless] `src/state.rs:9`, `src/store.rs:16`, `src/systemd_boot.rs:20` import `Slot`/`TrialRecord` from the crate root, and `store.rs:119` calls `crate::now()`: the I/O-free state machine and the store depend on `lib.rs`, the most volatile file, which imports them back (`lib.rs:48-51`).
  - P043 [needless] `src/lib.rs:98-108` `TrialRecord` is a field of `PersistentState` (`state.rs:31`) but lives in `lib.rs` beside the CLI output types.
  - P044 [needless] `src/systemd_boot.rs:620-641` builds `crate::SlotView`, the public `status --json` type: the boot backend is tied to the output contract.
  - P045 [needless] `src/lib.rs:56-58` the domain type `Slot` derives `clap::ValueEnum`, so the library and every consumer (install-tests, xtask) depend on the CLI framework; `contract.rs:18` relies on it.
  - P046 [needless] `src/main.rs:9` parses `recpart::types::InstallMode`, then converts it (`:161-165`) into recab's own `InitLayout` (`lib.rs:173-180`): two types for one choice, and the CLI depends on the partitioner's type.
  - P049 [needless] `src/store.rs:148-163` make the state store the home of generic file helpers, so `systemd_boot.rs:18`, `init.rs:19`, `stage.rs:19` import "the store" for plain file I/O.
  - P053 [needless] `src/lib.rs:15` `pub mod carried`, and `lib.rs:43-44` `crash_option`, `RECAB_BIN`, are public only for `tests/lifecycle.rs:279,626,633,859,864`.
  - P056 [needless] `src/init_system.rs:88-101` `InitSystemName` copies `distro_profile::InitSystem` (re-exported `lib.rs:42`) only to attach contract derives; it must track every init added upstream.
  - P058 [surplus] `src/lib.rs:76-81` `Slot::as_letter` is public; its only user is `Display` (`:94`).
  - P059 [surplus] `src/lib.rs:34` re-exports `layout_entry_name`, `INITRD_FILE`, `KERNEL_FILE`, `LOADER_CONF` with no caller outside the crate.
  - P060 [surplus] `src/lib.rs:39-40`, `src/error.rs:7-12` the public `EXIT_CODE_*` constants are read only by `ErrorClass::code`; `error_class` (`error.rs:86`) is public with no outside caller.
  - P061 [surplus] `src/store.rs:19-20` `Store.state_path`, `audit_path` are `pub`, read only inside `store.rs`.
  - P062 [surplus] `src/store.rs:70-72,148-163` pass-through wrappers around `target_root::{exists, read_optional, write_durable, copy_durable, sync_dir}`.
  - P066 [surplus] `src/contract.rs:19` `ERROR_SCHEMA_VERSION` exported with an empty description.
  - P105 [hidden] `STATE_SCHEMA_VERSION` (`src/state.rs:14`) is restated as the literal `state.schema < 1` (`src/store.rs:88`).
  - P122 [surplus] `SYSTEMD_BOOT_SOURCE_DIR` is `pub` (`src/systemd_boot.rs:35`) and `PANIC_OPTION` `pub(crate)` (`:43`), both used only in `systemd_boot.rs`.
  - P259 [hidden] `src/stage.rs:19` writes through the store wrapper `write_durable`, while `:394,518,521,534` call `target_root::read_optional`/`write_durable` directly: wrapper and wrapped both in use.
  - P281 [hidden] `AuditEvent` (`src/store.rs:28-37`) embeds the whole `before`/`after` `PersistentState`, so every state-schema change changes the audit record, though `store.rs:13-15` says the audit schema is "versioned apart".
  - P334 [needless] `Startup` (`src/state.rs:263-273`) is defined in init vocabulary (systemd maintenance, OpenRC single-user) inside the pure state machine; `init_system.rs` imports it.

### F08 One error model

One `refuse`/classify helper in `error.rs` used by every module; every refusal and every post-write failure classified; the class table has one source; `ErrorPayload.message` without the tag; `--json` errors for every command.

- **Done when:** `grep tagged_err` finds one helper used everywhere; no refusal exits 1; `ErrorPayload.message` has no `[TAG]`; every command honours `--json` for errors; the class table is defined once.
- **Depends on:** nothing (lands beside F12)
- **Problems (14):**
  - P001 [hidden] `src/main.rs:133` lists by hand which commands print errors as JSON (`Status{json:true}`, `Health{json:true}`); a new `--json` command must be added there too, and nothing says so.
  - P011 [hidden] `tagged_err` is reached two ways: `crate::tagged_err` through a private `use` at `src/lib.rs:46` (`init.rs:24`, `init_system.rs:20`, `stage.rs:21`) and `crate::error::tagged_err` (`config.rs:12`, `state.rs:8`, `store.rs:10`, `systemd_boot.rs:15`).
  - P028 [hidden] `src/error.rs:30-58`: `ErrorClass::ALL`, `code()` and `tag()` are three hand-kept lists of the same variants.
  - P029 [hidden] `src/error.rs:71` puts `[TAG]` in `Display`, so `ErrorPayload.message` (`error.rs:115`) repeats the class inside the text front ends are told never to read (`error.rs:1-3`).
  - P075 [special] `src/store.rs:87-90` `schema < 1` rejected: the only schema check, in the store rather than the state module.
  - P079 [special] `src/main.rs:137-139` if `ErrorPayload` fails to serialize under `--json`, the error silently falls back to text.
  - P081 [special] `src/contract.rs:23` a double `unwrap()` on each class's serde name.
  - P084 [special] Baseline: `recab status` on a host whose `/boot` is unreadable exits 1, unclassified (`'/boot/loader/entries/rocky-a.conf': Permission denied`): an unreadable ESP is neither a state/detection error (21) nor a permission error (13).
  - P149 [hidden] "Only NotFound means absent" is restated at `src/stage.rs:287-293` (`canonical`), `:443-450` (`metadata`), `:462-466` (in `copy_tree`), though `target-root/src/lib.rs:1-8,32-39` says it is implemented once; `stage.rs:133-142` uses `Path::is_file`, which turns an I/O error into "not a file".
  - P235 [special] `src/init_system.rs:236-243` `rc-status --crashed`'s exit status ignored, stderr discarded; `:258` `ESRCH` swallowed for a process that exited during the scan.
  - P257 [hidden] The "NotFound → None" probe is also copied at `src/systemd_boot.rs:882-886` (`RootSelector::device`) and `:489-493` (`loader_efivar`), beside `target_root::exists`/`read_optional`.
  - P258 [hidden] `refuse()` at `src/stage.rs:110-112` copies `src/state.rs:157-159`; `systemd_boot.rs:335` (`state_err`) and many inline `tagged_err(ErrorClass::StateOrDetection, …)` repeat the same refusal idea.
  - P289 [hidden] A failed efivar write is tagged `StateOrDetection` (exit 21) in `reset_stale_selection` (`src/systemd_boot.rs:187-196`) but untagged (exit 1) inside `apply` (`:244,250`).
  - P326 [hidden] Some refusals are untagged (exit 1) while their siblings are classified: `src/init.rs:39` ("slot A and slot B are the same partition"), `src/stage.rs:308`, `src/carried.rs:37`, `src/main.rs:164`; error classes are chosen in every module (`config.rs:70`, `store.rs:77-80,97-104`, `init.rs:112-119`, `init_system.rs:23-50`, `systemd_boot.rs:222,290,424,516,548,584,609`, `stage.rs:110`, `lib.rs:452,507-526`).

### F01 The system recab looks at (`Target`)

One value, built once, for the running system, an install target or a scratch tree: every path and program, the efivar store, `/proc`, the stage directory, and the facts read once from it (os-release by one rule → entry prefix, init, flavour; the root partition by PARTUUID). Every module takes it; nothing reads `/`, `/proc` or the environment itself. Whether the system is A/B is a field of it.

- **Done when:** No module outside the `Target` reads `std::env`, `/`, `/proc`, `/etc/os-release` or names a program; os-release is read once per run by one rule; scratch trees need no production branch; whether the system is A/B is a `Target` field.
- **Depends on:** F12
- **Problems (38):**
  - P231 [surplus] `src/init_system.rs:270-276` the `openrc-init` → `openrc-shutdown -r now` branch: AcornOS's PID 1 is busybox init from an inittab (`installed-system.rhai:227,245`), and nothing in the recipes uses `openrc-init`.
  - P016 [hidden] `src/lib.rs:301,343,435,500` re-read os-release through `init_system::running_profile` mid-operation, though `System` claims the run's view is read once (`lib.rs:204-206`).
  - P024 [hidden] `src/config.rs:19-21` (`state_file`, a String from `STATE_MOUNT`) and `:121-124` (`state_files`, PathBufs via `under()`) compute the same path separately; `"state.json"` appears at `:20` and `:123`.
  - P025 [hidden] `src/config.rs:85,109` each hard-code `run/lock/recab.lock`, and `:78,105` `etc/os-release`, outside `under()` and the shared constants.
  - P026 [hidden] `src/config.rs:104` `install_target` drops `RECAB_ENTRY_PREFIX`, and `init.rs:31-32` names entries from the target's `ID`: init and a running recab with the override name entries by different rules.
  - P027 [hidden] `src/init.rs:31,103-105` read the target's os-release with `target_root::os_release(root)`, ignoring `config.os_release`, which `config.rs:105` sets for exactly this.
  - P047 [needless] `src/config.rs:33-60` `Config` is one flat bag of every module's settings (systemctl/rc_status for init_system, recstrap/stage_dir for stage, efivars for systemd_boot, store paths); a setting for one module changes the type all take.
  - P048 [needless] `src/config.rs:24-31` `RunningRoot`, a behaviour choice only `systemd_boot` uses, lives in config.
  - P063 [surplus] `src/config.rs:109` `install_target` sets `lock_path` under the target, but neither user locks (`status --root`, `init`).
  - P064 [surplus] `src/config.rs:92` `RECAB_RECSTRAP_BIN` and `:68-73` `RECAB_ENTRY_PREFIX` are set by no caller or test (and init ignores the prefix).
  - P072 [special] `src/lib.rs:278` `init` calls `require_root` itself and bypasses `System`, so it takes no lock though it writes recab's state (`init.rs:81`).
  - P076 [special] `src/config.rs:81` whether the variables are real efivarfs is inferred from whether the test override `RECAB_EFIVARS_PATH` is set: production behaviour changes for tests (`systemd_boot.rs:467-469`).
  - P077 [special] `src/config.rs:69-71` an `if` for one value (`RECAB_ENTRY_PREFIX` set but blank).
  - P078 [special] `src/config.rs:86` `stage_dir` is the one path with no `RECAB_*` override, against the rule at `config.rs:5-7`.
  - P092 [hidden] `parse_entry_name` (`src/systemd_boot.rs:725`) lower-cases names, `entry_prefix` (`:602-603`) returns `RECAB_ENTRY_PREFIX` as given, so `:130`, `:185` silently never match a mixed-case override.
  - P093 [hidden] The entry prefix is read from os-release three ways: `SystemdBoot::entry_prefix` (`src/systemd_boot.rs:601-617`, `etc/os-release` only), `init::os_identity` (`src/init.rs:103-110`, `target_root::os_release` with the `usr/lib` fallback), and recsetup (`tools/recstrap/src/bin/recsetup/target.rs:165-168`, lower-cased, "linux" fallback); README.md:47-48 and `systemd_boot.rs:598-599` say "the same rule".
  - P107 [hidden] `status --root` reads the target's ESP and state but this host's efivars (`src/config.rs:96-99`, `systemd_boot.rs:123,143`): its default and one-shot come from whatever machine runs it.
  - P120 [needless] `SystemdBoot::new` (`src/systemd_boot.rs:106-118`) copies nine `Config` fields into a parallel struct; each new path means editing both.
  - P138 [special] `clear_immutable` treats ENOTTY/EINVAL/EOPNOTSUPP as success (`src/systemd_boot.rs:942-945`), for the scratch tree.
  - P152 [hidden] `src/stage.rs:239,269` hard-code the running root as `/` and `/etc/fstab`, and `:196` reads `metadata("/")`; `config.rs:1-7` says Config decides every path, and `check_flavour` reads `config.os_release` (`stage.rs:79`): two notions of "the running system" in one stage.
  - P166 [hidden] `src/init.rs:112-119`, `src/stage.rs:217-222`, `recsetup/plan.rs:500-509` are three wrappers around `target_root::partuuid`, each with its own "needs a GPT partition" error.
  - P167 [hidden] "slot A is the partition mounted at root" is checked only in recsetup (`plan.rs:417-428`), not in `recab init`, which writes the record; `init` also never checks `boot/`, `var/` are mount points.
  - P175 [needless] `src/stage.rs:251` names the stage mount point with `Slot::entry_suffix()`, the boot-entry naming helper.
  - P182 [surplus] `config.rs:49-50,86` `stage_dir` has one fixed value; its three mount points (`stage.rs:45,84,251`) are named in three places and never removed.
  - P187 [special] `src/stage.rs:192-199` falls back to `metadata("/").dev()` for a rootfs partition mounted as `/` but absent by source from mountinfo.
  - P199 [hidden] `running_profile` (`src/init_system.rs:23-37`, exit 20) and `installed_profile` (`:40-53`, exit 21) read the same profile with different classes; `installed_profile` bypasses `Config::install_target`'s `os_release` (`config.rs:105`).
  - P200 [hidden] `src/init_system.rs:248-263` (`/proc` scan) and `:273` (`/proc/1/comm`) are hard-coded; the OpenRC lifecycle test (`tests/lifecycle.rs:778`) depends on the build host running no `openrc` process.
  - P230 [surplus] `src/lib.rs:443-446` `install_unit`'s fallback to the root's os-release when `--package` is absent; the one production caller always passes it (`install-tools.rhai:111`).
  - P236 [special] `src/init_system.rs:267-268` `RECAB_REBOOT_BIN` replaces the program and drops its arguments, for the test fixture (`tests/lifecycle.rs:782`).
  - P246 [hidden] `Store::load` (`src/store.rs:74-92`) returns `PersistentState::default()` for a missing file, so "missing" vs "empty" is rebuilt by a second `exists()` call (`src/lib.rs:347,371`).
  - P261 [hidden] `init`'s `os_identity` uses `target_root::os_release(root)` (falls back to `usr/lib/os-release`, resolves absolute links inside the root) while the reader (`src/systemd_boot.rs:601-617`) reads `root/etc/os-release` plainly (`config.rs:105`): a target whose `/etc/os-release` is missing or an absolute link gets entries `status --root` cannot find, or the host's ID.
  - P262 [hidden] `check_flavour` reads the running flavour through `config.os_release` (`src/stage.rs:79`) and the image's through `target_root::os_release` (`:87`); `running_profile`/`installed_profile` (`init_system.rs:23,40-41`) split the same way.
  - P270 [needless] `init::run(config, request)` (`src/lib.rs:279`, `init.rs:28-29`) receives the root twice: inside `config.install_target(&request.root)` and as `request.root`.
  - P299 [needless] `RunningRoot::Mounted` (`src/config.rs:24-31`) carries the install-target workflow into the general backend, which branches on it (`systemd_boot.rs:512-540`).
  - P312 [hidden] Under Ginux, `testing/ginux-boot/stage.sh:17-37` fakes a `/proc` without process entries, so `openrc_running` (`src/init_system.rs:248-263`) always answers false there.
  - P323 [hidden] Programs bypass `Config` too: `mkfs.*`, `mount`, `umount` (`src/stage.rs:246,576,587,599,608`), `openrc-shutdown`, `reboot` (`src/init_system.rs:275,277`), while `systemctl`, `rc-status`, `recstrap` are configurable.
  - P324 [hidden] "The running root" is worked out three ways: `src/systemd_boot.rs:512-540` (cmdline `root=` or the mount at an install target), `src/stage.rs:196-197` (device number of `/`), `src/stage.rs:239` (literal `/`).
  - P325 [hidden] One `stage` run reads the running os-release three times with three error classes: `init_system.rs:23-37` (BackendUnavailable, via `lib.rs:301`), `stage.rs:79-82` (untagged or StateOrDetection), `systemd_boot.rs:605-616` (StateOrDetection).

### F02 One owner of the boot partition

An `esp` module with one layout API (prefix + slot → entry, kernel dir, files), one typed `Entry` (parse once, render once), one kernel install (remove, write under temporary names, rewrite the entry, rename), loader install and loader.conf. `init` and `stage` call it and write no ESP file themselves; recab reads only the `root=PARTUUID=` entries it writes.

- **Done when:** Only the `esp` module writes or parses entries, kernels, loader.conf and the loader; `init.rs` and `stage.rs` import no layout constant; one `Entry` type; kernel install leaves no window where a half-written slot is bootable.
- **Depends on:** F01
- **Problems (10):**
  - P010 [hidden] `src/lib.rs:84-89` `Slot::entry_suffix` holds the ESP naming suffix at the crate root; the naming rules using it live in `systemd_boot.rs:652-668,728`, and `stage.rs:251` reuses it for a mount-point name.
  - P089 [hidden] The entry-file grammar is parsed in seven places with different rules: `trial_options` `:54`, `entry_boot_files` `:764`, `entry_with_boot_files` `:778`, `entry_with_options` `:816` (first token); `entry_options` `:804` (trim + strip_prefix); `root_selector_from_entry_content` `:922` (also skips `#`); `loader_default` `:736` vs `loader_with_default` `:749` match `default` differently.
  - P090 [hidden] `new_entry` (`src/systemd_boot.rs:680-683`) and `install_slot_boot_files` (`:401-405`) each format the `linux`/`initrd` lines; `src/init.rs:54-59` repeats the kernel/initrd copy `install_slot_boot_files` does (`:394-400`).
  - P097 [hidden] "A slot is unbootable while it is written" is held only by `write_slot` calling `remove_slot_boot_files` first (`src/stage.rs:241`) and `install_slot_boot_files` last (`:277`); the backend exposes three steps and `slot_boot_files_dir` (`systemd_boot.rs:365`) for `check_inputs` (`stage.rs:144`).
  - P099 [hidden] `slot_device` (`src/systemd_boot.rs:333-361`) parses the entry's `root=PARTUUID=` but returns only the device; `stage::check_target` reads the PARTUUID again with `target_root::partuuid` (`src/stage.rs:217`).
  - P121 [surplus] `slot_boot_dir` (`src/systemd_boot.rs:671-673`) and `entry_name` (`:666-668`) only call `layout_name`/`layout_entry_name` with `Some(slot)`: three public names for one naming rule.
  - P136 [special] `RootSelector::parse` (`src/systemd_boot.rs:850-872`) accepts `UUID=`, `LABEL=`, `/dev/disk/by-*`, quotes and bare paths, and `names_same_root` (`:897-899`) runs blkid on both sides, though recab writes only `root=PARTUUID=` (`:684`) and `slot_device` refuses anything else (`:340`).
  - P168 [hidden] `src/init.rs:48` takes `entries[0]` as the installed slot by position; `:77-78` hard-code `Slot::A` again.
  - P176 [needless] `src/init.rs:20-23` imports the ESP layout (`ENTRIES_DIR`, `KERNEL_FILE`, `INITRD_FILE`, `LOADER_CONF`, `layout_name`, `install_loader`, `new_entry`, `loader_with_default`) and writes the ESP itself, though `lib.rs:11-12` and README say `systemd_boot` owns it.
  - P278 [hidden] `src/init.rs:54-59` copies the kernel and initrd without removing the directory first, unlike stage (`systemd_boot.rs:371-379`): re-running `init` without `--initrd` leaves a stale `initramfs.img`.

### F03 Boot selection through one operation

Firmware variables as a typed store (variable enum, one encode/decode, efivarfs or file store chosen by the `Target`); the selection read once with raw values, including a default naming a non-slot entry; one `select(change)` that checks its own preconditions (can change, bootable, stale values) and reports exactly what it changed. `init` uses it.

- **Done when:** Variable names exist once as an enum; one encode/decode; callers call one `select` and no precondition by hand; `init` changes the selection through it; error outcomes say exactly what changed.
- **Depends on:** F01, F02
- **Problems (21):**
  - P013 [hidden] `src/lib.rs:305,309` and `src/lib.rs:491-496` repeat the pair `ensure_can_change` → `reset_stale_selection` in `stage` and `change()`, untied.
  - P085 [hidden] `src/systemd_boot.rs:157-171` (`firmware_trial`) and `:120-140` (`read`) each work out "the default entry" their own way: `read` filters by prefix and skips unbootable entries, `firmware_trial` compares raw strings of any prefix.
  - P086 [hidden] `src/systemd_boot.rs:153-155` says `firmware_trial` needs neither ESP nor `/var`, but its fallback (`:165`) reads loader.conf on the ESP; it works without the ESP only because `init.rs:89` and `apply` (`:270`) always set `LoaderEntryDefault`, which nothing states.
  - P087 [hidden] The variable names `LoaderEntryDefault`/`OneShot`/`Selected` are literals at `src/systemd_boot.rs:123,143,159,163,200,207,244,249,250,270` and `src/init.rs:89-90`.
  - P095 [hidden] `ensure_can_change` (`src/systemd_boot.rs:216-232`) and `efivars_available` (`:443-445`) test the same two directories, one as an error, one as a bool (`init.rs:87` vs `lib.rs:305,491`).
  - P096 [hidden] Callers must call `ensure_bootable` → `ensure_can_change` → `reset_stale_selection` → `apply` in order (`src/lib.rs:484-501`); `apply` (`systemd_boot.rs:238`) checks none of these itself.
  - P098 [hidden] `ApplyError::Untouched` ("Nothing changed", `src/systemd_boot.rs:97`) is returned after the entry was rewritten (`:243-244`, `:264-269`), and `lib.rs:504` then says "the boot selection is unchanged" though `reset_stale_selection` (`lib.rs:495`) may have rewritten `LoaderEntryDefault`.
  - P114 [needless] `SystemdBoot::apply` takes `init: InitSystem` (`src/systemd_boot.rs:238`), used only by `OneShot`; so `change()` reads the profile for every commit and rollback (`lib.rs:500`), and a missing distro profile can block a commit.
  - P115 [needless] `reset_stale_selection(boot, keep, slot)` (`src/systemd_boot.rs:181-212`) serves stage (running, staged) and set-next (default, trial); its one-shot branch (`:206`) can fire only for stage, and its error text (`:192`, "Slot {slot} was not changed") is stage's wording shown for set-next.
  - P117 [needless] `src/init.rs:86-91` calls `set_loader_efivar` with raw variable names, bypassing `BootChange`/`apply`; that is why `set_loader_efivar` is `pub(crate)` (`systemd_boot.rs:451`).
  - P118 [needless] `Boot` (`src/systemd_boot.rs:84-93`) is handed back to almost every backend method mostly for `boot.prefix`; its `raw_default`/`raw_oneshot` are `pub` but used only inside the backend.
  - P137 [special] `set_loader_efivar`'s `!self.efivarfs` branch (`src/systemd_boot.rs:467-469`) writes plain files only for the scratch tree.
  - P141 [special] `read()` drops a `LoaderEntryDefault` that names an existing non-slot entry and falls back to loader.conf (`src/systemd_boot.rs:127-139`), though systemd-boot boots that entry; `reset_stale_selection` then rewrites it, which changes what boots.
  - P169 [hidden] `src/init.rs:93-96`'s warning restates stage's behaviour ("'recab stage' resets them before it writes slot B") and prints for the mutable layout too.
  - P170 [hidden] `src/init.rs:86-91` resets this machine's firmware variables to the target's entry name; installing from an installed system rewrites the host's own default.
  - P251 [hidden] `reset_stale_selection` gets `keep = active` from stage (`src/lib.rs:309`) and `keep = selection.default` from change (`:494-495`); both are right only because `plan`/`plan_stage` already checked the slot is the known-good default (`state.rs:173-184,216-223`).
  - P276 [hidden] `src/init.rs:81` saves recab's state before `:86-91` writes the firmware variables: the reverse of "boot selection first, state second" (README.md:176, REQUIREMENTS REQ-PERSIST-005), which `lib.rs::change` follows.
  - P277 [hidden] `src/init.rs:69-73,86-91` re-implements `apply`'s `BootChange::Default` (`src/systemd_boot.rs:246-281`: loader.conf default, `LoaderEntryDefault`, clear the one-shot) without its ordering or its loader.conf restore.
  - P284 [hidden] The efivar file path `"{name}-{LOADER_VENDOR_GUID}"` is built separately by the writer (`src/systemd_boot.rs:452`) and reader (`:488`); the byte encoding (attributes `0x7` LE + UTF-16LE + NUL, `:463-466`, decoded `:495-505`) is re-implemented in `tests/lifecycle.rs:100-104,175-183`.
  - P285 [hidden] `apply` re-reads `LoaderEntryOneShot` from the firmware (`src/systemd_boot.rs:249`) although `Boot.raw_oneshot` (`:92`) holds the value read at `:143`: two sources for one fact in one run.
  - P298 [needless] `ensure_bootable`'s advice "Write a system into slot {slot} with 'recab stage' first" (`src/systemd_boot.rs:293-294`) is also shown when a failed trial's fallback cannot boot (`lib.rs:416-419`).

### F11 The init-system adapter

One adapter per init: the startup verdict (`Health` carries it, and `trial` uses `health`), failed services, reboot (programs from the `Target`), crash option, the service file, its enabling and logging. Only the `init` fact of the profile is read; the unused `openrc-init` branch goes; the OpenRC service reports recab's start honestly; recab declares its own packaging step, so the builder's generic recipe has no `if recab`.

- **Done when:** Each init's facts (verdict, failed list, reboot, crash option, service file and link, logging) live in one adapter; `trial` uses `health`; only `REC_INIT` is required of the profile; `install-tools.rhai` has no `if ctx.name == "recab"`.
- **Depends on:** F01
- **Problems (24):**
  - P008 [hidden] `src/main.rs:177-188` renders `Health` as text one way and `src/lib.rs:352-361` renders the same startup/failed data as a trial message another way.
  - P017 [hidden] `src/lib.rs:342-350` (trial) and `:434-436` (health) repeat `Config::running` → `running_profile().init` → `wait_finished`.
  - P052 [needless] `src/lib.rs:442`, `src/main.rs:119-126` the init-system argument is named `package` (`--package <init>`) after Recipe's use (`install-tools.rhai:111`), not after what it is.
  - P197 [hidden] `trial` and `health` share the wait (`src/lib.rs:342-350`, `:433-437`) but `trial` does not call `health`, which README.md:22 and `main.rs:105` call "the verdict a trial judges by"; the verdict `trial` uses (`Startup`) is `#[serde(skip)]` (`init_system.rs:80-83`), and `health --json` consumers judge by `finished` + `failed` (`testing/ginux-boot/ginux-boot.sh:159-176`), a different rule (`degraded` is `finished: true`, `init_system.rs:150`).
  - P198 [hidden] `Startup::Failed` holds systemd's raw word (`src/init_system.rs:150-151`) but English phrases on OpenRC (`:190,195`); `lib.rs:354` formats both as "finished starting in state '…'".
  - P205 [hidden] `src/package.rs:38` writes the `sysinit.target.wants` link while `recab-trial.service:14` (`WantedBy=sysinit.target`) states the same fact for `systemctl enable` (README.md:161); `package.rs:39` (`etc/runlevels/boot`) is restated in README.md:129,161-162.
  - P206 [hidden] `RECAB_BIN = "/usr/bin/recab"` (`src/package.rs:16`) vs `install-tools.rhai:19`'s `INSTALL_PATH` override and `distro/builder/src/recipe/tools.rs:332`; `recab-trial.openrc:23` checks `comm = recab`: four copies of where and what the binary is.
  - P207 [hidden] `src/package.rs:50-52` fills `@RECAB@`/`@TIMEOUT@` with `str::replace`; the template placeholders (`recab-trial.service:11`, `recab-trial.openrc:12`) must match and nothing checks that all were replaced.
  - P221 [needless] `src/init_system.rs:31` and `lib.rs:343,500` parse the whole six-fact `Profile` for `init`; `trial`, `health`, `commit`, `rollback` fail with exit 20 when an unrelated fact (`REC_DESKTOP`, `REC_KERNEL_LINE`) is missing.
  - P223 [needless] `health`'s default timeout is the trial deadline `TRIAL_TIMEOUT_SECS` (`src/main.rs:112`).
  - P224 [needless] `install-unit` hides that it also writes the carried list (`src/package.rs:67-69`).
  - P226 [needless] `type Waited = (bool, String, Vec<String>, Startup)` (`src/init_system.rs:120`) is a positional tuple; `finished` is stated apart from `Startup`, so the two can disagree.
  - P234 [special] `src/init_system.rs:128` `systemctl is-system-running` stderr goes to `/dev/null`; `:152` an empty answer becomes "no answer"; `:151` `maintenance` alone is "not finished but failed"; `:197-198` `shutdown`/`reboot` become `Inconclusive`.
  - P237 [special] `services/recab-trial.openrc:12-14` backgrounds `recab trial`, then `eend $?` reports `echo`'s status: the service always counts as started.
  - P238 [special] `services/recab-trial.openrc:19-29` `stop()` imitates systemd's `Conflicts=shutdown.target`, hiding the `comm` lookup error with `2>/dev/null` (`:23`).
  - P247 [hidden] The text of `TrialRecord.detail` (shown by status) is assembled in `src/lib.rs:352-361,395,400` from `Startup` (defined in `state.rs`, produced in `init_system.rs`): owned by none of them; the message "the trial boot of slot {active} {detail}; not committing it…" is copied four times with different endings (`lib.rs:382-383,412-413,417-418,426-427`).
  - P271 [surplus] `--timeout`'s default `TRIAL_TIMEOUT_SECS` (`src/main.rs:101`) and the service's `@TIMEOUT@` (`package.rs:51-52`) carry the same constant; the service always passes it explicitly.
  - P304 [hidden] `src/services/recab-trial.openrc:7` `after sysfs` is there only because OpenRC's sysfs service mounts efivarfs, which `firmware_trial()` reads (`src/systemd_boot.rs:157`); nothing says so.
  - P305 [hidden] The two service files repeat the same `Description` (`recab-trial.openrc:3`, `recab-trial.service:3`), and the OpenRC `stop()` copies systemd's `Conflicts=shutdown.target` by hand (`.openrc:17-18`); nothing ties them.
  - P308 [hidden] `TRIAL_TIMEOUT_SECS` lives in the packaging module (`src/package.rs:19`) but is also the CLI default for `trial` and `health` (`main.rs:101,112`) and a contract constant (`contract.rs:29`); with `budget.rs:50-51` far shorter, `ab_update` can never see the "startup never finishes" reboot path.
  - P310 [hidden] The `Startup` concept is split over three files: defined in `src/state.rs:261-273`, produced only in `src/init_system.rs:148-203` (with recab-made phrases on OpenRC), turned into prose in `src/lib.rs:352-356`.
  - P311 [hidden] `openrc-shutdown` and `reboot` are fixed program names (`src/init_system.rs:275,277`) while `systemctl` and `rc-status` go through `Config`: overrides are inconsistent within one adapter.
  - P320 [surplus] `health()` (`src/lib.rs:433-437`) is a two-line pass-through to `running_profile` + `wait_finished`.
  - P331 [hidden] OpenRC runlevel paths are known three times: `package.rs:39` (`etc/runlevels/boot`), `config.rs:88` (overridable runlevels dir), `init_system.rs:218` (`boot` again).

### F04 One trial lifecycle in the state machine

One `Request` covering set-next, commit, rollback, stage and judging a trial; the state machine plans every transition, including trial-failed and stale-trial cleanup; a boot is a trial only if the state scheduled it and the firmware booted it as a one-shot; the trial decision runs after startup finished and asks the `Target` whether this is an A/B system; selection before state everywhere; status exports the trial's facts.

- **Done when:** Every transition (including stage, trial commit and trial failure) comes from one `plan(Request)`; a boot counts as a trial only when state and firmware agree; the trial service decides after startup and from the `Target`'s A/B fact; selection is written before state in every path; status shows the trial's facts.
- **Depends on:** F03, F11
- **Problems (37):**
  - P300 [surplus] The `BootChange::None` arm of `apply` (`src/systemd_boot.rs:240`) is dead: `plan` never returns it and `stage` never applies `plan_stage`'s.
  - P269 [needless] `System::open` (`src/lib.rs:234-247`) bundles three concerns that change for different reasons (privilege check, run lock, reading boot and state); the trial path needs them apart, which is why it re-checks root by hand (`:380`).
  - P263 [hidden] `trial` builds a second backend (`src/lib.rs:346`, `SystemdBoot::new`) beside `sys.backend`; `Store::new` (`store.rs:40-46`) copies `Config` fields one-for-one like `SystemdBoot::new`.
  - P321 [special] The early `NotATrial` exit (`src/lib.rs:344-349`) exists because recab and its always-enabled trial service ship in every image's base rootfs, live and mutable included (`distro/builder/src/pipeline/live_tools.rs:43` `ToolLayer::Rootfs`), so non-A/B systems must be recognised at runtime.
  - P279 [special] Whether recab's state file exists is the unnamed signal "this is an A/B system": `src/init.rs:75-82` writes state only for `InitLayout::Ab`, and `src/lib.rs:347`, `src/state.rs:294,324` (via `Store::exists`, `store.rs:70-72`) read it that way.
  - P014 [hidden] "save state, then audit" is repeated by hand at `src/lib.rs:314-317`, `:406-410`, `:522-528`; each caller must know the order and the arguments.
  - P015 [hidden] `src/lib.rs:400-405` builds the trial-failed transition by hand (clears `pending_trial_slot`, stamps the operation and time), copying `stamped()` (`lib.rs:468-473`) and bypassing `state::plan`: part of the state machine lives outside `state.rs` (REQ-LIB-003).
  - P019 [hidden] `src/lib.rs:323,328,333,366` pass operation names as literals (`"set-next"`, `"commit"`, `"rollback"`, `"trial"`), copying `Operation::name()` (`state.rs:116-125`); `"trial"` matches no operation (`trial-failed`).
  - P020 [hidden] `Store::save`'s `during` label comes from three sources: `"stage"` (`lib.rs:315`), `"trial"` (`:406`), `transition.operation.name()` (`:522`).
  - P050 [needless] `src/store.rs:94` `save(state, during: &str)` takes a string only for the error message, in each caller's vocabulary.
  - P051 [needless] `src/lib.rs:478` `change()`'s `annotate` closure exists for trial's commit alone (`:396`); every other caller passes `|_| {}` (`:323,328,333,423`).
  - P067 [special] `src/lib.rs:366-374` trial treats any `System::open` failure (a permission refusal, a lock error) as "the ESP or /var broke" (`view = None`).
  - P068 [special] `src/lib.rs:380` `require_root("trial")` only in the `RebootUnread` branch; the others got it implicitly through `System::open(.., Some("trial"))`.
  - P069 [special] `src/lib.rs:387` an `unreachable!` for verdict/read-error pairs only `judge_trial`'s contract excludes.
  - P071 [special] `src/lib.rs:346-349` a shortcut returning `NotATrial` with no state and no firmware trial (live and mutable systems), ahead of the real verdict.
  - P104 [hidden] "Trial" means five things derived separately: `pending_trial_slot` (`src/state.rs:28`), `Status.trial_pending` = one-shot set (`lib.rs:261`), `SlotView.trial` = entry has trial options (`systemd_boot.rs:638`), `firmware_trial` = selected ≠ default (`systemd_boot.rs:170`), `judge_trial`'s `is_trial` (`state.rs:319`).
  - P106 [hidden] `src/lib.rs:411` computes the fallback with `known_good(..).filter(!= active)`, then `:421-423` runs `Request::Rollback` and relies on `rollback_target` (`state.rs:87-93`) picking the same slot from a hand-built state that `change()` then audits as "before" (`lib.rs:528`).
  - P119 [needless] `PersistentState`'s fields are all `pub` (`src/state.rs:18-39`) and set directly outside the state machine (`init.rs:76-80`, `lib.rs:396,401-405`).
  - P123 [surplus] `plan_stage`'s `Transition.boot` is always `BootChange::None` (`src/state.rs:193`) and never applied.
  - P124 [surplus] `src/lib.rs:494` the `Some(default)` guard can never be false for a `OneShot` (`plan` requires `sel.default == Some(fallback)`, `state.rs:217`).
  - P126 [special] `plan` refuses `SetNext(slot)` when `slot == active` (`src/state.rs:206-211`): with two slots the only valid argument is `active.other()`, so the parameter exists to be checked; it also accepts the known-good slot of an uncommitted run (`:212-226`).
  - P127 [special] `plan` returns a no-op for set-next when `sel.oneshot == Some(slot)` (`src/state.rs:213-215`) even if the state has no `pending_trial_slot`.
  - P129 [special] `plan_stage` clears `pending_trial_slot`/`previous_good_slot` only when they name the staged slot (`src/state.rs:186-192`); otherwise a stale `pending_trial_slot` lives on.
  - P132 [special] `src/lib.rs:421` skips the rollback when the next boot already is the fallback.
  - P142 [special] `Operation::Init(Slot)` is always `Slot::A` (`src/init.rs:78`), and init saves state without the audit event every other transition writes (`init.rs:81`).
  - P195 [hidden] `src/lib.rs:344-349` decides "no state and no one-shot" before `wait_finished` (`:350`), while the unit has `DefaultDependencies=no` in sysinit (`services/recab-trial.service:4,14`) and the OpenRC script only `after sysfs` (`recab-trial.openrc:7`): correctness rests on `firmware_trial`, an ordering rule nothing states.
  - P196 [hidden] Live and mutable images exit early only because `firmware_trial` (`src/systemd_boot.rs:157-171`) rejects entry names without `-a`/`-b`: the entry naming rule is part of the trial service's "not a trial" decision.
  - P227 [surplus] `src/lib.rs:353` the `Startup::Inconclusive(_)` arm building `problem` is never used (`judge_trial` returns `Ignore` first, `state.rs:308-309`).
  - P232 [special] `src/lib.rs:411` `.filter(|slot| *slot != active)` for the one case where the known-good slot is the trial slot.
  - P244 [hidden] Transitions come from three unrelated paths: `state::plan` (`src/state.rs:198`), `state::plan_stage` (`:163`, not a `Request` variant), and the hand-built trial-failed state (`src/lib.rs:400-405`); `Request` (`state.rs:96-100`) has no Stage or TrialFailed while `Operation` (`state.rs:104-113`) has both.
  - P245 [hidden] "Never select an unbootable slot" sits in `lib.rs`, outside the state machine that picks the slot: `src/lib.rs:416` checks the failed trial's fallback, and `change()` checks the Rollback it then plans again (`:484-487`).
  - P248 [hidden] The word "stage" means two things: `src/main.rs:85` ("Stage a one-shot trial boot") and `src/state.rs:208,219-220` ("set-next stages the inactive slot", "cannot stage trial boot") use it for set-next, while `stage` is the command that writes a slot; scheduling a trial has no name of its own.
  - P272 [special] `src/lib.rs:343` reads the distro profile before checking whether the boot is a trial (`:347`); the trial service runs on every image, so an os-release without `REC_*` fails with exit 20 on boots that are not trials.
  - P274 [special] `Operation::label` writes `rollback-to-{slot}` (`src/state.rs:134`) while every other operation is `{name}-{slot}`.
  - P280 [hidden] "Never lower the schema, keep unknown fields" (`src/state.rs:11-14`, `store.rs:88`, REQUIREMENTS REQ-PERSIST-004) holds only because each writer clones the loaded state; `src/init.rs:76-80` starts from `default()` and overwrites both.
  - P283 [hidden] `Operation` has two name tables, `name()` (audit) and `label()` (`last_operation`) (`src/state.rs:116-137`); both change with every variant.
  - P302 [special] `known_good(..).unwrap_or(active)` (`src/state.rs:216`) is a second fallback whose only effect is the wording of the refusal (`:217-222`).

### F05 One trial-options policy

The options a trial entry carries come from one function over the *target slot's* init and whether its entry boots through recinit's installed initramfs (typed, not sniffed); the same function strips them at commit; a non-zero `panic=` is forced on trial entries.

- **Done when:** One function decides and strips trial options from the target slot's init and its typed initramfs fact; no module iterates `InitSystem::ALL` to recognise options; trial entries always carry a non-zero `panic=`.
- **Depends on:** F02, F11
- **Problems (9):**
  - P091 [hidden] `src/systemd_boot.rs:412` decides the fail-reboot word by sniffing the entry text after `:401` rewrote the `initrd` line, though the function has `initrd: Option<&Path>`; the ordering is what makes it right.
  - P112 [needless] `src/systemd_boot.rs:22` imports `recinit::live::FAIL_REBOOT_OPTION` (an installed-initramfs word) from recinit's live-boot protocol module; recab depends on recinit for this constant and three ISO paths.
  - P113 [needless] `src/systemd_boot.rs:19,53-61` the ESP text code owns the trial-option policy and depends on `init_system::crash_option` and `InitSystem::ALL`, so it changes whenever init support does.
  - P134 [special] `src/systemd_boot.rs:407-409` adds `panic=10` only when no `panic=` is present, for entries not written by `new_entry` (`:686`, which always adds it); an operator's `panic=0` wins.
  - P260 [hidden] Entry options are assembled in three places: `new_entry` (`src/systemd_boot.rs:678-690`), `install_slot_boot_files` (`:406-413`) and `set_trial_options` (`:316-328`); the last two copy the strip-and-re-add of trial options.
  - P286 [hidden] "Boots through an initramfs" has three forms: `new_entry`'s `initrd: bool` (`src/systemd_boot.rs:678`), `install_slot_boot_files`'s `initrd: Option<&Path>` (`:389`), and `trial_options` scanning the entry text (`:54`).
  - P287 [hidden] `src/systemd_boot.rs:55` adds `FAIL_REBOOT_OPTION` whenever an entry has any `initrd` line, assuming every initramfs is recinit's, which honours `rd.emergency=reboot` (`tools/recinit/src/live.rs:48-52,165-167`).
  - P288 [hidden] The trial-options concept spans three places: the word in `tools/recinit/src/live.rs:167`, the crash option in `src/init_system.rs:58-62`, composition and stripping in `src/systemd_boot.rs:53-61`; `is_trial_option` iterates `InitSystem::ALL` through another module to recognise its own output.
  - P297 [needless] `set_trial_options(entry, trial: Option<InitSystem>)` (`src/systemd_boot.rs:316`) encodes the caller's workflow: `None` means "becoming the default".

### F06 Stage: a check phase, then one write phase

All checks (inputs, slot partition, the image's own profile: ID, init, architecture, flavour; its carried list; the running fstab; sizes) run read-only and produce a `StagePlan`; only `StagePlan::write` mutates, and its failures are classified as partial updates; one mount guard type, with stale stage mounts cleaned at start; formatting through recpart's format command and roles.

- **Done when:** `write` takes only a `StagePlan`; every stage refusal happens before the first write (lifecycle test); post-write failures exit 22 naming the slot; one mount guard; mkfs flags come from recpart.
- **Depends on:** F01, F02, F03, F08
- **Problems (26):**
  - P012 [hidden] `src/lib.rs:298-318` fixes the order of the `stage::*` steps (`ensure_can_change`, `check_inputs`, `reset_stale_selection`, `check_target`, `check_flavour`, save + audit, `write_slot`) with comments as the only guard; the steps live in `stage.rs`, their order in `lib.rs`.
  - P021 [hidden] `src/lib.rs:290-296` keeps the release medium mounted only by binding the guard to `_media`; rename it `_` and the medium unmounts before `write_slot` reads it.
  - P022 [hidden] `src/lib.rs:299-300` works out the target slot (`active.other()`) instead of taking it from `plan_stage`'s transition (`:298`): a second copy of the state machine's choice.
  - P073 [special] `src/lib.rs:287,311` a `change_flavour: bool` threaded through the public `stage()` for one CLI flag (`main.rs:79-82`).
  - P116 [needless] `slot_device` (`src/systemd_boot.rs:340-344`) carries stage's rule ("stage needs root=PARTUUID=") inside a backend query.
  - P145 [hidden] `src/stage.rs:245-254` mounts a fresh ext4 and runs recstrap without `--allow-nonempty`, relying on recstrap's empty-guard exception for `lost+found` (`recstrap/guards.rs:179-198`).
  - P146 [hidden] `src/stage.rs:246-247` builds `mkfs.<fs> -F -q -L <label>` from `Role::filesystem()` but hard-codes ext4's flags; recpart owns per-filesystem mkfs flags (`tools/recpart/src/exec.rs:237-247`).
  - P147 [hidden] `src/stage.rs:28-33` (`slot_role`, Slot → Role) duplicates `Role::slot` (`tools/recpart/src/policy.rs:76-82`); `stage.rs:244` `.expect("slot roles have labels")` relies on `policy.rs:40-51`.
  - P148 [hidden] `Mounted::mount_image_ro` (`src/stage.rs:585-596`, `ro` vs `ro,loop` by a bool) copies `Session::mount_image` (`tools/recstrap/src/session.rs:144-157`); the image is mounted twice per stage (check_flavour, then recstrap); the three `Mounted` constructors (`stage.rs:575-604`) repeat one body.
  - P160 [hidden] `src/stage.rs:54-58` (`open_media`) and `recsetup/plan.rs:322-332` both resolve the installed kernel/initramfs on a medium from `recinit::live`, with opposite policies: stage silently stages without an initrd, recsetup refuses (`plan.rs:364-377`); the kernel/initrd `is_file` checks (`stage.rs:133-142`) duplicate `plan.rs:361-379`.
  - P161 [hidden] `src/stage.rs:208-216` (image bytes ≤ partition) and recstrap's `enough_space` (`guards.rs:21,213-222`) are unrelated size checks for one extraction.
  - P162 [hidden] `check_target`'s doc (`src/stage.rs:165-168`) claims "named by PARTUUID, not the running root", checks that live in `systemd_boot.rs:333-361`.
  - P163 [hidden] `src/stage.rs:86` mounts the image as EROFS before recstrap's `kernel_has_erofs` guard (`guards.rs:287-300`) runs; on a kernel without EROFS the user gets a raw mount error.
  - P164 [hidden] `Inputs` holds only the canonical rootfs (`src/stage.rs:65-67`); `write_slot` re-reads the raw request paths (`:233,258,280-281`), so nothing ties what it installs to what was checked.
  - P165 [hidden] `src/stage.rs:237,282`, `lib.rs:301` give the staged entry trial options from the running init; the image's own profile is compared only on `REC_FLAVOUR` (`stage.rs:75-101`), while recstrap parses it separately (`guards.rs:302-313`); an image of another distro, init or architecture passes.
  - P174 [needless] `write_slot` takes the whole `Profile` and uses only `profile.init` (`src/stage.rs:228-237`).
  - P178 [needless] `Mounted::mount_image_ro(.., block_device: bool, ..)` (`src/stage.rs:586`) makes the caller pass `ErofsImage.rdev.is_some()` (`:86`).
  - P181 [surplus] `src/stage.rs:387,564` replace the baseline and fstab durably, file by file, inside a slot that is unbootable and then flushed whole by `sync_filesystem` (`:274,502-511`).
  - P186 [special] Refusals after the slot is wiped: `carried::read` (`src/stage.rs:263`), "listed as a directory but is not one" (`:308`), fstab without `/` (`:562`) all run after `remove_slot_boot_files` and `mkfs` (`:241-247`), and after the slot was dropped as rollback target (`lib.rs:315`).
  - P189 [special] `Mounted`'s Drop swallows an unmount failure into a warning (`src/stage.rs:613-619`); `check_flavour` unmounts before checking the read (`:87-89`), so an unmount error hides the read error; a killed stage leaves `/run/recab/*` mounted.
  - P192 [special] `write_slot` failures after the state save exit 1, unclassified (`src/lib.rs:318`).
  - P250 [hidden] `src/lib.rs:290-296` mounts a release medium before `plan_stage` (`:298`) can refuse, so a refused stage still mounts and unmounts it.
  - P252 [hidden] `Inputs` and `Target` (`src/stage.rs:65-67,104-108`) are check-result tokens, but `check_flavour` (`:75`) returns none, so nothing stops a caller skipping it before `write_slot`.
  - P253 [hidden] `ErofsImage.rdev.is_some()` stands for "the rootfs is a block device" at `src/stage.rs:86,186,192`; the concept is never named.
  - P254 [hidden] The stage inputs (rootfs/kernel/initrd) are listed as separate literal arrays at `src/stage.rs:133-142` and `:146-160`.
  - P267 [hidden] `recab-tui/src/presentation/pages.tsx:265` assumes exit 21 means "recab checked first and changed nothing", while stage refusals at `src/lib.rs:310-311` come after `reset_stale_selection` wrote EFI variables.

### F07 The image declares its machine state; every slot carries its own baseline

The baseline is written when the image is built (decision D-6), so slot A has one; the carried list states everything carried, including fstab, per init; the carry engine gives each kind its behaviour (three-way merge with mode and owner for files and directories, accounts merged by name with UID/GID collisions refused and group members joined, prefixes with whiteouts) and never resolves through symlinks inside the slot; `/root` per decision D-4.

- **Done when:** A freshly installed slot A has `usr/lib/recab/baseline`; the carried list covers fstab and states per-init entries; each carry kind has its own behaviour with mode and owner; no carry path resolves through a symlink in the slot; `/root` survives an update (VM).
- **Depends on:** F06; decisions D-4, D-6
- **Problems (25):**
  - P143 [hidden] `src/stage.rs:255-260` extracts with `recstrap`, which copies with GNU `cp -aT --preserve=xattr` (`tools/recstrap/src/bin/recstrap/rootfs.rs:44-48`), while `copy_tree` (`stage.rs:452-456`) avoids `cp` "because busybox differs" and REQ-BACKEND-014 forbids GNU options: one requirement, two answers.
  - P144 [hidden] `copy_tree` (`src/stage.rs:457-499`) keeps owner, mode and symlinks but drops xattrs, ACLs and hard links; recstrap treats a lost xattr as an error (`rootfs.rs:15-17,46`).
  - P150 [hidden] `write_fstab` (`src/stage.rs:541-566`) is a second fstab reader/writer beside recfstab's (`tools/recstrap/src/fstab.rs:245-300`); it rewrites only the root line and leaves recfstab's `# <origin device>` comment (`fstab.rs:51`) naming the old slot's device.
  - P151 [hidden] `src/stage.rs:268-273` carries `/etc/fstab` by its own rule, outside the image-declared list (`services/carried`), which cannot express it.
  - P153 [hidden] `src/stage.rs:298-299,312-317` the "staged before baselines existed" fallback applies to every freshly installed slot A: nothing at install time (recstrap, recsetup, builder) writes `usr/lib/recab/baseline`; only stage does (`stage.rs:266,337`).
  - P154 [hidden] `BASELINE` (`src/stage.rs:337`) sits in image-owned `/usr` beside the package-owned `CARRIED_LIST` (`carried.rs:9`); nothing reserves the path from packages.
  - P155 [hidden] The baseline line format is stated at `src/stage.rs:333-336`, written at `:380-386`, read at `:395-405`, with the signature form split and re-joined both ways.
  - P156 [hidden] The carried-list format is stated three times: `services/carried:1-9`, `src/carried.rs:12-22`, parsed at `carried.rs:53-59`.
  - P157 [hidden] `services/carried` must cover every file `distro/builder/guest/installed/system-setup.sh:37-112` writes at install (localtime, locale.conf, vconsole.conf, conf.d/loadkeys, runlevels, hostname, hosts, accounts, machine-id, unit links); neither refers to the other.
  - P158 [hidden] `distro/builder/recipes/installed-system.rhai:391-402` moves homes to `/var` so updates keep them, but `/root` stays in the slot and `services/carried` does not list it: root's files are lost on every update.
  - P177 [needless] `target_root::merge_accounts` (`tools/recstrap/target-root/src/lib.rs:135-154`) sits in the shared crate with one caller (`src/stage.rs:526`); it merges by name only (no UID/GID collision check, no union of group members).
  - P179 [needless] `package.rs:45,67-69` (per-init `install-unit`) writes one init-agnostic list carrying both `dir etc/systemd/system` and `dir etc/runlevels` everywhere (`services/carried:26-27`), while REQ-BACKEND-013 ties the carried configuration to the init.
  - P180 [surplus] `write_preserving_mode` (`src/stage.rs:531-538`) wraps `target_root::write_durable` + chown for one caller.
  - P183 [special] `src/stage.rs:298-299,315-316` the no-baseline path carries a dir whole over the new image's `etc/systemd/system` (default.target, display-manager, wants links from `installed-system.rhai:74-147`); with `--change-flavour` it undoes the flavour change, and later baselines make it permanent.
  - P184 [special] `src/stage.rs:305-318` only `Carry::Dir` gets the three-way merge; `Carry::File` (`services/carried:11-19`) always takes the machine's copy even when untouched, so an image can never update e.g. `etc/hosts`; a deleted `file` entry comes back; prefix entries never remove anything.
  - P185 [special] The carry follows symlinks inside the new slot (`src/stage.rs:411-421` recursion, `create_dir_all` in `copy_tree` `:482`, `to.join(path)` in `remove_whiteouts` `:432-435`); an absolute link in the image resolves against the running root.
  - P188 [special] `merge_accounts` (`src/stage.rs:517-523`) skips silently when the running file is missing and copies it whole when the image lacks it.
  - P190 [special] `src/stage.rs:341-351` the signature records type, link target and content only, so a chmod/chown-only change is not carried, though `copy_tree` carries owner and mode (`:492-496`).
  - P208 [hidden] The carried list is one feature in four places: `package.rs:67-69` writes it, it lives at `src/services/carried` (not a service), `carried.rs:9` owns its path and parser, `stage.rs:261` reads it; README.md:215 files it under "the trial service files".
  - P255 [hidden] The baseline's `"d"` signature is compared as a raw string at `src/stage.rs:312,357,415`.
  - P256 [hidden] `src/stage.rs:305-318` puts `Carry::File` and `Carry::Dir` in one arm, then tells them apart again with `matches!` twice (`:307,312`).
  - P265 [hidden] The trial service's enabling links (`src/package.rs:38-39`) sit inside directories the carried list carries (`services/carried:26-27`, `dir etc/systemd/system`, `dir etc/runlevels`): whether a new slot runs trials depends on the carry and its baseline, and nothing ties the two lists. The OpenRC `boot` runlevel (`package.rs:39`) is stated nowhere in `recab-trial.openrc`.
  - P273 [special] `write_fstab` sets the fstype only when a third field exists (`src/stage.rs:555-557`).
  - P307 [hidden] Whether an operator's disabling of the trial service (README.md:160-162) survives an update depends on the baseline and whiteout logic (`src/stage.rs:312-314`), because the enable link (`package.rs:38-39`) is inside a carried directory; none of the three mentions the others.
  - P314 [hidden] `src/services/carried:33-36` (`dir etc/sudoers.d`, `dir etc/doas.d`) re-encodes the privilege-tool layout `distro/profile/src/lib.rs:141-163` (`admin_rule_file`, `config_file`) owns (doas's rule lives in `etc/doas.conf`).

### F09 recab publishes its decisions; front ends render them

`status --json` carries each action's verdict (ready / no change / refused + reason, from the state machine), the trial facts, the entry prefix and the audit path, with a schema version; recab-tui renders them, uses the generated error classes, and drops its planner, EROFS check and fixture copies (decision D-3).

- **Done when:** recab-tui has no planning rule, EROFS check, exit-code table or fixture copy of recab's rules; status and health JSON carry a schema version; `cargo xtask gen --check` and `cargo xtask check tui` pass.
- **Depends on:** F04, F08; decision D-3
- **Problems (16):**
  - P030 [hidden] `tui/apps/runtime/recab-tui/src/domain/recab.ts:164-175` regex-parses `recab: [RECAB_ERR_…]` from stderr, including the JSON stderr of `status --json` (`main.rs:138`), so it shows raw JSON as the message; the class and code it is meant to read are ignored.
  - P037 [hidden] `tui/apps/runtime/recab-tui/src/app/recab-runner.ts:31-41` copies the EROFS magic and offset check (`README.md:61-62`, `target_root::erofs_image`).
  - P065 [surplus] `src/contract.rs:27-29` exports `ERROR_CLASSES`, `STATE_FILE`, `TRIAL_TIMEOUT_SECS`, read by nothing outside `recab-tui/src/generated/`; the `Health` and `ErrorPayload` TS types have no consumer.
  - P102 [hidden] `Status.known_good_slot` is derived (`known_good(state, sel)`, `src/lib.rs:264`), not the stored field; `recab-tui/src/domain/actions.ts:79` restates `plan`'s `.unwrap_or(active)` (`state.rs:216`) as `s.known_good_slot ?? active`.
  - P103 [hidden] `recab-tui/src/domain/actions.ts` re-implements `state::plan`/`plan_stage` with copied refusal texts: set-next `:81-88` = `state.rs:213-223`; commit `:108-120` = `state.rs:229-236`; rollback `:143-150` = `state.rs:240-247`; stage `:177-192` = `state.rs:166-184`. It already differs: it ignores `SlotView.bootable` (`lib.rs:485-486` refuses) and the flavour refusal (`lib.rs:311`).
  - P171 [hidden] `recab-tui/src/app/recab-runner.ts:31-42` copies the EROFS magic but not the `.erofs` extension rule (`target-root/src/lib.rs:513-514`): a file the TUI accepts, recab refuses. `actions.ts:239-249` and `presentation/pages.tsx:333` restate stage's flags without `--from-media`/`--change-flavour`; `actions.ts:197` restates the carried list; `presentation/use-recab.ts:22` restates `INITRAMFS_INSTALLED_ISO_PATH`.
  - P210 [hidden] `src/contract.rs:12` writes the checked-in `docs/*.schema.json`, which carry no "generated" mark; `docs/health.schema.json:8,15,22` copy the doc comments of `init_system.rs:70-78`.
  - P211 [hidden] `recab-tui/src/domain/recab.ts:44,55,123` promise the trial service commits any uncommitted running slot; `judge_trial` judges only `pending_trial_slot == active` (`state.rs:319`) and `Status` does not export `pending_trial_slot`, so the TUI cannot tell a menu pick from a trial.
  - P212 [hidden] `recab-tui/src/domain/actions.ts:230` reboots with a bare `["reboot"]`, while recab picks the reboot command per init (`init_system.rs:266-283`); `recab.ts:136` help for exit 20 omits the missing-profile cause (README.md:264).
  - P228 [surplus] `recab-tui/src/domain/recab.ts:138` help for exit 23, which only `recab trial` returns, a command the TUI never runs.
  - P264 [hidden] `config::state_file()` is exported but no audit-log path is, so `tui/apps/runtime/recab-tui/src/presentation/pages.tsx:405` hard-codes `/var/lib/recab/audit.log`.
  - P266 [hidden] `recab-tui/src/domain/recab.ts:61` (warning "default isn't the known-good slot") copies set-next's refusal condition (`src/state.rs:217`).
  - P275 [special] `recab-tui/src/app/recab-runner.ts:45-46,60-62` swallow read errors (`isErofs` returns null) and leave the decision to recab.
  - P327 [hidden] `tui/apps/runtime/recab-tui/src/domain/recab.ts:136` says exit 20 means a missing blkid/mkfs/mount/recstrap; `stage.rs:245-261` runs those unclassified (exit 1), and README.md:264 describes 20 differently again.
  - P328 [hidden] recab-tui's fixtures (`tui/apps/runtime/recab-tui/fixtures/recab:39-41,94,100`, "Changes follow recab's rules (tools/recab/src/state.rs)") are a third copy of the planning rules.
  - P332 [hidden] `docs/status.schema.json`, `tui/apps/runtime/recab-tui/src/generated/recab.ts` and `docs/content/src/tool-help.ts` live in three repositories and must all be regenerated when `Status`, `Health`, `ErrorPayload` or the help change; the only guard at run time is a shape check (`recab-runner.ts:21-27`).

### F10 Other tools take recab's facts from recab

install-tests read `status --root --json` and recab's layout API instead of grepping files and hard-coding names; recsetup builds `recab init` from recab's own request type and stops repeating its checks; docs/content take names, paths and the carried list from generated facts.

- **Done when:** install-tests, recsetup, the builder and docs/content name no recab path, entry name, state field or CLI flag of their own; recsetup no longer repeats `recab init`'s checks.
- **Depends on:** F02, F09
- **Problems (26):**
  - P239 [special] `distro/builder/recipes/install-tools.rhai:109-112` `if ctx.name == "recab"` adds a packaging step and an `INIT_SYSTEM` requirement for one tool; `is_installed` (`:94-101`) does not check the service or the carried list; the target is hard-coded x86_64 musl (`:14,41`).
  - P225 [needless] `distro/builder/src/recipe/tools.rs:328-337` defines `INIT_SYSTEM` and keys the work directory per init for every live tool, because recab alone needs it.
  - P204 [hidden] `src/package.rs:26-31,36-41` (unit path and enabling link per init) repeat the service layout in `installed-system.rhai:66-80` (`service_file`/`enable`), `testing/install-tests/test-scripts/lib/init/systemd.sh:25-32` and `openrc.sh:24-33`.
  - P100 [hidden] `RunningRoot::Mounted` (`src/systemd_boot.rs:523-537`, partition mounted at root → PARTUUID) duplicates recsetup's `RootMount` (`recsetup/plan.rs:513`).
  - P005 [hidden] `tools/recstrap/src/bin/recsetup/plan.rs:396-455` builds `recab init`'s argv by hand (`--slot-a`, `--root-device`, `--kernel`, `--options`), tracking `src/main.rs:37-62` with no shared type.
  - P006 [hidden] The layout rule (no root device under ab, both slots, A ≠ B) is checked three times: `recsetup/plan.rs:399,411,433`, `src/main.rs:51,161-165`, `src/init.rs:38`; recsetup compares device paths, init compares PARTUUIDs.
  - P033 [hidden] `testing/install-tests/src/scenarios/ab_update.rs:50-55` and `scenarios/mod.rs:516-519` pass the raw `os_id` to `recab::entry_name`/`layout_name`; recab lower-cases it first (`os_prefix`, `systemd_boot.rs:646-648`, crate-private) and honours `RECAB_ENTRY_PREFIX`; `Status.entry_prefix` goes unused.
  - P034 [hidden] `testing/install-tests/src/scenarios/ab_update.rs:51` and `test-scripts/install.sh:36` hard-code `/boot/`, a copy of `recpart::policy::ESP_MOUNT`.
  - P035 [hidden] `testing/install-tests/test-scripts/install.sh:29-31` hard-codes the entry names `$OS_ID-a.conf`/`-b.conf` and greps the state file for the literal `"known_good_slot": "A"` (field name `state.rs:21`, Slot spelling `lib.rs:60`, `to_vec_pretty` spacing `store.rs:95`).
  - P036 [hidden] `testing/ginux-boot/ginux-boot.sh:143-144` re-checks `Health`'s JSON shape by hand in jq, copying `init_system.rs:66-84`; `docs/health.schema.json` is unused there.
  - P054 [needless] `testing/install-tests/src/scenarios/mod.rs:514-520` and `ab_update.rs:53-63` depend on ESP layout internals (`layout_name`, `entry_name`, `ENTRIES_DIR`, `trial_service_link`).
  - P094 [hidden] `os_prefix` (`src/systemd_boot.rs:646`) is crate-private, so `testing/install-tests/src/scenarios/ab_update.rs:54`, `scenarios/mod.rs:516`, `test-scripts/install.sh:26-30` use the raw `os_id`; correct only while IDs are lower case.
  - P108 [hidden] `testing/install-tests/test-scripts/install.sh:22-37` hard-codes the ESP layout: `boot/EFI/BOOT/BOOTX64.EFI` (x64 only vs `SYSTEMD_BOOT_ARCHES`, `systemd_boot.rs:38`), `boot/loader/loader.conf`, `vmlinuz`, `initramfs.img`; the matching re-exports go unused.
  - P109 [hidden] `testing/install-tests/src/scenarios/ab_update.rs:75-82,123-127` check trial options by grepping entry text for `panic=` (`PANIC_KEY` is private, `systemd_boot.rs:42`) and `FAIL_REBOOT_OPTION`, instead of reading `Status.slots[].trial`.
  - P110 [hidden] `docs/content/src/content/04-architecture/01-faq.ts:197` hard-codes `/boot/levitateos-a/` (wrong for AcornOS); `01-getting-started/08-troubleshooting.ts:62,66` restates entry names and kernel file names; `06-installation-boot.ts:38` says recsetup installs systemd-boot with `bootctl install`, a job `install_loader` (`systemd_boot.rs:695`) now does.
  - P111 [hidden] `tools/recpart/src/policy.rs:117,119` (`SLOT_BOOT_FILES_MB`, `BOOTLOADER_MB`) size the ESP for recab's kernel dirs and loader; nothing in recab refers to that budget (`systemd_boot.rs:384-417`).
  - P172 [hidden] `testing/install-tests/src/scenarios/ab_update.rs:200-203` (identity = machine-id + SSH keys) restates part of `services/carried`; `:115-116` checks the hostname against the contract default, so a carried hostname is never told from a shipped one.
  - P213 [hidden] `testing/install-tests/test-scripts/lib/init/systemd.sh:7-14` and `openrc.sh:13-17` (used by `installed-boot.sh:11-12`, `desktop-boot.sh:30-37`) re-derive the startup verdict apart from `recab health`; the OpenRC copy (`rc-status --format ini boot default`) skips sysinit and the openrc-process check (`init_system.rs:185,229`).
  - P219 [hidden] `docs/content/src/content/04-architecture/03-atomic-updates.ts:37`, `01-getting-started/07-post-installation.ts:108`, `08-troubleshooting.ts:102` restate the trial verdict and fallback table of README.md:107-158.
  - P222 [needless] `trial_service_link` is public (`src/package.rs:36`, `lib.rs:44`) so install-tests (`ab_update.rs:63`, `scenarios/mod.rs:514`) can depend on the package layout.
  - P291 [hidden] `tools/recstrap/src/bin/recsetup/plan.rs:380` and `target.rs:184` hard-code `"boot"` for the ESP recab takes from `recpart::policy::ESP_MOUNT` (`config.rs:103`); recsetup never checks the state partition is mounted at `var/`, which `recab init` writes into.
  - P292 [hidden] `testing/install-tests/src/scenarios/mod.rs:515-519` re-derives `init`'s positional rule (`init.rs:48`, "A for A/B, None for mutable") on its own.
  - P296 [hidden] `docs/content/src/content/05-rec-tooling/03-recab.ts:15` cites `tools/recab/docs/status.schema.json` and `/var/lib/recab` as literals; `:25` hard-codes the medium's `/mnt/live/filesystem.erofs`, `/mnt/boot/vmlinuz`, `/mnt/boot/initramfs-installed.img`, which `stage --from-media` resolves itself (`src/stage.rs:39-58`).
  - P317 [hidden] `testing/install-tests/test-scripts/install.sh:32` checks the trial service link only in `ab` mode, though `package.rs:5-6` enables it on every image and `scenarios/mod.rs:514,531` passes it for both modes.
  - P318 [hidden] `docs/content/src/install.ts:242-243` (`commands.stage`, used by `07-post-installation.ts:84`) hard-codes the medium layout and mounts/unmounts by hand (`07-post-installation.ts:78,91`), which `stage --from-media` does itself; the carried list is restated in prose in `04-architecture/03-atomic-updates.ts:35`, `05-rec-tooling/03-recab.ts:32`, `01-getting-started/07-post-installation.ts:108`.
  - P330 [hidden] `distro/builder/src/release.rs:86` copies `usr/lib/systemd/boot/efi/systemd-bootx64.efi` from `src/systemd_boot.rs:35,38,699`.

### F13 Tests and docs follow the product

Lifecycle fixtures are made by `recab init` itself (the layout recab writes) with the F03 file store; constants come from recab; the VM scenario checks carry semantics and both trial options; README and REQUIREMENTS state each fact once and stop restating defaults, the CLI and exit codes.

- **Done when:** Lifecycle fixtures come from `recab init`; tests import recab's constants; `ab-update` asserts carry semantics and both trial options; README/REQUIREMENTS hold no copied defaults, CLI lists or exit-code tables.
- **Depends on:** all others
- **Problems (25):**
  - P209 [hidden] `TRIAL_TIMEOUT_SECS = 900` (`src/package.rs:19`) is restated in README.md:107; `testing/install-tests/src/budget.rs:50-51` (`TrialReboot` 300, `Commit` 200) are set independently, so a trial that runs to its deadline outlives the scenario's wait.
  - P031 [hidden] Exit codes are copied in `README.md:261-267`, `REQUIREMENTS.md:333-335` and `recab-tui/src/domain/recab.ts:131-138` (help keyed by literal 13/20/22/23); the generated `ERROR_CLASSES` (`contract.rs:27`) is unused.
  - P040 [hidden] `README.md:14-23` and `REQUIREMENTS.md:197-206` copy the CLI by hand (REQUIREMENTS lacks `health`, `--from-media`, `--mode`, `--change-flavour`; README lacks `--change-flavour`); `README.md:276-290` copies the env defaults of `config.rs:76-92`; `README.md:207-217` omits `lib.rs`, `main.rs`, `config.rs`, `contract.rs`.
  - P041 [hidden] `testing/ginux-boot/probe.sh:7-9` still says recab lacks `health` until G19; it exists (`main.rs:107-114`).
  - P088 [hidden] `LOADER_VENDOR_GUID` (`src/systemd_boot.rs:25`) and the efivar byte encoding are copied as `GUID` in `tests/lifecycle.rs:16`.
  - P173 [hidden] `README.md:196` says stage uses `mkfs.ext4` and `chroot`; the code builds `mkfs.<recpart fs>` and never runs chroot.
  - P194 [special] `tests/lifecycle.rs:409-500` stage tests cover refusals and firmware resets only; `write_slot`, carry, baseline, fstab, flavour and `--from-media` have no scratch-tree test.
  - P214 [hidden] `ab_update.rs:79-82` requires `rd.emergency=reboot` on both distros (every release stages an installed initramfs, `distro/builder/src/release.rs:193-196`), while README.md:141-143,155-156 say OpenRC boots without an initramfs and relies on a panic; `tests/lifecycle.rs:281,792` assert the literal `"rd.emergency"` (`recinit/src/live.rs:51`) and "OpenRC trials need no initramfs options" (true only because the scratch entries have no `initrd`).
  - P215 [hidden] `ab_update.rs:155` hard-codes "7 boots" and README.md:230-233 restates "seven boots" with a dated pass.
  - P216 [hidden] `ab_update.rs:5,205`, `budget.rs:35`, README.md:248 name `recab-trial.service` for both distros; on AcornOS it is the OpenRC `recab-trial` script.
  - P217 [hidden] REQUIREMENTS.md:153-155 (REQ-SM-015) and `docs/content/src/content/01-getting-started/06-installation-boot.ts:38` say `recab init` sets up the trial service; the package does now (`init.rs:4-5`).
  - P218 [hidden] README.md:117-122 describes OpenRC failure detection without the "every runlevel service started" rule (only in the env line `:287`), and nests the firmware fallback under the OpenRC bullet; `:284-285` say `RECAB_SYSTEMCTL_BIN`/`RECAB_OPENRC_DIR` are used by `trial`, but `health` and `reboot` use them too.
  - P220 [hidden] `tests/lifecycle.rs:625,631,857,863` assert the unit and script paths as literals (copy of private `trial_service_path`, `package.rs:26-31`); `:18-19` spell all six `REC_*` facts because `running_profile` parses the whole `Profile`.
  - P229 [surplus] `tests/lifecycle.rs:650-654` the `init` test writes the unrendered `recab-trial.service` template into the target, left from when `init` enabled the service.
  - P241 [special] `tests/lifecycle.rs:804` the OpenRC test schedules its second trial under the systemd profile (`tree.ok`), so the OpenRC trial is judged on an entry with systemd's crash option.
  - P243 [special] `ab_update.rs:202` `cat /etc/machine-id 2>/dev/null` swallows a missing machine-id in the identity check.
  - P268 [hidden] README's stage refusal list (`README.md:59-72`) omits the `.erofs` naming rule (`target-root/src/lib.rs:513-515`) and the flavour refusal.
  - P293 [hidden] Most lifecycle tests run on hand-written entries (`tests/lifecycle.rs:37-43,195-205`: `linux /vmlinuz`, `root=/dev/fake-a rw`) in a format `new_entry` never writes (no PARTUUID, no `/<prefix>-a/`): they exercise a layout recab does not produce.
  - P294 [hidden] `tests/lifecycle.rs:661` fakes `blkid` by matching any argv ending in `part-a`/`part-b`, tied to `target_root`'s blkid argv by luck; `:50-56,557` fake `systemctl` by exact argv, and on a mismatch the fake silently prints nothing.
  - P295 [hidden] `README.md:42` (`timeout 5`, `editor 0`), `:155` (`rd.emergency=reboot`), `:276` (`/boot`) restate `systemd_boot.rs:758`, `recinit/src/live.rs:51`, `recpart::policy::ESP_MOUNT`; `README.md:288` says the systemd reboot default is `systemctl reboot`, the code runs `systemctl --no-block reboot` (`init_system.rs:269`); the env table (`README.md:271-290`) does not say `stage_dir` is fixed or that `install_target` drops `RECAB_ENTRY_PREFIX`.
  - P301 [surplus] `REQUIREMENTS.md:3-5` keeps its own version and date, untied to the crate's `version.workspace`.
  - P316 [hidden] `testing/install-tests/src/scenarios/ab_update.rs:24` `FAILING_SERVICE = "ab-update-fail"` is copied into `tests/lifecycle.rs:823`; `ab_update.rs:80,125` checks only `FAIL_REBOOT_OPTION`, never the init's crash option.
  - P319 [hidden] There is no `health` test in `tests/lifecycle.rs`; the hand-written jq in `testing/ginux-boot/ginux-boot.sh:143-144` is the `Health` contract's only check.
  - P329 [hidden] `tests/lifecycle.rs:261,272,364,387,487,539` assert literal exit codes and message wording (`state.rs:208`, `systemd_boot.rs:258,293`); `:723` hard-codes the state path `config.rs:121-124` derives.
  - P333 [hidden] README.md:280 says the os-release picks the privilege tool; recab never reads it.

### Leave as is

- P038 [hidden] `src/contract.rs:13` `Contract::new("recab", &["recab"])` names the binary as a literal that `xtask/src/tasks/generate.rs:75-76` matches against TUI manifests and that must equal Cargo's implicit bin name.
  - **Why:** The binary name is a one-line literal that must equal Cargo's bin name; a check in generate costs more than the literal.
- P039 [hidden] `Cargo.toml:14,24`: `target-root` and `tool-contract` live under `tools/recstrap/`, so the layout hides that they are shared libraries.
  - **Why:** Crate location under tools/recstrap is a repo layout choice outside recab; moving shared crates costs more than it saves.
- P080 [special] `src/main.rs:171-176` `trial` prints one line on success, the single exception to quiet-on-success (REQUIREMENTS REQ-CLI-010).
  - **Why:** The trial's one-line success message is the only operator-visible record that a trial was judged before reboot; a deliberate exception.
- P082 [special] `testing/install-tests/src/scenarios/ab_update.rs:206-218` polls `recab status` with a 2 s sleep until the trial service commits.
  - **Why:** Waiting for a reboot and an asynchronous commit inside a VM has no event to wait on; a bounded poll is the requirement.
- P083 [special] `testing/install-tests/src/scenarios/ab_update.rs:245-251` searches for the first `{` to skip console noise before `recab status --json`.
  - **Why:** The serial console interleaves output with the command's; tolerating noise before the JSON is the transport's requirement.
- P139 [special] `loader_with_default` writes `timeout 5`/`editor 0` only when it creates loader.conf (`src/systemd_boot.rs:758`).
  - **Why:** recab must not overwrite an operator's loader.conf settings; defaults belong only in a file recab creates.
- P140 [special] `install_loader` refuses a target with two architectures' systemd-boot (`src/systemd_boot.rs:710`) and knows only two arches (`:38`).
  - **Why:** An ESP boots one architecture's loader; an image shipping two is ambiguous and must stop the install.
- P159 [hidden] `installed-system.rhai:396` and `recsetup/plan.rs:141-155` share the A/B `/var/home` fact.
  - **Why:** The A/B /var/home fact is shared by the builder and recsetup, outside recab; recorded for their owner.
- P193 [special] `testing/install-tests/src/scenarios/ab_update.rs:64-66` runs `ssh-keygen -A` only so the `prefix etc/ssh/ssh_host_` carry has something to carry.
  - **Why:** Installed systems leave sshd off, so the scenario must create host keys to test that they are carried.
- P233 [special] `src/init_system.rs:138,205` sleep in a 250 ms poll loop (`POLL`, `:107`) while waiting for startup.
  - **Why:** OpenRC offers no "wait until startup ends" call, and the systemd wait needs a deadline; a short poll is inherent.
- P240 [special] `tests/lifecycle.rs:253-257` `mutations_refuse_without_root` skips itself as root and reports a pass.
  - **Why:** A non-root refusal cannot be observed when the tests run as root; skipping then is correct.
- P242 [special] `tests/lifecycle.rs:557-561,832` a fake `systemctl` that sleeps 30 s and real 1 s timeouts simulate a startup that never finishes.
  - **Why:** A deadline test needs real elapsed time; one second is cheap.
- P303 [special] `src/systemd_boot.rs:506` treats an empty efivar string as unset.
  - **Why:** systemd-boot and bootctl treat an empty variable as unset; recab must read it the same way.
- P313 [hidden] `distro/builder/guest/installed/system-setup.sh:12-17` reads `REC_INIT`/`REC_PACKAGES` by sourcing os-release itself, a second parser beside `distro_profile::Profile::from_os_release`.
  - **Why:** system-setup.sh parsing os-release is the builder guest's own duplication of distro_profile, outside recab; recorded for its owner.
- P315 [hidden] `distro/builder/src/recipe/tools.rs:12-26` (`TOOLS`) and `distro/builder/src/pipeline/live_tools.rs:38-47` (`LIVE_TOOLS`) are two lists of the same tools that must agree.
  - **Why:** The builder's two tool lists duplicate the builder's own fact, outside recab; recorded for the builder owner.
- P322 [special] `testing/ginux-boot/levitate/expect:11-34` and `acorn/expect:8` accept known failing units as passes, and say (`:6-7`) they have not been re-recorded since recab's verdict.
  - **Why:** ginux-boot's known-gap lists are that harness's own record; re-recording them is its maintenance, not recab's structure.
- P055 [needless] `Cargo.toml:15` depends on all of `recpart` for two mount constants (`config.rs:13`), `InstallMode` (`main.rs:9`) and `Role` (`stage.rs:24`).
  - **Why:** recpart owns the mount points, install modes and slot roles; depending on it is depending on the owner of those facts, and splitting a policy crate out of recpart is not worth it.
- P249 [hidden] User-facing messages in the state machine and backend name CLI commands (`src/state.rs:170,175,182,208,220` 'recab commit'/'rollback'/'set-next'; `src/systemd_boot.rs:294` 'recab stage'): renaming a command means editing both.
  - **Why:** Actionable refusals (REQ-ERR-002) must name the command to run; a command rename is rare and a grep finds every message.
- P282 [needless] `PersistentState.last_trial` is `TrialRecord` (`src/state.rs:9,31`), the public status contract type exported to TypeScript (`lib.rs:101`): the persisted state and the status JSON change together.
  - **Why:** status reports the stored record as it is; the persisted record and its JSON change together by design. The type now lives in state.rs with the rest of the state file.
- P032 [hidden] `src/error.rs:15` (`ErrorPayload`: `code: u8` + `class`) and `tools/recstrap/src/output.rs:14-25` (`code: String`, `remediation`) are two different "schema 1" error payloads; `output.rs:2` calls its own "the one error shape every tool writes".
  - **Why:** recab's classes and exit codes are its documented contract (REQ-ERR-001) and recstrap's E0NN codes are recstrap's; one shape for every tool is a cross-tool redesign outside this untangle.
- P070 [special] `src/lib.rs:406-408` a failed save of the failed-trial record is swallowed into a warning and the reboot goes ahead.
  - **Why:** Rebooting out of a failed trial matters more than recording it: a state partition that cannot be written must not keep a broken slot running. The warning is the right outcome.
- P074 [special] `src/store.rs:109-145` an audit-log write failure is swallowed into a warning.
  - **Why:** The audit log is a record, not state (README, Persistence): a failed append must not undo or block a boot-selection change that already happened.
- P125 [special] `known_good` (`src/state.rs:76-78`) falls back to the loader default when no known-good slot is recorded; `committed`, `rollback_target`, `plan_stage` and status all inherit this missing-state case.
  - **Why:** A state file can be lost while the slots remain (the state partition reformatted or wiped); falling back to the boot loader's default as the known-good slot is the recovery the system needs, and status shows it.
- P290 [hidden] PARTUUIDs compare lower-case by two owners: `RootSelector::parse` (`src/systemd_boot.rs:862-863`) and `target_root::partuuid` (`tools/recstrap/target-root/src/lib.rs:250-252`).
  - **Why:** The kernel command line and blkid are two sources of PARTUUIDs; each normalises its own text once, which is applying the comparison rule, not restating it.
- P133 [special] `src/lib.rs:485` checks bootability only for `slot != active`.
  - **Why:** The running slot booted from its entry, so it can boot; checking it again would refuse a commit or rollback to the slot that is running.
- P135 [special] `parse_entry_name` (`src/systemd_boot.rs:722-726`) accepts names without `.conf` ("bootctl reports names without .conf") and strips directories: for values written by other tools.
  - **Why:** systemd-boot matches entry ids with or without .conf and in any case (bootctl and its menu write them so); recab must read the variables the way the loader does.
- P101 [hidden] The `root=` grammar is parsed by recab (`src/systemd_boot.rs:850-872,914-920`) and by recinit's `init_installed.template:57-93`, accepting different forms.
  - **Why:** recinit's initramfs reads the kernel command line for any boot loader; recab writes only root=PARTUUID=, which both read the same way.
- P191 [special] `src/init.rs:86-98` warns and continues when efivars are missing.
  - **Why:** Without efivars the stale variables cannot be reset, and installing must not fail for it; the warning says what may happen and that stage resets them later.
- P201 [hidden] `src/init_system.rs:184-199,218` hard-code what ends startup (`default`; `single`/`shutdown`/`reboot`) and `BOOT_RUNLEVELS = sysinit, boot, default`, which busybox's inittab chooses in `distro/builder/recipes/installed-system.rhai:227,245`; nothing links them.
  - **Why:** sysinit/boot/default and the single/shutdown/reboot softlevels are OpenRC's own standard runlevels; the inittab follows them, so the adapter must know them.
- P202 [hidden] `src/init_system.rs:229-234` requires every boot-runlevel service started, a set that includes `recab-trial` itself (`package.rs:39`, `boot` runlevel).
  - **Why:** The trial service is started (backgrounded) before the default runlevel is reached, so it never counts as failed; excluding it by name would add the special case.
- P203 [hidden] The verdict depends on the text formats of `systemctl list-units --failed --plain` (`src/init_system.rs:159-169`) and `rc-status --crashed` (`:243`); the test fakes copy the exact argv (`tests/lifecycle.rs:52-54`, including `--no-block reboot`).
  - **Why:** The init tools' text output is their only interface for these facts; each is parsed in one place.
- P306 [special] `src/services/recab-trial.openrc:12` sends output to `/dev/kmsg` because OpenRC has no journal: the one per-init logging path behind REQ-CLI-010.
  - **Why:** OpenRC has no journal; the kernel log is where a boot's record survives, so the OpenRC service sends the one line there (now through openrc-run's output_log).
- P309 [hidden] `src/lib.rs:425-428` returning `Err(TrialFailed)` after requesting the reboot also leaves `recab-trial.service` failed on systemd, on a run that did its job.
  - **Why:** Exit 23 is recab's documented signal that a trial failed (REQ-ERR-001); the unit marked failed records that in the journal on purpose.
- P018 [hidden] `src/lib.rs:347` reads `Store::exists` and `firmware_trial()` before the lock, and `:371`, `:375` read them again under it.
  - **Why:** The gate before the lock is read-only on purpose, so a boot that is no trial needs neither root nor the lock; the decision re-reads everything under the lock.
- P023 [hidden] `src/lib.rs:195-202,425-428`: a failed trial that rebooted as designed is returned as `Err(TrialFailed)` (exit 23), not as a `TrialOutcome`; callers must know this error means success.
  - **Why:** Exit 23 is recab's documented signal that a trial failed (REQ-ERR-001), also after the reboot it asked for.
- P057 [surplus] `src/lib.rs:322-334` `set_next`, `commit`, `rollback` are one-line wrappers passing a request and `|_| {}` to `change()`.
  - **Why:** REQ-LIB-002 requires set_next, commit and rollback as library functions; each is now one line naming its request.
- P128 [special] `plan_stage` refuses when `sel.default == Some(slot)` (`src/state.rs:178-184`), a guard for one case (after a one-shot boot of the running slot).
  - **Why:** A default naming the inactive slot means its freshly written system would boot untried; refusing that is the requirement.
- P130 [special] `judge_trial` (`src/state.rs:323-327`) turns a non-trial into `Fail` when state is missing and the firmware says one-shot ("/var failed to mount"); `trial` repeats that check early (`lib.rs:347`), so `firmware_trial` runs up to three times per run.
  - **Why:** A failed one-shot boot with no state on /var may be /var itself failing to mount: rebooting into the firmware's default is the recovery, and nothing is written to the unmounted /var (F-T2).
- P131 [special] `judge_trial` returns `Ignore` for every `Inconclusive` startup (`src/state.rs:308-310`).
  - **Why:** A shutdown or an unreachable init during startup gives no verdict on the boot; judging it would commit or reject on no evidence.

## Progress
- [x] **F12 Layering and the public surface.** `Slot` and `TrialRecord` live in `state.rs` (no clap derive; one `FromStr`/`Display` spelling); `status.rs` owns `Status`/`SlotView` and builds the slot views from small backend queries; `init.rs`/`stage.rs` own their request types; `lib.rs` re-exports only what other crates use; the store's pass-through file helpers are gone (callers use `target_root`), its fields are private and it owns the clock; the schema check lives in `state.rs`; `InitSystemName` is gone (`distro-profile` gained a `contracts` feature). Left as is, with reasons: P055, P249, P282. P008 moved to F11 (the trial message). Moving `System`/`change()`/`trial()` out of `lib.rs` is left to F04/F06, which rewrite them. **Behaviour changes (intended):** `status` text also lists each slot's entry, kernel dir, root, device, bootable and trial options; `health` text names the init (`systemd startup finished (running)`); a wrong argument combination for `init` or `stage` is a clap usage error (exit 2) instead of exit 1 or a panic. Checks: `cargo test -p recab` 20/20, `cargo check -p install-tests -p levitate-xtask`, `cargo xtask gen` regenerated the recab schemas and `recab-tui/src/generated/recab.ts`.
- [x] **F08 One error model.** One class table in `error.rs` (name, code, tag stated once, serde/schema/TS take the name from it); `refuse`, `classified` and `or_class` are the only helpers and every module imports them from `error`; the tag is no longer part of the message (the CLI prints `recab: [TAG] message`, the JSON's `message` has no tag); a global `--json` makes every command print its error as JSON; reading the ESP or state is classified (21), a failed firmware-variable write is `BackendUnavailable` (20) everywhere; the same-partition, loader and carried-list refusals are classified; `rc-status --crashed` failing with nothing listed is an error; the not-found probes use new `target_root::{metadata, canonical, read_optional_bytes}`. Left as is: P032, P070, P074 (reasons under Leave as is); P075 was done by F12. The refusals inside `stage`'s write path (`stage.rs` carry and fstab) are classified by F06, which moves them before the first write. **Behaviour changes (intended, bugs fixed):** `recab status` with an unreadable ESP exits 21, not 1 (P084); a firmware write that fails with nothing changed exits 20, not 1 (P289, lifecycle test updated); `--json` errors for every command (tool-help regenerated); error text is `recab: [TAG] context: message`. Checks: `cargo test -p recab` 20/20; `cargo xtask gen` (recab schemas, recab-tui types, tool-help; it also re-ordered recstrap's and recpart's generated types, identical content, stable on rerun).
- [x] **F01 The system recab looks at (`Target`).** `target.rs` replaces `config.rs`: one value for the running system or one installed under a root, with every path (root, ESP, state dir, lock, `/proc`, stage dir, OpenRC dirs), the programs, this machine's firmware (none for an installed system) and the os-release read once by one rule (entry prefix = lower-cased `ID`, `init` and `flavour` from `REC_*`, the same error class for running and installed). The backend, store, init adapter, `init` and `stage` take it; nothing else reads the environment, `/`, `/proc` or an os-release. Whether the variables are efivarfs is read with `statfs` (no test-only switch; the immutable flag is cleared only on efivarfs). `init` takes the run lock and checks its mounts (slot A or the mutable root at `--root`, the ESP, and `/var` for A/B). `Store::load` says when there is no state. `install-unit` requires `--package`. The `openrc-init` reboot branch is gone (P231, moved here): OpenRC reboots through busybox init's `reboot`. `RECAB_ENTRY_PREFIX` is gone (nothing set it). `target-root` gained `mount_table_at`; `tool-contract` now writes each export's declarations sorted, because ts-rs's dependency order changed between builds and made `cargo xtask gen --check` flap (all generated TS reordered once). Moved to F04: P263, P269, P279, P321 (the trial gate and its A/B fact); to F10: P100. Left as is: P125, P290. **Behaviour changes (intended):** `status --root` no longer mixes in this machine's firmware variables (an installed system's default is its loader.conf); `init` refuses a root, ESP or state partition that is not mounted, or a `--slot-a` that is not the mounted root; the override variables changed (README, Environment Overrides). Checks: `cargo test -p recab` 20/20; `cargo check -p install-tests -p levitate-xtask`; `cargo xtask gen --check` current after a rebuild; recab-tui, install-hub, base-system and disk-plan typecheck. Not run: the VM install (`init`'s mount check meets recsetup's real mounts there).
- [x] **F02 One owner of the boot partition** and **F03 Boot selection through one operation** (one commit: they rewrote the same file). `entry.rs` holds the naming (`layout_name`, `entry_name`), one grammar for entry files (a typed `Entry`: options, boot files including `devicetree`, root, initrd) and loader.conf; `efivars.rs` holds the variables (an enum, one path and encoding, efivarfs or plain files by `statfs`, `can_change`). The backend reads the selection once (raw values kept private) and changes it only through `select(change)`, which checks its own preconditions (firmware writable, target slot bootable, a stale default pointed at the checked one) and reports exactly what it changed (`SelectError`: firmware variables, entry options, loader.conf restore); `init` is read lazily, so commit and rollback no longer need `REC_INIT`. A slot write is a token: `begin_slot_write` resets stale variables and removes the kernel, `SlotWrite::finish` writes the entry first and copies the kernel last, so a half-written slot can never boot. `init` writes the layout through `install_layout` (replacing the installed kernel directory), then this machine's firmware, then the state (selection before state), and leaves this machine's firmware alone when it runs its own A/B install (D-5). A default variable naming another bootable entry is a fact (`Selection.foreign_default`): status shows no default slot, commit repairs it, stage and set-next refuse until then (it used to be ignored, then overwritten). Roots are matched by `root=PARTUUID=` only, as recab writes them. Left as is: P101, P133, P135, P191. P300 moved to F04. **Behaviour changes (intended, bugs fixed):** a refused stage changes no firmware variable (F-S1; it is checked before `begin_slot_write`); a firmware default naming a foreign entry is no longer silently overwritten (P141); `init` does not rewrite the host's firmware when installing from an installed system (D-5, P170); re-running `init` without `--initrd` leaves no stale initramfs (P278); a crash during stage's kernel install cannot leave a bootable half-written slot (F-S5); a kernel command line naming its root other than by PARTUUID is refused with that reason; refusal and partial-update messages name what changed. Tests: fixtures name roots by PARTUUID (the form recab writes); the init test gives this machine an empty ESP; three expectations follow the new behaviour. Checks: `cargo test -p recab` 20/20, consumers compile, `cargo xtask gen --check` current.
- [x] **F11 The init-system adapter.** `Health` carries the startup verdict itself (`startup`: running, failed, emergency, timed out, inconclusive; `finished` derives from it) and one `describe` gives the words a trial record, a trial message and `health`'s text use; `trial` waits through the same `init_system::health` as `recab health`; the positional `Waited` tuple and `Startup`'s per-init payloads are gone; no answer from `systemctl is-system-running` reports what systemctl said. The trial deadline (`TRIAL_TIMEOUT_SECS`) belongs to the adapter; the services no longer repeat it. The service templates take the description, the binary, its name and the systemd target from `package.rs` (one source each) and an unfilled placeholder is an error; the OpenRC service backgrounds and logs through openrc-run itself (`command_background`, `output_log=/dev/kmsg`), so the hand-written stop and the always-0 `eend` are gone, and it says why it runs after sysfs. `install-unit --package` is `--init`, with `--bin` (the builder now passes its own install path). Left as is: P201, P202, P203, P306, P309. Moved to F10: P204, P225, P239 (the builder's side); to F13: P209 (README, scenario budgets). **Behaviour changes (intended):** `health --json` gains `startup`; a committed trial's record reads 'finished starting'; `health` text names how startup ended in the same words as a trial. Checks: `cargo test -p recab` 20/20; install-tests and xtask compile; `cargo xtask gen` (health schema, recab-tui types). **Not run:** the OpenRC service under real OpenRC (VM).
- [x] **F04 One trial lifecycle in the state machine.** `state.rs` plans every transition: `plan(Request)` for set-next, commit, rollback and stage (`plan_stage` is gone), and `judge` for a boot (commit a healthy trial, record and select the fallback of a failed one, forget a trial another boot used up), with `Transition.boot` optional (no dead `BootChange::None`). Each slot's bootability is part of the `Selection`, so 'never select a slot that cannot boot' is the planner's rule. `system.rs` holds the one `apply` (selection, then state stamped with one operation label, then audit) used by every command, the failed trial included (selection before state there too); `Store::save` takes no label. `trial.rs`: `recab trial` waits for startup first, then asks whether this is an A/B system (both entries) or a one-shot boot, and only then takes root and the lock, so a non-trial boot needs neither; a permission or lock error is no longer read as 'the ESP broke'. `init` audits its first state. Operation labels are `name-slot` (`rollback-A`, was `rollback-to-A`); `set-next` is described as scheduling, not staging. Left as is: P018, P023, P057, P128, P130, P131. **Bugs fixed (from the ideal-filetree audit, now with lifecycle tests):** set-next of the known-good slot during an uncommitted trial is refused, so the fallback's entry never gets trial options (F-M1); a trial whose one-shot another boot used up is forgotten, so a later menu pick is not judged (F-M2); a one-shot without recab's record is scheduled again instead of taken as done (F-M3); a failed trial selects its fallback before saving state (F-M4); a non-root run on a non-trial boot no longer fails (F-M5); a failed trial with no state found writes nothing into the unmounted /var (F-T2); the trial decision runs after startup, when /var and the ESP are mounted (F-T1). Checks: `cargo test -p recab` 23/23 (three new tests, each failing on the old code); consumers compile; `cargo xtask gen` (tool-help). **Not run:** the VM trial scenarios.
