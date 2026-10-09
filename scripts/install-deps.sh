#!/usr/bin/env bash
# System libraries for building and running Seam on Ubuntu (CI machines).
set -euo pipefail
sudo apt-get update -qq
sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq \
  libwebkit2gtk-4.1-dev libayatana-appindicator3-dev librsvg2-dev libxdo-dev \
  libssl-dev build-essential file xvfb xauth >/dev/null
