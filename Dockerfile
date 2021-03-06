FROM nginx:1.21.1

ENV CMGRD_HOST host.docker.internal
ENV CMGRD_PORT 4200

COPY 11-use-resolv-conf.sh /docker-entrypoint.d/
COPY default.conf.template /etc/nginx/templates/default.conf.template
