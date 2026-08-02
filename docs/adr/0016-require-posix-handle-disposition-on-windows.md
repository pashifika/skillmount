# ADR 0016: Require POSIX Handle Disposition on Windows

- **Status:** Accepted
- **Date:** 2026-08-02
- **Supersedes:** _none_

## Context

Ownership-bound removal originally used basic `FileDispositionInfo` on the same no-follow handle
that supplied the entry identity. Review found that this binds the mutation to the right object but
does not prove cleanup is complete. Microsoft documents basic disposition as delete-pending: the
entry is not actually deleted until every open handle closes. A successful call could therefore be
reported as `Removed`, the journal could become terminal, and the mount name could remain visible
behind another delete-sharing handle. A delete-capable handle can also request cancellation before
the pending deletion completes.

Review also disproved a stronger share-mode premise. `CreateFileW` excludes ordinary read, write,
and delete access according to `dwShareMode`, but explicitly exempts attribute access. The
`FSCTL_SET_REPARSE_POINT` protocol accepts either `FILE_WRITE_DATA` or `FILE_WRITE_ATTRIBUTES`.
Therefore no `CreateFileW` share-mode combination can make a verified junction's kind and target
immutable: an attribute-only handle can change reparse data while preserving object identity. That
mutation cannot redirect an already-open handle, rename the object, or cancel its disposition, and
handle disposition never traverses the reparse target.

Microsoft documents extended disposition with `FILE_DISPOSITION_POSIX_SEMANTICS` differently: the
link is removed from the visible namespace when the POSIX delete handle closes even if unrelated
handles remain open. `FileDispositionInformationEx` is available beginning with Windows 10 version
1709.

- [`FILE_DISPOSITION_INFORMATION` semantics][basic-disposition]
- [`FILE_DISPOSITION_INFORMATION_EX` and POSIX semantics][extended-disposition]
- [`FileDispositionInformationEx` platform availability][information-class]
- [`NtSetInformationFile` delete-or-cancel contract][set-information]
- [`CreateFileW` access and share-mode contract][create-file]
- [`FSCTL_SET_REPARSE_POINT` access contract][set-reparse]

[basic-disposition]: https://learn.microsoft.com/windows-hardware/drivers/ddi/ntddk/ns-ntddk-_file_disposition_information
[extended-disposition]: https://learn.microsoft.com/windows-hardware/drivers/ddi/ntddk/ns-ntddk-_file_disposition_information_ex
[information-class]: https://learn.microsoft.com/windows-hardware/drivers/ddi/wdm/ne-wdm-_file_information_class
[set-information]: https://learn.microsoft.com/windows-hardware/drivers/ddi/ntifs/nf-ntifs-ntsetinformationfile
[create-file]: https://learn.microsoft.com/windows/win32/api/fileapi/nf-fileapi-createfilew
[set-reparse]: https://learn.microsoft.com/openspecs/windows_protocols/ms-fsa/4aeefef8-92c3-4abc-af7a-a610caf8a165

## Decision

Windows removal and verified creation rollback MUST use `FileDispositionInfoEx` with
`FILE_DISPOSITION_FLAG_DELETE | FILE_DISPOSITION_FLAG_POSIX_SEMANTICS` on the verified no-follow
handle. The handle MUST exclude ordinary write and delete access. The backend MUST verify kind,
target, and identity through that handle before mutation; after that eligibility check, the retained
identity is the authority for the object-bound mutation. Kind and target are not claimed to be
immutable because Windows exempts attribute-only access from share-mode enforcement. An
attribute-only reparse mutation of the same object does not transfer ownership or make handle
disposition traverse either target.

The removal handle MUST close before success is returned, and the backend MUST confirm that the
recorded identity is no longer visible at its old path. There is no fallback to basic disposition.

The supported Windows runtime baseline is Windows 10 version 1709 or later. An unavailable or
failed extended disposition is a cleanup error: the entry and its journal are retained rather than
being reported as removed.

## Alternatives

**Keep basic `FileDispositionInfo`.** Rejected because API success marks deletion pending rather
than proving that the visible mount name is gone. Terminalizing the journal at that point would
overstate cleanup.

**Close the basic-disposition handle and inspect only the old pathname.** Rejected because another
process can rename the verified object and replace its old name while a delete-sharing handle is
open. Inspecting that old name cannot prove that the moved object's pending disposition was not
cancelled elsewhere.

**Use basic disposition but deny all sharing.** Rejected because unrelated read handles could keep
the mount name visible indefinitely. POSIX semantics removes the namespace entry while those
handles continue to use the underlying object.

**Fall back to basic disposition on older Windows.** Rejected because one executable would then
have two cleanup guarantees, and the weaker branch could again remove its only durable recovery
record before namespace cleanup completed.

## Consequences

- Windows 10 versions before 1709 are no longer supported for mutating sessions. Read-only code has
  no separate compatibility promise because the shipped wrapper is supported as one product.
- Removal can fail more conservatively when another delete-capable handle already exists. The
  journal remains recovery evidence, and a later session can retry after contention ends.
- An ordinary write-capable handle is excluded during verified removal and rollback; unrelated
  read/inspect handles remain allowed and do not delay disappearance of the verified mount name.
- Attribute-only access is outside `CreateFileW` share-mode enforcement. Such a handle can mutate
  reparse data but cannot change the retained object identity, rename the entry, cancel disposition,
  or make removal traverse a target. This is a deliberate object-authority boundary, not a claim of
  metadata immutability.
- `windows-sys` already exposes the information class, structure, and flags under the existing
  feature set. No dependency, journal schema, CLI, privilege, or unsafe-module expansion follows.
- The architecture baseline and ownership-hardening change artifacts replace their earlier basic
  disposition decision with this one.

## Verification

- Native Windows tests hold an unrelated inspect handle open and require the verified mount name to
  be missing before that handle closes.
- Native tests attempt a pathname rename at the last pre-disposition boundary and require the
  removal handle to exclude that delete-capable mutation.
- Native tests require an ordinary no-follow writer to meet a sharing violation, then use the
  documented attribute-only exception to mutate a junction at the last boundary and prove that
  disposition still removes only the recorded object while both possible targets remain intact.
- A deterministic test seam forces disposition failure during both verified creation rollback and
  transaction cleanup; the original cause, retained entry, journal evidence, and intact source must
  remain observable.
- Windows x64 and x86 CI compile and run the backend suite. Warning-denied rustdoc and the scoped
  unsafe audit continue to cover the FFI wrapper.
