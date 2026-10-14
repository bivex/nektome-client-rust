#!/usr/bin/env bash
# Build and run the NektoMe Rust client.
#
# Usage: ./run.sh [options]
#   (no options)   build Debug with native audio when supported, then run
#   -n, --no-build skip the build step, just run
#   -r, --release  build and run in Release configuration
#   -t, --test     run Rust unit tests before building
#   --native-audio force the CPAL native microphone/speaker backend
#   --no-native-audio disable native microphone/speaker support
#   --signaling-only build without the default WebRTC/Opus media features
#   --cli          run the terminal diagnostic client
set -euo pipefail

cd "$(dirname "$0")"

CONFIG="Debug"
BUILD=1
TEST=0
CLI=0
NATIVE_AUDIO=auto
NATIVE_AUDIO_ENABLED=0
SIGNALING_ONLY=0

for arg in "$@"; do
  case "$arg" in
    -h|--help)
      echo "Usage: $0 [--no-build|-n] [--release|-r] [--test|-t] [--native-audio|--no-native-audio] [--signaling-only] [--cli]"
      exit 0
      ;;
    -n|--no-build) BUILD=0 ;;
    -r|--release)  CONFIG="Release" ;;
    -t|--test)     TEST=1 ;;
    --native-audio|--cpal-audio) NATIVE_AUDIO=on ;;
    --no-native-audio) NATIVE_AUDIO=off ;;
    --signaling-only|--no-default-features)
      SIGNALING_ONLY=1
      ;;
    --cli)         CLI=1 ;;
    *) echo "Unknown option: $arg" >&2
       echo "Usage: $0 [--no-build|-n] [--release|-r] [--test|-t] [--native-audio|--no-native-audio] [--signaling-only] [--cli]" >&2
       exit 1 ;;
  esac
done

if [ "$SIGNALING_ONLY" -eq 0 ] && [ "$NATIVE_AUDIO" != "off" ]; then
  PLATFORM="$(uname -s)"
  if [ "$NATIVE_AUDIO" = "on" ] || [[ "$PLATFORM" =~ ^(Darwin|MINGW|MSYS|CYGWIN) ]]; then
    NATIVE_AUDIO_ENABLED=1
  elif command -v pkg-config >/dev/null 2>&1 && pkg-config --exists alsa; then
    NATIVE_AUDIO_ENABLED=1
  else
    echo "==> Native audio disabled: ALSA development files were not found (use --native-audio to force it)."
  fi
fi

# Keep the Cargo argument array non-empty. Bash 3.2, which ships with macOS,
# treats expansion of an empty array as an unbound variable under `set -u`.
run_cargo() {
  local subcommand="$1"
  shift
  local args=("$subcommand" "$@")
  if [ "$SIGNALING_ONLY" -eq 1 ]; then
    args+=(--no-default-features)
  fi
  if [ "$NATIVE_AUDIO_ENABLED" -eq 1 ]; then
    args+=(--features cpal-audio)
  fi
  cargo "${args[@]}"
}

if [ "$TEST" -eq 1 ]; then
  echo "==> Running Rust tests..."
  run_cargo test
fi

if [ "$BUILD" -eq 1 ]; then
  echo "==> Building ($CONFIG)..."
  if [ "$CONFIG" = "Release" ]; then
    run_cargo build --release
  else
    run_cargo build
  fi
fi

echo "==> Starting NektoMe Rust client ($CONFIG)..."
if [ "$CONFIG" = "Release" ]; then
  BINARY="target/release/nektome-client"
else
  BINARY="target/debug/nektome-client"
fi
if [ ! -x "$BINARY" ]; then
  echo "Executable not found: $BINARY (run without --no-build first)" >&2
  exit 1
fi
if [ "$CLI" -eq 1 ]; then
  exec "$BINARY" --cli
else
  exec "$BINARY"
fi
