# cmgr Artifact Server

A simple daemon to automatically handle the distribution of
[cmgr](https://github.com/ArmyCyberInstitute/cmgr) build artifacts.

`cmgr-artifact-server` is intended to run alongside `cmgrd` and supports multiple file hosting
backends. Like `cmgrd`, it is a single binary and requires minimal configuration.

The `CMGR_ARTIFACT_DIR` environment variable (also used by `cmgrd`) determines which artifacts to
distribute, while the backend and any additional settings are specified via command-line options.

Behind the scenes, `cmgr-artifact-server` maintains a cache of extracted artifact tarballs
(`.artifact_server_cache`) within the specified `CMGR_ARTIFACT_DIR`. A full synchronization of all
existing local artifacts to the backend is performed upon startup. Any further changes to local
artifacts (due to build creation, updates, or deletion) are automatically handled as they occur.

## Installation

Download the latest [release](https://github.com/picoCTF/cmgr-artifact-server/releases) for your
platform, extract the tarball, and copy the binary to an appropriate location:

```bash
$ tar xzf cmgr-artifact-server_linux_amd64.tar.gz
$ cp cmgr-artifact-server /usr/local/bin
```

Alternatively, build and install from source:

```bash
$ cargo install --locked --path .
```

## Backends

### `selfhosted` backend

The `selfhosted` backend provides a simple way to serve build artifacts over HTTP without exposing
the full `cmgrd` API to end users. `cmgr-artifact-server` itself will act as a web server for
generated artifact files.

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

When using the this backend with the [picoCTF platform](https://github.com/picoCTF/platform) (note:
not yet publicly available), specify `http://hostname:4201` as the challenge server's **artifact
base URL**.

### `S3` backend

This backend syncs artifact files to a specified S3 bucket. It can also automatically generate
invalidations for an associated CloudFront distribution.

```bash
$ cmgr-artifact-server -b S3 \
> --backend-option bucket=sample-bucket-name \
> --backend-option path-prefix=ctf-artifacts \
> --backend-option cloudfront-distribution=EDFDVBD6EXAMPLE &

# Creates a new build:
$ cmgr build cmgr/examples/custom-socat 1
  Build IDs:
    4

# Potentially updates existing builds:
$ cmgr update

# In either case, any modified artifact files are synced to S3:
$ curl https://your-cloudfront-distribution.com/ctf-artifacts/4/file.c  # 200 OK
```

Note that there will necessarily be some delay between `cmgr(d)` reporting a build as successful and
the completed upload of its associated artifacts.

IAM user credentials are read from the same sources used by the AWS CLI, e.g. the
`AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` environment variables, the  `~/.aws/config` and
`~/.aws/credentials` files, etc. The provided IAM user requires the following permissions for the
associated resources:

- `s3:ListBucket`
- `s3:GetObject`
- `s3:PutObject`
- `s3:DeleteObject`
- `cloudfront:CreateInvalidation` (if a CloudFront distribution is specified)

The backend will check that all necessary IAM actions can be performed before starting.

When using this backend with the [picoCTF platform](https://github.com/picoCTF/platform) (note: not
yet publicly available), specify your bucket or CloudFront distribution URL (including path prefix,
if applicable) as the challenge server's **artifact base URL**.

## Options

| short | long | description |
| --- | --- | --- |
| `-b` | `--backend` | File hosting backend. Options: `selfhosted`, `S3`. |
| `-h` | `--help` | Prints help information. |
| `-l` | `--log-level` | Specify log level. Options: `error`, `warn`, `info`, `debug`, `trace`. Defaults to `info`. |
| `-o` | `--backend-option` | Backend-specific option in `key=value` format. May be specified multiple times. Some options may be required - see backend-specific documentation. |
| `-V` | `--version` | Prints version information. |

### `selfhosted` backend options

| key | required? | description |
| --- | --- | --- |
| address | no | Socket address to bind to. Defaults to `0.0.0.0:4201`. |

### `S3` backend options

| key | required? | description |
| --- | --- | --- |
| bucket | yes | S3 bucket name |
| path-prefix | no | Slash-delimited path prefix to use when uploading artifacts. |
| cloudfront-distribution | no | CloudFront distribution ID. If specified, will automatically create invalidations when artifacts are updated. Uses `path-prefix` if set (assumes distribution's origin path is the bucket root). |
