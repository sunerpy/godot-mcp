#!/bin/sh
# Fake Godot probe fixture: godot-unknown-flag. Not a real Godot build.
case "$1" in
--version)
  printf '%s\n' '4.7.1.stable.official.b09f793f5'
  exit 0
  ;;
--help)
  printf '%s\n' 'Unknown command line option: --help'
  printf '%s\n' 'Use --help to list all options.'
  exit 1
  ;;
*)
  # Every other flag is silently ignored with a success status.
  exit 0
  ;;
esac
