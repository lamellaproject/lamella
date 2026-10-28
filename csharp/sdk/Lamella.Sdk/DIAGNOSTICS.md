# Lamella.Sdk diagnostics

Each error and warning that `Lamella.Sdk` reports about a condition it finds itself carries one of these codes in
MSBuild's code field, as in `error LAMSDK0003: The lamella tool comes from the package ...`. A code names one condition
and what to do about it. Where a condition has more than one message, the messages differ in the names they fill in, and
the table says so.

An error or warning that `Lamella.Sdk` passes on from the C# compiler (`CS`, `LAM`) or from Lamella RHU's build targets
(`RHU`) keeps that code. The `lamella` command-line tool prints no codes.

| Code | Reported as | Condition | What to do |
|---|---|---|---|
| LAMSDK0001 | error; a warning when devices are listed | `LamellaToolPath` names no file. | Set `LamellaToolPath` to the full path of a `lamella` executable, or remove it to use the tool restored with `Lamella.Sdk`. |
| LAMSDK0002 | error; a warning when devices are listed | This `Lamella.Sdk` has no `lamella` tool for the machine the .NET SDK runs on. The message names the machines it has one for, and that Intel macOS (osx-x64) is not a supported host in this release, or says it has none. | Set `LamellaToolPath` to the full path of a `lamella` executable built for this machine. |
| LAMSDK0003 | error; a warning when devices are listed | The `lamella` tool has not been restored: the project has not been restored, or the tool is not where restore puts it. | Restore the project (`dotnet restore`), or set `LamellaToolPath`. |
| LAMSDK0004 | error; a warning when devices are listed | The restored `lamella` tool could not be read to check its SHA-256. | Restore the project again (`dotnet restore`), or set `LamellaToolPath`. |
| LAMSDK0005 | error; a warning when devices are listed | The restored `lamella` tool is not the one this `Lamella.Sdk` was packed with. | Delete the folder the message names and restore the project again, or set `LamellaToolPath`. |
| LAMSDK0006 | error | `LamellaTargetBoard` names no board this SDK knows. The message names the nearest boards. | Name a board that `lamella boards` lists. |
| LAMSDK0007 | error | The board `LamellaTargetBoard` names is not supported by the project's target framework. The message names the framework that supports it. | Target that framework, or name a board this one supports. |
| LAMSDK0008 | error | A program is published for a board, and the project names none. | Name the board as `LamellaTargetBoard`: in the project file, in a publish profile, or with `-p:LamellaTargetBoard=<board>`. |
| LAMSDK0009 | error | The `lamella` tool wrote no references for the build. | Build again; the tool's own message, above this one, says why. |
| LAMSDK0010 | error | A library package carries builds for a later Lamella than the project targets. | Target the Lamella version the message names, or reference an earlier version of the package. |
| LAMSDK0011 | error | `dotnet run` was given a device for a board other than the one the project names. | Choose a device for the project's board: `dotnet run --list-devices` lists them. |
| LAMSDK0012 | error; a warning when devices are listed | One of Lamella RHU's packages has not been restored: its build package, or a board, runtime or tools package. The message names it. | Restore the project (`dotnet restore`). |
| LAMSDK0013 | error; a warning when devices are listed | A restored Lamella RHU package is not the one this `Lamella.Sdk` pins. The message names the package and both hashes. | Delete the folder the message names and restore the project again. |
| LAMSDK0014 | error; a warning when devices are listed | The restored `Lamella.Rhu.Build` and this `Lamella.Sdk` implement different contracts. | Use one `Lamella.Sdk` release, which restores the `Lamella.Rhu.Build` it was built with. |
| LAMSDK0015 | error; a warning when devices are listed | The restored `Lamella.Rhu.Build` names no props and targets that exist. | Delete its folder and restore the project again. |
| LAMSDK0016 | warning | A project that names a board found only debug probes' UART bridges, which are not listed for it. | Run with `dotnet run --device <device>`, naming the bridge; `lamella devices` lists them. |
| LAMSDK0017 | warning; an error when that board's probe is named | The board the project names has no probe route in this SDK yet. | Run the program another way the board supports. |
| LAMSDK0018 | warning | A `net11.0` project names no board, and no debug probe or emulator that reaches one of Lamella RHU's boards was found. | Attach a debug probe wired to the board, or name the board as `LamellaTargetBoard`. |
| LAMSDK0019 | error | A file-based program has no device to run on. | Name its device with `-p:Device=<device>`. |
| LAMSDK0020 | error | More than one device is available, and none was chosen. | Name one with `--device`, or with `-p:Device=<device>` for a file-based program. The message lists them. |
| LAMSDK0021 | error | The device given is not one of the project's devices. | Name one the message lists, or that `dotnet run --list-devices` lists. |
| LAMSDK0022 | error | A run on a debug probe has no image to run: no manifest was published. | Run without `--no-build`, so that the run publishes the image. |
| LAMSDK0023 | error | A `net11.0` program was run with no device. | Choose one from `dotnet run --list-devices`. |
| LAMSDK0024 | error | A file-based `net11.0` program names no board, and the device chosen for it is for a board. | Name the device on the command line with `-p:Device=<device>`, as the message shows. |
