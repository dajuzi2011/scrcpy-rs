@echo off
rem convenience launcher for the release binary
"%~dp0target\release\scrcpy-rs.exe" %*
if errorlevel 1 pause
