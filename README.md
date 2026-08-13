# Lamella

![Status](https://img.shields.io/badge/status-in_development-orange)

A C# toolchain built from scratch in Rust: a compiler, an interpreted runtime (VES/CLR), an ahead-of-time backend (Cortex-M, RISC-V, WebAssembly), and a base class library. The language and runtime are implemented directly from their ECMA standards.

Lamella was born as a research project and for use in student theses. While its scope has outgrown those original purposes,
that academic discipline continues--with standards' clauses cited beside the code implementing them.

It is free and open source under the [licenses below](#license).

## Status

Lamella is in development and is **not yet ready for use**. Code is being reviewed and will be published incrementally. 

To be notified when releases are available, click **Watch** at the top of this repo, select **Custom**, and check **Releases**.

## Contributing

The Lamella Project encourages community collaboration.  Many Lamella repositories gladly accept pull requests under a contributor 
agreement (to ensure that all code remains free to use, dual-licensed as Apache-2.0 / MIT).

This specific core repository was developed clean-room: modifications to its code are made by responding to community members' 
requests.  If you find a bug, please feel welcome to [open an issue](https://github.com/lamellaproject/lamella/issues).

## How this repo is managed

Lamella is built by humans using traditional software engineering practices, in collaboration with frontier AI models.
@lamella-mel, AI co-author for this project, will often interact with community members to help support and accelerate 
their requests.

## About the name

A **lamella** is a thin layer of bone matrix--the composable building block that gives bone its extraordinary strength. The Lamella project brings the same approach to C#: decomposing the ECMA-335 Common Language Infrastructure into composable pieces that can be assembled to run C# programs on resource-constrained targets, either interpreted or native-compiled (AOT).

**LAMELLA** also works as a sufficiently nerdy acronym: Layered Architecture for Managed Embedded Low-Level Applications. Nobody should actually try to memorize that; just call it Lamella.

Fun fact: in Spanish, *la mella* means "the gap." Filling gaps is what Lamella is for: bringing C# to the places it couldn't run before, including the bare metal of sub-$1 microcontrollers. More softly, *hacer mella* means "to make an impact"--a goal for this project, in the classroom and on the workbench. **Lamella** gives students and adult hobbyists engineering-grade tools to explore electronics, with the power of C# behind them.

## License

Dual licensed under either of

- MIT license ([LICENSE-MIT](LICENSE-MIT))
- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))

at your option.
