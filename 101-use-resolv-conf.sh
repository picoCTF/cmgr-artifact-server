#!/bin/bash

# Workaround to use the nameservers defined in resolv.conf for the nginx `resolver`
# directive. Since we may want to resolve the cmgrd hostname dynamically, we need to
# specify a `resolver`. Usually, this only accepts a hardcoded nameserver. However,
# since this image may run in several environments (local Docker network, ECS, etc.),
# we instead substitute whatever is defined in /etc/resolv.conf.
#
# This script is intended to run prior to the `20-envsubst-on-templates.sh` script
# provided by the base image in `/docker-entrypoint.d`.
#
# See also:
# https://trac.nginx.org/nginx/ticket/658

set -e

if [ "$NAMESERVER" == "" ]; then
	export NAMESERVER=`cat /etc/resolv.conf | grep "nameserver" | awk '{print $2}' | tr '\n' ' '`
fi

echo "Using nameserver: $NAMESERVER"
envsubst '$NAMESERVER' < /etc/nginx/templates/default.conf.template > /etc/nginx/templates/default.conf.template.tmp
mv /etc/nginx/templates/default.conf.template.tmp /etc/nginx/templates/default.conf.template
