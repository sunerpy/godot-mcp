#!/bin/sh
# Fake Godot probe fixture: godot-4.7.1-template. Not a real Godot build.
case "$1" in
--version)
  printf '%s\n' '4.7.1.stable.official.b09f793f5'
  exit 0
  ;;
--help)
  printf '%s\n' 'Godot Engine - https://godotengine.org'
  printf '%s\n' ''
  printf '%s\n' 'Usage: godot [options] [path to scene or '\''project.godot'\'' file]'
  printf '%s\n' ''
  printf '%s\n' 'General options:'
  printf '%s\n' '  -h, --help                       Display this help message.'
  printf '%s\n' '  --version                        Display the version string.'
  printf '%s\n' '  -v, --verbose                    Use verbose stdout mode.'
  printf '%s\n' '  --headless                       Enable headless mode (--display-driver headless --audio-driver Dummy).'
  printf '%s\n' '  --path <directory>               Path to a project (<directory> must contain a '\''project.godot'\'' file).'
  printf '%s\n' '  -q, --quit                       Quit after the first iteration.'
  printf '%s\n' '  --quit-after <int>               Quit after the given number of iterations. Set to 0 to disable.'
  printf '%s\n' '  -s, --script <script>            Run a script.'
  printf '%s\n' '  --check-only                     Only parse for errors and quit (use with --script).'
  printf '%s\n' '  --main-pack <file>               Path to a pack (.pck) file to load.'
  printf '%s\n' '  --remote-debug <uri>             Remote debug (<protocol>://<host/IP>[:<port>], e.g. tcp://127.0.0.1:6007).'
  exit 0
  ;;
*)
  # Every other flag is silently ignored with a success status.
  exit 0
  ;;
esac
