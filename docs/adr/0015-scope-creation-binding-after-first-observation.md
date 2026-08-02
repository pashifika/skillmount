# ADR 0015: Scope Creation Binding after the First Observation

- **Status:** Accepted
- **Date:** 2026-08-02
- **Supersedes:** _none_

## Context

Review of the ownership-hardening change found a boundary the design had treated as proof but the
platform APIs do not provide. The backend creates a transaction-unique symbolic link or directory
by pathname and only then performs its first no-follow observation. A non-cooperating same-user
process can replace that pathname between the two calls. If the replacement has the expected kind
and, for a link, target, the first observation cannot distinguish it from the object the create call
made. Its identity can then be adopted and later moved or removed as session-owned. For Windows
junction creation, the adopted object is still a plain directory at that boundary; SkillMount then
writes mount-point reparse data into that same directory through the adopted handle.

This is a capability gap, not an omitted comparison. Apple's [`symlink(2)`][apple-symlink] and
[`mkdir(2)`][apple-mkdir] return only success or failure. Microsoft's
[`CreateSymbolicLinkW`][create-symbolic-link] and [`CreateDirectoryW`][create-directory] likewise
return status rather than a handle. [`CreateFileW`][create-file] can open a directory handle but
cannot create a directory. `NtCreateFile` can create a directory and return its handle, but that
solves only directory creation: constructing a symbolic-link reparse point directly through
[`FSCTL_SET_REPARSE_POINT`][set-reparse] requires `SE_CREATE_SYMBOLIC_LINK_NAME`, while
`CreateSymbolicLinkW` is also required to preserve Developer Mode's documented unprivileged path.

[apple-symlink]: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/symlink.2.html
[apple-mkdir]: https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/mkdir.2.html
[create-symbolic-link]: https://learn.microsoft.com/windows/win32/api/winbase/nf-winbase-createsymboliclinkw
[create-directory]: https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-createdirectoryw
[create-file]: https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-createfilew
[set-reparse]: https://learn.microsoft.com/windows/win32/api/winioctl/ni-winioctl-fsctl_set_reparse_point

## Decision

Creation evidence begins with the first successful no-follow observation; it proves which object
subsequent operations describe, but it does not prove atomic continuity from a preceding
path-based create call. SkillMount MUST retain and report any path when failure occurs before that
observation, and MUST NOT issue an unchecked pathname rollback. After evidence exists, Windows
rollback, placement, and removal remain bound to the verified handle or identity; Unix retains its
ADR 0014 pathname limitations.

The create-to-first-observation replacement window is an explicit residual risk. SkillMount does
not claim to distinguish a same-shaped replacement installed there. Transaction-unique staging
names reduce accidental contention but are not authority or ownership proof.

## Alternatives

**Treat the first identity read as proof of creation.** Rejected because it proves the currently
observed object, not which object the earlier status-only create call produced.

**Use `NtCreateFile` for every created entry.** Rejected because it can return a newly created
directory handle but does not preserve unprivileged directory-symlink creation. Directly setting a
symbolic-link reparse point requires a privilege the supported `CreateSymbolicLinkW` Developer Mode
path does not require. A partial Windows-only mechanism would enlarge the unsafe and compatibility
surface while leaving the cross-platform link guarantee false.

**Never accept an entry after path-based creation.** Rejected because no supported create API on
macOS, and no supported unprivileged directory-symlink API on Windows, returns the capability such a
rule would require. It would make normal link creation unusable rather than make it safer.

**Change containing-directory permissions or create a private parent.** Rejected because the
project and discovery stores are user-owned state SkillMount must not re-permission, and a process
with the same user authority is not excluded by that policy mutation.

## Consequences

- A non-cooperating same-user process can deliberately cross the create-to-observation window and
  have a same-shaped replacement adopted. For a Windows junction, SkillMount can write mount-point
  reparse data into the adopted empty directory; a later verified mutation can move or remove the
  adopted entry.
- Damage remains bounded by the existing no-recursive contract: link targets are never traversed
  for deletion, regular non-empty directories are retained, and every later mismatch is reported.
- Before first observation, failure is always conservative residue. Once a Windows handle is open,
  later rename, disposition, and rollback retain the stronger object-bound guarantee.
- No CLI, dependency, journal schema, packaging, release-target, or privilege-policy change follows.
  A future supported API that returns the created link capability can supersede this decision.
- The architecture baseline, directory-link delta specification, design, source comments, and
  review evidence must describe initial observation as the evidence boundary rather than proof of
  object birth.

## Verification

- Native Unix and Windows tests inject failures immediately after link or directory creation and
  require the staged pathname to be retained without pathname rollback.
- Native Windows tests inject failures after handle verification and require rollback to act on the
  verified handle; placement and removal tests separately replace the pathname after verification.
- Transaction tests require every retained temporary and final candidate to remain journal-backed
  while independently verified owned entries are still cleaned up.
- Pre-landing review audits every create-to-observation sequence against the status-only API
  contracts linked above. The residual window cannot be closed by an automated assertion without
  replacing those APIs; the tracked review and implementation evidence live under
  `rasen/changes/harden-link-ownership-binding/evidence/`.
