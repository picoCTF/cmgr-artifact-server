# cmgr Artifact Server

A simple proxy for serving [cmgr](https://github.com/ArmyCyberInstitute/cmgr) artifacts to users via `cmgrd`.

Forwards requests to `/builds/:build_id/:artifact` routes via `/artifacts/:build_id/:artifact`, while restricting access to all other administrative `cmgrd` routes.

Does not handle TLS termination - the expectation is that this would sit behind a load balancer or ingress in a production environment.

## Example

```shell
$ curl http://localhost:4200/builds/1  # 200 OK
$ curl http://localhost:4200/builds/1/file.c  # 200 OK

$ docker-compose up -d

$ curl http://localhost:4201/artifacts/1/file.c  # 200 OK
$ curl http://localhost:4201/artifacts/1  # 404 Not Found
$ curl http://localhost:4201/artifacts/1/artifacts.tar.gz  # 404 Not Found
```

## Configuration Options (environment variables)

| variable | default | description |
| --- | --- | --- |
| CMGRD_HOST | `host.docker.internal` | Hostname of the cmgrd server to reverse proxy. |
| CMGRD_PORT | `4200` | Port of the cmgrd server to reverse proxy. |
| NAMESERVER | host's `resolv.conf` value | Nameserver used to resolve `CMGRD_HOST`. |
