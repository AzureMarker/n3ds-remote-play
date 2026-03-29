#!/bin/bash
# Install dependencies needed to build the server (xcap and ffmpeg-next crate requirements)
set -eu pipefail

apt-get update
apt-get install -y pkg-config clang libclang-dev libxcb1-dev libxrandr-dev \
  libdbus-1-dev libpipewire-0.3-dev libwayland-dev libegl-dev libgbm-dev \
  libavcodec-dev libavformat-dev libavutil-dev libavfilter-dev libavdevice-dev
