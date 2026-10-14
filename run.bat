@echo off
setlocal EnableExtensions

rem Build and run the NektoMe Rust client on Windows.
rem
rem Usage: run.bat [options]
rem   (no options)        build Debug with native audio, then run
rem   -n, --no-build      skip the build step, just run
rem   -r, --release       build and run in Release configuration
rem   -t, --test          run Rust unit tests before building
rem   --native-audio      enable the CPAL microphone/speaker backend
rem   --no-native-audio   disable native microphone/speaker support
rem   --signaling-only    build without WebRTC/Opus media features
rem   --cli               run the terminal diagnostic client

cd /d "%~dp0" || exit /b 1

set "SCRIPT_NAME=%~nx0"
set "CONFIG=Debug"
set "BUILD=1"
set "TEST=0"
set "CLI=0"
set "NATIVE_AUDIO=auto"
set "SIGNALING_ONLY=0"

:parse_args
if "%~1"=="" goto args_done
if /I "%~1"=="-h" goto help
if /I "%~1"=="--help" goto help
if /I "%~1"=="-n" goto option_no_build
if /I "%~1"=="--no-build" goto option_no_build
if /I "%~1"=="-r" goto option_release
if /I "%~1"=="--release" goto option_release
if /I "%~1"=="-t" goto option_test
if /I "%~1"=="--test" goto option_test
if /I "%~1"=="--native-audio" goto option_native_audio
if /I "%~1"=="--cpal-audio" goto option_native_audio
if /I "%~1"=="--no-native-audio" goto option_no_native_audio
if /I "%~1"=="--signaling-only" goto option_signaling_only
if /I "%~1"=="--no-default-features" goto option_signaling_only
if /I "%~1"=="--cli" goto option_cli

echo Unknown option: %~1 1>&2
call :usage 1>&2
exit /b 1

:option_no_build
set "BUILD=0"
shift
goto parse_args

:option_release
set "CONFIG=Release"
shift
goto parse_args

:option_test
set "TEST=1"
shift
goto parse_args

:option_native_audio
set "NATIVE_AUDIO=on"
shift
goto parse_args

:option_no_native_audio
set "NATIVE_AUDIO=off"
shift
goto parse_args

:option_signaling_only
set "SIGNALING_ONLY=1"
shift
goto parse_args

:option_cli
set "CLI=1"
shift
goto parse_args

:args_done
where cargo >nul 2>&1
if errorlevel 1 (
    echo Cargo was not found. Install Rust from https://rustup.rs/ and reopen this terminal. 1>&2
    exit /b 1
)

set "CARGO_MODE_ARGS="
if "%SIGNALING_ONLY%"=="1" (
    set "CARGO_MODE_ARGS=--no-default-features"
) else if /I not "%NATIVE_AUDIO%"=="off" (
    set "CARGO_MODE_ARGS=--features cpal-audio"
)

if "%TEST%"=="1" (
    echo ==^> Running Rust tests...
    cargo test %CARGO_MODE_ARGS%
    if errorlevel 1 goto cargo_failed
)

if "%BUILD%"=="1" (
    echo ==^> Building ^(%CONFIG%^)...
    if /I "%CONFIG%"=="Release" (
        cargo build --release %CARGO_MODE_ARGS%
    ) else (
        cargo build %CARGO_MODE_ARGS%
    )
    if errorlevel 1 goto cargo_failed
)

if /I "%CONFIG%"=="Release" (
    set "BINARY=target\release\nektome-client.exe"
) else (
    set "BINARY=target\debug\nektome-client.exe"
)

if not exist "%BINARY%" (
    echo Executable not found: %BINARY% ^(run without --no-build first^) 1>&2
    exit /b 1
)

echo ==^> Starting NektoMe Rust client ^(%CONFIG%^)...
if "%CLI%"=="1" (
    "%BINARY%" --cli
) else (
    "%BINARY%"
)
exit /b %ERRORLEVEL%

:cargo_failed
set "CARGO_EXIT=%ERRORLEVEL%"
echo Cargo command failed with exit code %CARGO_EXIT%. 1>&2
exit /b %CARGO_EXIT%

:help
call :usage
exit /b 0

:usage
echo Usage: %SCRIPT_NAME% [--no-build^|-n] [--release^|-r] [--test^|-t] [--native-audio^|--no-native-audio] [--signaling-only] [--cli]
exit /b 0
