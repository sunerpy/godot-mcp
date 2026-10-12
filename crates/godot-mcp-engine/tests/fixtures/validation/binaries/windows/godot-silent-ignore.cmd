@echo off
rem A fake Godot build that SILENTLY IGNORES the flags it was handed while
rem printing plausible-looking progress output.
rem
rem Behavioural twin of the sibling `unix/godot-silent-ignore.sh`; see that file
rem for why this failure mode is real rather than hypothetical.
rem
rem NOT VERIFIED ON WINDOWS: only the `.sh` variant has actually been executed on
rem this repository's Linux host.
echo Godot Engine v4.7.1.stable.official - https://godotengine.org
echo godot-mcp-fake: started
echo godot-mcp-fake: running

:parse
if "%~1"=="" goto done
if /I "%~1"=="--version" (
  echo 4.7.1.stable.official.b09f793f5
  exit /b 0
)
shift
goto parse

:done
echo godot-mcp-fake: done
exit /b 0
