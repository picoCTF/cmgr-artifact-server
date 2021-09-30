# cmgr Artifact Server

A server for automatically handling the distribution of
[cmgr](https://github.com/ArmyCyberInstitute/cmgr) build artifacts.

`cmgr-artifact-server` is intended to run alongside `cmgrd` and can operate in one of two modes.
Similarly to `cmgrd`, it is a single binary requiring minimal configuration.

## Testing mode (file server)

Testing mode provides a simple way to serve build artifacts over HTTP without exposing the full
`cmgrd` API to end users. In this mode, `cmgr-artifact-server` functions as a simple web server.

```bash
# Artifact files can be served by the cmgrd API, but this also exposes other endpoints:
$ cmgrd &
$ curl http://localhost:4200/builds/1/file.c  # 200 OK
$ curl http://localhost:4200/builds/1  # 200 OK
$ curl http://localhost:4201/artifacts/1/artifacts.tar.gz  # 200 OK

# In testing mode, cmgr-artifact-server serves individual artifact files only:
$ cmgr-artifact-server -m testing -d
$ curl http://localhost:4201/artifacts/1/file.c  # 200 OK
$ curl http://localhost:4201/artifacts/1  # 404 Not Found
$ curl http://localhost:4201/artifacts/1/artifacts.tar.gz  # 404 Not Found
```

This mode is suitable only for local development or testing environments. TLS termination is not
supported.

When using `cmgr-artifact-server` in testing mode with the [picoCTF
platform](https://github.com/picoCTF/platform), specify `http://hostname:4201/artifacts` as the
challenge server's **artifact base URL**.

## Production mode (cloud upload)

In production mode, `cmgr-artifact-server` does not handle requests itself, but instead watches
`CMGR_ARTIFACT_DIR` for changes and syncs any updated files to a cloud storage provider. Currently,
only Amazon S3 is supported.

```bash
$ cmgr-artifact-server -m production -d -b s3 \
> --backend-opt bucket=sample-bucket-name \
> --backend-opt path-prefix=ctf-artifacts \
> --backend-opt cloudfront-distribution=EDFDVBD6EXAMPLE

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

When using `cmgr-artifact-server` in production mode with the [picoCTF
platform](https://github.com/picoCTF/platform), specify your bucket or CloudFront distribution URL
(including any configured path prefix) as the challenge server's **artifact base URL**.

## Flags

| short | long | description |
| --- | --- | --- |
| `-b` | `--backend` | `production` mode backend. Currently only accepts `s3`. Ignored in `testing` mode. |
| `-d` | `--daemonize` | Run `cmgr-artifact-server` in the background and do not log to stdout. |
| `-i` | `--interface` | Interface to bind to in `testing` mode. Defaults to `0.0.0.0`. |
| `-l` | `--log-level` | Specify log level from the usual options. Defaults to `INFO`. |
| `-m` | `--mode` | One of `testing` or `production`. If `production`, `-b` must be specified as well. |
| `-o` | `--backend-opt` | Backend-specific options in `key=value` format. Some may be required, see backend-specific documentation. |
| `-p` | `--port` | Port to bind to in `testing` mode. Defaults to `4201`. |

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
