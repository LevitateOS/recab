# recab

Purpose: manage A/B slot boot selection + commit/rollback semantics for immutable-by-default OS images.

Keep this tool:

- Small and explicit (Unix tool behavior)
- Easy to call from installers/tests
- Conservative about making boot-affecting changes

Prefer implementing slot selection via a single well-defined backend (likely systemd-boot), with a clean Rust API so other crates can reuse it.

