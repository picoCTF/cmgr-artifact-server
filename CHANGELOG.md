# Changelog

## Unreleased

- Artifact tarballs in a subdirectory of `CMGR_ARTIFACT_DIR` are now published under a matching path prefix, so that one server can publish the artifacts of the several orchestrators a [cork](https://github.com/CyLabAcademy/challenge-orchestrator) build plane builds for. Only subdirectories carrying a `.cork-artifact-namespace` marker are treated this way; every other subdirectory is ignored, as before. A tarball in `CMGR_ARTIFACT_DIR` itself is unaffected.
- The `S3` backend's startup removal of orphaned bucket directories is now skipped when the local artifact directory holds no builds at all, and can be disabled with `-o prune-orphans=false`. An empty artifact directory means the host has not built yet rather than that every build was deleted, and sweeping there emptied the bucket of a running event. Deletions seen while the server is running are propagated as before and do not depend on that pass.

## v2.3.0

- The `linux_arm64` release binary is now built natively on an arm64 runner. Earlier releases mislabeled this tarball: it actually contained a `linux_amd64` binary.
- The `darwin_amd64` tarball is no longer published. Earlier releases' `darwin_amd64` tarballs actually contained `darwin_arm64` binaries.
- Linux binaries are now built on Ubuntu 24.04 runners (rather than 22.04). They require glibc 2.39 or newer on the host (e.g. Ubuntu 24.04, Debian 13, RHEL 10). Previous releases required glibc 2.34 and also ran on Ubuntu 22.04, Debian 12, RHEL 9, and Amazon Linux 2023.
- Bump deps

## v2.2.3

- Add retry/backoff for S3 requests, batched CloudFront invalidations https://github.com/picoCTF/cmgr-artifact-server/pull/343
- Bump deps

## v2.2.0

- Linux binaries are now built on Ubuntu 22.04 runners (rather than 24.04) for compability with a wider range of glibc versions.

## v2.1.0

- Added the ability to replace build IDs in artifact download URLs with a salted SHA-256 digest.
- Code cleanup and dependency updates.

## v2.0.6

Dependency updates.

## v2.0.5

Fixed CI issue preventing the creation of release tarballs.

## v2.0.4

Dependency updates.
Relicensed to MIT OR Apache-2.0.

## v2.0.3

Fixed panic when called with one or more `--backend-option` values.

## v2.0.2

Dependency updates, including a [fix](https://github.com/stephank/hyper-staticfile/releases/tag/v0.9.2) for a malicious path traversal vulnerability on Windows hosts if using the `selfhosted` backend (RUSTSEC-2022-0069).

## v2.0.1

Dependency updates.

## v2.0.0

`cmgr-artifact-server` is now a standalone binary supporting the same platforms as
[`cmgr`](https://github.com/ArmyCyberInstitute/cmgr). Artifact requests are no longer
reverse-proxied through `cmgrd`, allowing usage with `cmgr` only.

Two file hosting backends are now supported:

- `selfhosted`, which runs its own web server to serve artifact files.

- `S3`, which syncs artifacts to an [S3 bucket](https://aws.amazon.com/s3/) and can also generate
  [invalidations](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/Invalidation.html)
  for an associated CloudFront distribution when artifacts are updated or deleted.

See the [README](README.md) for details, including a full option listing and usage examples.

## v1.0.0

The first version of `cmgr-artifact-server` was a customized nginx Docker container that
reverse-proxied requests for artifact files to a
[`cmgrd`](https://github.com/ArmyCyberInstitute/cmgr) instance's `/builds/` endpoint.
