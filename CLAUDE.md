# recab

Purpose: own system updates and symmetric A/B continuation for LevitateOS and
AcornOS. Follow `REQUIREMENTS.md` for the product contract and `README.md` for
the intended lifecycle and current implementation gaps.

Use `IMPLEMENTATION.md` as the active implementation plan. It covers this tool
and its Recipe, installer, boot and frontend callers. Earlier untangling logs
are historical evidence, not the product plan.

Installation must populate A and B with equivalent personalized systems and
boot artifacts. The running system root stays immutable; recab prepares the
inactive system using only differences, coordinates changes and downloaded
updates through Recipe in that same preparation, then trial-boots and commits
it or falls back. Everything possible A→B must also work B→A. Never erase pending
changes to apply an update, or overwrite the fallback of an uncommitted trial.

The current destructive image-staging implementation is incomplete. Empty B at
installation, formatting on each continuation and limiting recab to boot
selection are not the intended design. Recipe owns software operations inside
Ginux; recab owns their destination and the system lifecycle.

Keep this tool:

- Small and explicit (Unix tool behavior)
- Easy to call from installers/tests
- Conservative about making boot-affecting changes

Prefer implementing slot selection via a single well-defined backend (likely systemd-boot), with a clean Rust API so other crates can reuse it.

