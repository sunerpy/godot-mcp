@echo off
rem Fake Godot probe fixture: godot-4.7.1-template. Not a real Godot build.
if "%~1"=="--version" goto version
if "%~1"=="--help" goto helpscreen
exit /b 0
:version
echo 4.7.1.stable.official.b09f793f5
exit /b 0
:helpscreen
echo Godot Engine - https://godotengine.org
echo .
echo Usage: godot [options] [path to scene or 'project.godot' file]
echo .
echo General options:
echo   -h, --help                       Display this help message.
echo   --version                        Display the version string.
echo   -v, --verbose                    Use verbose stdout mode.
echo   --headless                       Enable headless mode (--display-driver headless --audio-driver Dummy).
echo   --path ^<directory^>               Path to a project (^<directory^> must contain a 'project.godot' file).
echo   -q, --quit                       Quit after the first iteration.
echo   --quit-after ^<int^>               Quit after the given number of iterations. Set to 0 to disable.
echo   -s, --script ^<script^>            Run a script.
echo   --check-only                     Only parse for errors and quit (use with --script).
echo   --main-pack ^<file^>               Path to a pack (.pck) file to load.
echo   --remote-debug ^<uri^>             Remote debug (^<protocol^>://^<host/IP^>[:^<port^>], e.g. tcp://127.0.0.1:6007).
exit /b 0
