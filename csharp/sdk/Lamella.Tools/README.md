# Lamella.Tools.$rid$

The Lamella command-line tool, `lamella`, and its debugger, `lamella-debug`, built for $rid$.
Lamella.Sdk restores this package and runs the tool for `dotnet build`, `dotnet publish` and
`dotnet run`, after checking it against the SHA-256 the SDK carries for it. An editor's debug
session starts the debugger, which the SDK names to the editor after checking it the same way. Do
not reference this package directly: a project names Lamella.Sdk, which restores the tools package
for the machine the .NET SDK runs on, at its own version.

The package holds the tool and the debugger under `tools/`, the notices of the third-party software
they are built with, and Lamella's license texts.

A tools package is published for each of these hosts: win-x64, win-arm64, linux-x64, linux-arm64
and osx-arm64.
