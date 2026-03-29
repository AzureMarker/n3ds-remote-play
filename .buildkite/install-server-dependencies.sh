#!/bin/bash
# Install dependencies needed to build the server

apt-get update
apt-get install -y pkg-config libclang-dev libxcb1-dev libxrandr-dev \
  libdbus-1-dev libpipewire-0.3-dev libwayland-dev libegl-dev ffmpeg
