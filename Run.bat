@echo off

echo Running first.bat...
call client-backend\include_ui.bat

echo Changing directory to the Rust project...
cd /d "%~dp0client-backend\"

echo Running cargo build...
cargo run

pause