# Lamella

![Status](https://img.shields.io/badge/status-in_development-orange)

A C# toolchain built from scratch in Rust: a compiler, an interpreted runtime (VES/CLR), an ahead-of-time backend (Cortex-M, RISC-V, WebAssembly), and a base class library. The language and runtime are implemented directly from their ECMA standards.

Lamella is a student project, built for learning and possible use in a thesis. It is free and open source under the [licenses below](#license).

Bug reports are welcome: please [open an issue](https://github.com/lamellaproject/lamella/issues) on GitHub. Pull requests are not being accepted at this time. Please also be patient with response times; this is maintained on a student schedule.

## Status

Lamella is in development and is **not yet ready for use**. Code is being reviewed and will be published incrementally. 

To be notified when releases are available, click **Watch** at the top of this repo, select **Custom**, and check **Releases**.

## About the name

A **lamella** is a thin layer of bone matrix--the composable building block that gives bone its extraordinary strength. The Lamella project brings the same approach to C#: decomposing the ECMA-335 Common Language Infrastructure into composable pieces that can be assembled to run C# programs on resource-constrained targets, either interpreted or native-compiled (AOT).

In Spanish, *la mella* means "the gap." Filling gaps is what Lamella is for: bringing C# to the places it couldn't run before, including the bare metal of sub-$1 microcontrollers. And *hacer mella* means "to make an impact," which is a goal for this project, in the classroom and on the workbench. Lamella gives students and adult hobbyists engineering-grade tools to explore electronics, with the power of C# behind them.

**LAMELLA** also works as a sufficiently nerdy acronym: Layered Architecture for Managed Embedded Low-Level Applications. Nobody should actually try to memorize that; just call it Lamella.

## License

Dual licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
