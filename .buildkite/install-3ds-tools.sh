#!/bin/bash
# Install the tools needed to compile on the 3DS
set -eu pipefail

# Set up DevKitPro pacman
mkdir -p /usr/share/keyring/
wget -U "dkp apt" -O /usr/share/keyring/devkitpro-pub.gpg https://apt.devkitpro.org/devkitpro-pub.gpg
echo "deb [signed-by=/usr/share/keyring/devkitpro-pub.gpg] https://apt.devkitpro.org stable main" \
  > /etc/apt/sources.list.d/devkitpro.list

apt-get update
apt-get install -y build-essential devkitpro-pacman

# Install DevKitPro 3DS tools
dkp-pacman -S --noconfirm 3ds-dev
export DEVKITPRO=/opt/devkitpro
export DEVKITARM=/opt/devkitpro/devkitARM

# Install cargo-3ds
cargo install --locked cargo-3ds

# Download Rust standard library sources (needed for build-std)
rustup component add rust-src
