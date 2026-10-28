# Lamella.Tools.$rid$

The Lamella command-line tool, `lamella`, built for $rid$. Lamella.Sdk restores this package and
runs the tool for `dotnet build`, `dotnet publish` and `dotnet run`, after checking it against the
SHA-256 the SDK carries for it. Do not reference this package directly: a project names Lamella.Sdk,
which restores the tools package for the machine the .NET SDK runs on, at its own version.

The package holds the tool under `tools/`, the notices of the third-party software it is built
with, and Lamella's license texts.

A tools package is published for each of these hosts: win-x64, win-arm64, linux-x64, linux-arm64
and osx-arm64. Intel macOS (osx-x64) is not a supported Lamella.Sdk build or publish host in this
release.
