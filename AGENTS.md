# Repository Agent Guidelines (AGENTS.md)

This document contains non-negotiable execution rules, quality gates, and workflow constraints for AI agents (OpenCode, Claude Code, Cursor, Codex) working within this Rust codebase.

---

## 1. Mandatory Pre-Submit Verification Gate

Before completing any task or submitting a patch that modifies Rust code (`*.rs`, `Cargo.toml`, `Cargo.lock`), you MUST execute the following checks in order:

```bash
# 1. Formatting check
cargo fmt --all -- --check

# 2. Strict Clippy check (must pass with zero warnings)
cargo clippy --workspace --all-targets --all-features -- -D warnings

# 3. Comprehensive test suite
cargo test --workspace --all-targets
```

### Gate Execution Rules & Pitfalls
* **No Masked Exit Codes:** NEVER pipe gate commands into `tail`, `head`, or `grep` within a `&&` pipeline (e.g., `cargo clippy 2>&1 | tail -n 5`). Pipelines swallow non-zero exit codes, masking failures as successes. Capture output to a variable or run commands standalone.
* **Run on Final Diff:** Re-run the verification pipeline on the *exact* final commit or file state before declaring work done. Passing earlier in a multi-turn conversation does not validate post-refactor changes.
* **No Quick-Fix Annotations:** Do not suppress compiler warnings or clippy lints with `#[allow(...)]` simply to force a pass. Address the underlying issue structurally.

---

## 2. Rust Code Quality & Architecture Rules

### Ownership & Borrowing
* **Parameter Types:** Prefer slice references over owned or boxed collections in function signatures (`&str` instead of `&String`, `&[T]` instead of `&Vec<T>`).
* **No Borrow-Checker `.clone()` Hacks:** Never resolve ownership or lifetime errors by blindly adding `.clone()`, `Rc`, or `Arc`. Refactor function signatures, lifetime bounds, or struct definitions instead.

### Error Handling
* **No Panics in Production:** Do not use `.unwrap()` or `.expect()` in non-test code paths.
* **Error Propagation:** Use `?` for propagation. Contextualize application errors using `anyhow::Context` and library errors using `thiserror`.
* **Testing Exceptions:** Explicit panics via `.unwrap()` or `.expect()` are permissible only inside `#[cfg(test)]` modules or `tests/` integration testing suites.

### Async & Concurrency
* **Mutex Guards Across `.await`:** NEVER hold a `std::sync::MutexGuard` across an `.await` boundary. Use explicit scope blocks `{ ... }` to drop guards prior to yielding, or use `tokio::sync::Mutex` if state must persist across await points.
* **Blocking Call Offloading:** Never call blocking I/O or long-running CPU computation inside an async task. Delegate work via `tokio::task::spawn_blocking`.

### Control Flow & Functional Idioms
* **Let-Else Guards:** Prefer `let-else` statements for early returns over deeply nested `if let` or complex `match` blocks.
* **Zero-Cost Iterators:** Prefer declarative iterator chains (`.filter_map()`, `.collect()`) over manual imperative loops with dynamic vector allocation.

### Comments
* Do not add comments unless explicitly approved.

---

## 3. Coding Process & Code Quality

* Use https://github.com/nicolaschan/cx, which has a nix flake, to measure code complexity. When designing a change, aim to reduce code complexity.
* When responding to comments, respond to all comments point by point.
* Make sure code works on all common platforms (see the github workflows for a specific list). When not possible to test, check all relevant documentation online.
* Changes must be targeted so that each commit has a singular focus. Make the least changes need to achieve the desired intent.
* Break up changes into sequences of small, targeted commits. This aids in reviewing and future debugging.

---

## 4. Agent Execution Protocol

1. **Context First:** Inspect relevant `mod.rs`, `lib.rs`, and surrounding code before modifying files to ensure consistency with localized patterns.
2. **Incremental Validation:** For non-trivial modifications, run `cargo check` incrementally after small changes rather than generating large volumes of unverified code.
3. **Escalate Unresolved Errors:** If a borrow-checker or architectural issue cannot be cleanly resolved without violating these rules, state the compiler error directly and present explicit technical trade-offs before proceeding.
4. **Debugging:** After an unexpected result, or a failure, always root-cause the problem first. Do **not** simply implement the first possible option for "resolving" the problem --- this usually is addressing a symptom, not the underlying problem.
