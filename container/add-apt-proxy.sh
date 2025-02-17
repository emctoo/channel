#!/usr/bin/env bash
set -x -eo pipefail

# Get HTTP proxy, fallback to HTTPS proxy if not set
http_proxy_value=${http_proxy:-$https_proxy}

# Get HTTPS proxy, fallback to HTTP proxy if not set
https_proxy_value=${https_proxy:-$http_proxy}

# If either proxy is set, add to apt.conf
if [ -n "$http_proxy_value" ] || [ -n "$https_proxy_value" ]; then
    echo "Configuring proxy settings..."
    mkdir -p /etc/apt/apt.conf.d
    [ -n "$http_proxy_value" ] && echo "Acquire::http::Proxy \"$http_proxy_value\";" >>/etc/apt/apt.conf.d/proxy.conf
    [ -n "$https_proxy_value" ] && echo "Acquire::https::Proxy \"$https_proxy_value\";" >>/etc/apt/apt.conf.d/proxy.conf
    echo "Proxy settings added to /etc/apt/apt.conf.d/proxy.conf"
else
    echo "No proxy environment variables found. Skipping proxy configuration."
fi
