@echo off
rem Fake Godot probe fixture: godot-4.5.1-editor. Not a real Godot build.
if "%~1"=="--version" goto version
if "%~1"=="--help" goto helpscreen
exit /b 0
:version
echo 4.5.1.stable.official.b09f793f5
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
echo   -e, --editor                     Start the editor instead of running the scene.
echo   --import                         Starts the editor, waits for any resources to be imported, and then quits.
echo   --export-release ^<preset^> ^<path^>  Export the project in release mode using the given preset and output path.
echo   --export-debug ^<preset^> ^<path^>   Export the project in debug mode using the given preset and output path.
echo   --export-pack ^<preset^> ^<path^>    Export the project data only using the given preset and output path.
echo   --doctool [^<path^>]               Dump the engine API reference to the given ^<path^> in XML format.
echo   --debug-server ^<uri^>             Start the editor debug server (^<protocol^>://^<host/IP^>[:^<port^>], e.g. tcp://127.0.0.1:6007).
exit /b 0
