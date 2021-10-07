# Changelog

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
