# Agent Instructions

## Project

APC (Arch Package Commander) is a Rust 2021 terminal UI for Arch Linux package management. It uses `libalpm` through the `alpm` crate and reads pacman configuration through `pacmanconf`. The current application and its unit tests are in `src/main.rs`.

## Build and test

Run these checks after completing a set of related edits:

```sh
cargo test --locked
cargo build --locked
```

Building requires a Rust toolchain and the system `libalpm` development files. On Arch Linux, install the `pacman` package and `pkgconf` (along with the normal build toolchain).

The unit tests should not perform real package transactions or require root privileges. Keep tests isolated with temporary files and directories.

## Change guidelines

- Keep changes focused; avoid adding abstractions or dependencies without a demonstrated need.
- Do not perform work beyond the explicitly agreed task queue without the user's permission; ask before expanding scope.
- Do not add tests that assert pacman/libalpm semantics unless those semantics have been verified against authoritative documentation or source for the supported version, or by an integration test against the actual libalpm implementation.
- Preserve pacman and libalpm behavior, especially signature-verification and configuration semantics.
- Preserve IgnorePkg entries owned by the user's pacman configuration. APC-managed lock changes must not silently discard external exclusions.
- Treat file persistence and libalpm state changes as a coordinated operation; report failures and verify rollback where practical.
- Add regression tests for behavior changes and edge cases.
- Do not use shelling out to pacman as a substitute for libalpm transaction handling.
