# ADR 0011: Relax `unsafe_code` from `forbid` to `deny` and Scope It to Two Audited Modules

- **Status:** Accepted
- **Date:** 2026-08-02
- **Supersedes:** _none_

## Context

`Cargo.toml` set `unsafe_code = "forbid"`. That was the correct default while the crate only read
the filesystem through `std::fs`, and `src/lock.rs` and `src/mount/resolve.rs` both carry comments
recording where it cost accuracy and deferring the question to "the change that introduces the
Windows platform backend". This is that change.

The `directory-link-backends` specification requires four things the standard library does not
expose on the supported release targets:

- **Atomic no-replace placement.** `std::fs::rename` maps to `rename(2)` on Unix and to
  `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING` on Windows. Both replace an existing destination.
  The specification requires placement to fail instead, and the design states that "a backend that
  cannot guarantee no replacement returns unsupported before plan application; it does not emulate
  the operation with a check-then-rename race". The flags that forbid replacement —
  `renameatx_np(RENAME_EXCL)` and handle-bound
  [`SetFileInformationByHandle(FileRenameInfo)`][set-info] with `ReplaceIfExists = false` — have no
  safe wrapper.
- **Junction creation.** `std::os::windows::fs` creates directory symbolic links only. Writing a
  mount-point reparse buffer requires `DeviceIoControl` with `FSCTL_SET_REPARSE_POINT`.
- **Reparse-tag inspection.** `std::fs::symlink_metadata` reports both a symbolic link and a
  junction as a symlink and exposes no stable reparse tag. Removal must "verify the reparse tag,
  normalized target, expected created-link kind, and platform identity", which needs
  `FSCTL_GET_REPARSE_POINT`.
- **Stable file identity.** `MetadataExt::volume_serial_number` and `MetadataExt::file_index` are
  behind the unstable `windows_by_handle` feature. Ownership verification and link-cycle detection
  both want a real identity rather than a path spelling.
- **Object-bound rename and removal.** A pathname check followed by `MoveFileExW` or
  `RemoveDirectoryW` can act on a replacement. Windows can instead retain the no-follow handle that
  supplied attributes, identity, and reparse data. [`FILE_RENAME_INFO`][rename-info] renames that
  object without replacement, and [`FILE_DISPOSITION_INFO_EX`][disposition-info] removes that
  object's visible link with POSIX semantics when the verified handle closes. The standard library
  exposes neither handle mutation nor the variable-length rename layout. ADR 0016 records why the
  extended disposition contract replaced basic delete-pending disposition and why identity, rather
  than mutable reparse metadata, remains the authority after handle verification.
- **Durable journal replacement on Windows.** `std::fs::rename` replaces a journal but exposes no
  write-through option. The journal must not authorize the next filesystem mutation until its
  namespace replacement is durable, so the audited boundary also wraps `MoveFileExW` with
  `MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH`. Microsoft documents successful return from
  that flag combination as the file having actually moved on disk; a checksum alone would only
  detect torn contents, not a lost or rolled-back directory entry.

`forbid` cannot be lifted by an inner `allow` anywhere in the crate; that is its defining property
and the reason it is stronger than `deny`. So the choice is binary: keep `forbid` and drop a
normative requirement, or move to `deny` and make every exception explicit and reviewable.

## Decision

`Cargo.toml` sets `unsafe_code = "deny"`. Exactly two modules carry `#![allow(unsafe_code)]`:
`src/link/unix_ffi.rs` and `src/link/windows_ffi.rs`. No other module may add one.

[set-info]: https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-setfileinformationbyhandle
[rename-info]: https://learn.microsoft.com/en-us/windows/win32/api/winbase/ns-winbase-file_rename_info
[disposition-info]: https://learn.microsoft.com/windows-hardware/drivers/ddi/ntddk/ns-ntddk-_file_disposition_information_ex

Both modules follow the same rules, which a reviewer can check by reading the two files:

- one focused filesystem operation per wrapper, with every `libc` or Win32 failure converted to
  `io::Error` before it leaves the boundary and only owned Rust values returned;
- no `libc` or `windows_sys` type crosses the module boundary;
- every `unsafe` block carries a `SAFETY` comment naming the invariant that makes it sound;
- handles are adopted into `std::os::windows::io::OwnedHandle`, so closing them is the standard
  library's responsibility and no `Drop` implementation here can leak or double-close one;
- reparse-buffer codec arithmetic lives in safe `src/link/reparse.rs`; the Win32-required
  variable-length `FILE_RENAME_INFO` allocation stays inside `src/link/windows_ffi.rs`, uses checked
  byte arithmetic and aligned owned storage, and has x86/x64 ABI layout tests.

## Alternatives

**Keep `forbid` and drop junction support.** Rejected because it removes the fallback the
specification requires. Creating a directory symbolic link on Windows needs Developer Mode or an
elevated process; a default Windows installation has neither, so `auto` would fail for the common
case rather than degrading to a junction.

**Keep `forbid` and depend on a junction crate.** Rejected because it relocates the `unsafe`
rather than removing it, and relocates it somewhere this project does not review. The audited
surface here is roughly 150 lines and is exercised by this crate's own native tests; a dependency
would add a supply-chain obligation and a `deny.toml` entry for the same code written by someone
else. It also would not solve no-replace placement or reparse-tag inspection, so `forbid` would
still have to be relaxed.

**Keep `forbid` and shell out to `mklink /J`.** Rejected on three counts. It violates the
platform-native-values invariant, because a shell reinterprets quoting in paths that may contain
spaces, Japanese characters, and backslashes. It requires spawning a child process during apply,
which the transaction layer must not do. And it provides no way to read a reparse tag back, so
verified removal — the property that keeps SkillMount from deleting a user's Skills — would have no
evidence to check.

**Keep `forbid` and emulate no-replace placement with an existence check.** Rejected explicitly by
the design: "a check-then-act fallback is not acceptable". The window between the check and the
rename is exactly the window the guarantee exists to close.

**Move to `allow` rather than `deny`.** Rejected as strictly worse than `deny` with two module
allows: it makes new `unsafe` invisible in review anywhere in the crate, while buying nothing.

## Consequences

- The crate no longer has a mechanical guarantee that it contains no `unsafe`. The replacement
  guarantee is narrower but checkable: `rg '!\[allow\(unsafe_code\)\]' src` lists every exception,
  and it must return exactly two files. A reviewer who sees a third in a diff should treat it as a
  design question, not a detail.
- Two dependencies are added, both pinned exactly and both `MIT OR Apache-2.0`, which `deny.toml`
  already allows: `libc = "=0.2.189"` for `cfg(unix)` and `windows-sys = "=0.61.2"` for
  `cfg(windows)`. `windows-sys` is enabled with five feature groups and no others. Neither carries a
  runtime deployment obligation; both resolve to declarations that link against the platform's own
  libraries.
- `libc` is a `cfg(unix)` rather than a `cfg(target_os = "macos")` dependency. Linux is not a
  release target, but the CI quality job runs the whole suite on Ubuntu, and a placement primitive
  that only existed on macOS would make every placement test on that runner unverified. The Linux
  branch uses `renameat2(RENAME_NOREPLACE)`; any other Unix fails closed.
- `windows-sys` requires Rust 1.71 and `libc` requires 1.65, both below the crate MSRV of 1.85.0, so
  the MSRV is unchanged.
- The later locking change already made `LockResourceIdentity::physical` use the link layer's
  `PlatformIdentity`. This hardening reuses that contract; it introduces no second identity type,
  lock-key version, journal migration, or dependency. ADR 0016 separately raises the Windows runtime
  baseline to Windows 10 version 1709 for POSIX handle disposition.

## Verification

- `src/link/reparse.rs` tests reject a truncated buffer, an over-declared length, a name that
  reaches outside its path buffer, an odd name length, an interior NUL, and an unowned reparse tag.
  These run on every host, so a mistake in the layout arithmetic fails on macOS and Linux too rather
  than only on a Windows runner.
- `src/link/winpath.rs` tests pin namespace normalization, including the `\??\` and `\\?\UNC\`
  forms, drive-letter case folding, and the refusal to case-fold anything else.
- `src/link/unix_tests.rs` and `src/link/windows_tests.rs` assert a sentinel file inside every
  source directory after every removal path, which is what fails if a removal ever descends.
- `src/link/unix_tests.rs::exactly_one_of_two_racing_placements_wins_the_destination` and its
  Windows counterpart run two real threads through one destination and require exactly one winner.
  A check-then-rename emulation fails these.
- Windows native tests replace staged pathnames after handle verification and require placement to
  affect the verified object. Removal tests require the verified handle to exclude a competing
  delete-capable rename and ordinary reparse writer, exercise the documented attribute-only mutation
  exception without traversing either target, and require POSIX disposition to remove the visible
  name while an unrelated inspect handle remains open. The `FILE_RENAME_INFO` layout test runs under
  both native x86 and x64 jobs.
- `tests/read_only.rs` continues to snapshot the project, the Skill source, and a redirected home
  directory around every `inspect` and `--dry-run` path, so the new no-follow handle opens cannot
  become writes without a failure.
- Enforcement of the two-module rule is a review step. `cargo clippy` reports a third
  `#![allow(unsafe_code)]` as allowed code, not as a lint, so no automated check catches it.
