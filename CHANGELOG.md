# Changelog

## Unreleased

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
