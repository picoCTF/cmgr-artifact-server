# cmgr Artifact Server

A server for automatically handling the distribution of
[cmgr](https://github.com/ArmyCyberInstitute/cmgr) build artifacts.

`cmgr-artifact-server` is intended to run alongside `cmgrd` and supports multiple file hosting
backends. Similarly to `cmgrd`, it is a single binary and requires minimal configuration.

The `CMGR_ARTIFACT_DIR` environment variable (also used by `cmgrd`) determines which artifacts to
serve, while the backend and other options are specified via flags.

## `selfhosted` backend

The `selfhosted` backend provides a simple way to serve build artifacts over HTTP without exposing
the full `cmgrd` API to end users. `cmgr-artifact-server` itself will run a web server for generated
artifact files.

```bash
# Artifact files can be served by the cmgrd API, but this also exposes other endpoints:
$ cmgrd &
$ curl http://localhost:4200/builds/1/file.c            # 200 OK
$ curl http://localhost:4200/builds/1                   # 200 OK
$ curl http://localhost:4200/builds/1/artifacts.tar.gz  # 200 OK
$ curl http://localhost:4201/challenges                 # 200 OK

# With the selfhosted backend, cmgr-artifact-server serves individual artifact files only:
$ cmgr-artifact-server -b selfhosted &
$ curl http://localhost:4201/1/file.c                   # 200 OK
$ curl http://localhost:4201/1                          # 404 Not Found
$ curl http://localhost:4201/1/artifacts.tar.gz         # 404 Not Found
```

When using the `selfhosted` backend with the [picoCTF
platform](https://github.com/picoCTF/platform), specify `http://hostname:4201` as the challenge
server's **artifact base URL**.

## `s3` backend

This backend watches `CMGR_ARTIFACT_DIR` for changes and syncs any updated files to the configured
S3 bucket. It can also automatically generate invalidations for an associated CloudFront
distribution.

```bash
$ cmgr-artifact-server -b s3 \
> --backend-option bucket=sample-bucket-name \
> --backend-option path-prefix=ctf-artifacts \
> --backend-option cloudfront-distribution=EDFDVBD6EXAMPLE &

# Creates a new build:
$ cmgr build cmgr/examples/custom-socat 1
  Build IDs:
    4

# Potentially updates existing builds:
$ cmgr update

# In either case, any modified artifact files are synced to the configured cloud storage provider:
$ curl https://your-cloudfront-distribution.com/ctf-artifacts/4/file.c  # 200 OK
```

Note that there will necessarily be some delay between `cmgr(d)` reporting a build as successful and
the completed upload of its associated artifacts.

When the `s3` backend with the [picoCTF platform](https://github.com/picoCTF/platform), specify your
bucket or CloudFront distribution URL (including path prefix, if applicable) as the challenge
server's **artifact base URL**.

## Flags

| short | long | description |
| --- | --- | --- |
| `-b` | `--backend` | File hosting backend. Options: `selfhosted`, `s3`. |
| `-h` | `--help` | Prints help information. |
| `-l` | `--log-level` | Specify log level from the usual options. Defaults to `info`. |
| `-o` | `--backend-option` | Backend-specific option in `key=value` format. May be specified multiple times. Some options may be required - see backend-specific documentation. |
| `-V` | `--version` | Prints version information. |

### `selfhosted` backend options

| key | required? | description |
| --- | --- | --- |
| address | no | Socket address to bind to. Defaults to `0.0.0.0:4201`. |

### `s3` backend options

Note: IAM user credentials are loaded from the usual sources
(`AWS_ACCESS_KEY_ID`/`AWS_SECRET_ACCESS_KEY`, `~/.aws/config`, etc.) The provided IAM user requires
the following permissions for the associated resources:

- `s3:ListBucket`
- `s3:ListObjectsV2`
- `s3:HeadObject`
- `s3:PutObject`
- `s3:DeleteObject`
- `cloudfront:CreateInvalidation` (if a CloudFront distribution is specified)

| key | required? | description |
| --- | --- | --- |
| bucket | yes | S3 bucket name |
| path-prefix | no | Slash-delimited path prefix to use when uploading artifacts. Defaults to `/`. |
| cloudfront-distribution | no | If specified, invalidations will automatically be created as needed for this distribution. Uses `path-prefix` if set. |
