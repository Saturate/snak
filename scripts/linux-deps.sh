#!/usr/bin/env bash
# Installs the system libraries GPUI and gpui-kit link against on Debian/Ubuntu
# (Wayland/X11, fontconfig, webkit, audio). Shared by CI and the release build so
# the two can't drift; mirrors gpui-kit's script/install-linux.sh upstream.
# Leaves out libvulkan1: it is only the runtime loader, which GPUI dlopens when
# the app starts, so nothing at build or link time needs it.
set -euo pipefail

sudo apt-get update
sudo apt-get install -y \
  gcc g++ clang \
  libfontconfig-dev libwayland-dev libxkbcommon-x11-dev libx11-xcb-dev \
  libwebkit2gtk-4.1-dev libssl-dev libzstd-dev libasound2-dev
