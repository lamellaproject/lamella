# C# language support

This page lists every named feature of the C# language, by the version of C# that introduced it, and how
much of each one Lamella's C# compiler supports. It is regenerated from the compiler's measurements
for every release, so a feature moves from "not yet" to "in progress" to "complete" here as it lands.

Lamella's C# compiler, `lcsc`, against every named feature of C# 1.0 through C# 15.0:
439 features, of which 67 are complete, 2 require more tests, 205 are in progress, 18 are not yet supported and 147 have no tests yet.

**No C# version is complete yet.** C# 1.0 has 53 of its 169 features complete. Each of
the others is listed below with the share of its conformance tests that pass.

## How each feature is measured

A feature's status comes from the conformance suite:

- **Complete:** every suite test that belongs to the feature passes, and those tests show the whole feature.
- **More tests required:** every current suite test that belongs to the feature passes, but the suite needs
  more tests to show the whole feature.
- **In progress:** some of them pass.
- **Not yet:** none of them passes.
- **No tests yet:** no suite test checks the feature at its language version yet.

The suite tests passed column gives the count and the share: for example, "19 of 20, 95%", or "20 of 20,
100%" for a complete feature. It is empty for a feature with no tests yet.

The conformance suite is [ecma-334-conformance-tests](https://github.com/lamellaproject/ecma-334-conformance-tests).
It holds small programs that each check one rule of the C# language:

- Most compile and run, and must print the right result.
- The rest must be rejected by the compiler.

A test belongs to a feature when the rule it checks is in the feature's clause of the ECMA-334 standard,
7th edition, or, for C# 8 and later, in the feature's language proposal.

Each feature is scored at its own language version, counting only the tests that version can express. For
example, a C# 2.0 feature is scored at `/langversion:2`.

The results on this page:

- **Suite:** `ecma-334-conformance-tests` commit `e7fe658131d1`.
- **Compiler:** `lcsc` at public commit `595b6d7843`.
- **Runs:** 2026-10-11, at C# 1, C# 2, C# 3, C# 4, C# 5, C# 6, C# 7, C# 7.1, C# 7.2, C# 7.3 and C# 11.
- **Platform:** the suite's programs run on .NET (Windows x64, .NET 11.0.0-rc.1.26425.128).

Running on a microcontroller, through Lamella's interpreter or its ahead-of-time compiler, is measured
separately.

The suite has no run at C# 8.0, C# 9.0 and C# 10.0. Features of those versions are
scored at the next version it was run at, C# 11.

The suite was not run at C# 12.0 or later, so every feature of those versions has no tests yet.

## Summary

| C# version | features | complete | in progress | not yet | no tests yet | suite tests passed at this version |
|---|---:|---:|---:|---:|---:|---|
| [C# 1.0](#c-10) | 169 | 53 | 109 | 2 | 5 | 4,622 of 5,847, 79% |
| [C# 2.0](#c-20) | 38 | 2 | 23 | 4 | 9 | 6,775 of 9,806, 69% |
| [C# 3.0](#c-30) | 11 | 1 | 7 | 3 | 0 | 7,429 of 11,075, 67% |
| [C# 4.0](#c-40) | 4 | 0 | 4 | 0 | 0 | 7,499 of 11,310, 66% |
| [C# 5.0](#c-50) | 2 | 0 | 1 | 1 | 0 | 7,543 of 11,402, 66% |
| [C# 6.0](#c-60) | 18 | 0 | 15 | 0 | 3 | 7,684 of 11,592, 66% |
| [C# 7.0](#c-70) | 19 | 2 | 15 | 2 | 0 | 7,874 of 11,899, 66% |
| [C# 7.1](#c-71) | 4 | 0 | 4 | 0 | 0 | 7,878 of 11,919, 66% |
| [C# 7.2](#c-72) | 11 | 1 | 9 | 1 | 0 | 7,952 of 12,083, 65% |
| [C# 7.3](#c-73) | 12 | 0 | 12 | 0 | 0 | 7,984 of 12,162, 65% |
| [C# 8.0](#c-80) | 30 | 5 | 0 | 0 | 25 | 8,240 of 13,639, 60% (at C# 11) |
| [C# 9.0](#c-90) | 25 | 2 | 2 | 1 | 20 | 8,240 of 13,639, 60% (at C# 11) |
| [C# 10.0](#c-100) | 21 | 1 | 2 | 1 | 17 | 8,240 of 13,639, 60% (at C# 11) |
| [C# 11.0](#c-110) | 22 | 0 | 4 | 3 | 15 | 8,240 of 13,639, 60% |
| [C# 12.0](#c-120) | 12 | 0 | 0 | 0 | 12 |  |
| [C# 13.0](#c-130) | 12 | 0 | 0 | 0 | 12 |  |
| [C# 14.0](#c-140) | 17 | 0 | 0 | 0 | 17 |  |
| [C# 15.0](#c-150) | 12 | 0 | 0 | 0 | 12 |  |

The last column counts every suite test that runs at that version, whichever feature it belongs to.

## C# 1.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Abstract classes | In progress | 19 of 20, 95% | Declare classes that cannot be instantiated directly. | ECMA-334 7th edition, clause 15.2.2 |
| Abstract methods | In progress | 6 of 10, 60% | Require derived classes to provide method implementations. | ECMA-334 7th edition, clause 15.6.7 |
| Access modifiers | In progress | 34 of 35, 97% | Control declared accessibility with public, protected, internal, private, and protected internal. | ECMA-334 7th edition, clause 7.5.2 |
| Address-of operator | In progress | 4 of 7, 57% | Obtain a variable's address using & in unsafe code. | ECMA-334 7th edition, clause 23.6.5 |
| Application entry point | In progress | 17 of 19, 89% | Start an executable with a permitted static Main signature. | ECMA-334 7th edition, clause 7.1 |
| Arithmetic operators | In progress | 89 of 321, 27% | Add, subtract, multiply, divide, or calculate numeric remainders. | ECMA-334 7th edition, clause 12.10 |
| Array covariance | In progress | 3 of 4, 75% | Convert reference-element arrays to compatible base-element array types. | ECMA-334 7th edition, clause 17.6 |
| Array initializers | In progress | 10 of 13, 76% | Initialize array elements with a brace-delimited list. | ECMA-334 7th edition, clause 17.7 |
| As operator | In progress | 2 of 4, 50% | Attempt a reference conversion, returning null when unsuccessful. | ECMA-334 7th edition, clause 12.12.13 |
| Attribute targets | In progress | 16 of 31, 51% | Select the declaration or generated entity receiving an attribute. | ECMA-334 7th edition, clause 22.3 |
| Attributes | In progress | 33 of 59, 55% | Attach metadata using positional and named attribute arguments. | ECMA-334 7th edition, clauses 22.2, 22.3 |
| AttributeUsage attribute | Not yet | 0 of 1, 0% | Control permitted attribute targets, inheritance, and repeated application. | ECMA-334 7th edition, clause 22.5.2 |
| Automatic memory management | Complete | 5 of 5, 100% | Allow the runtime to reclaim unreachable managed objects. | ECMA-334 7th edition, clause 7.9 |
| Base access | In progress | 3 of 10, 30% | Access inherited implementations explicitly using base. | ECMA-334 7th edition, clause 12.8.14 |
| Bitwise and logical operators | Complete | 41 of 41, 100% | Apply &, \|, or ^ to integral, enumeration, or Boolean operands. | ECMA-334 7th edition, clause 12.13 |
| Bitwise complement operator | No tests yet |  | Invert integral bits using ~. | ECMA-334 7th edition, clause 12.9.5 |
| Blocks | Complete | 7 of 7, 100% | Group statements and local declarations into a lexical scope. | ECMA-334 7th edition, clause 13.3 |
| Boolean literals | Complete | 1 of 1, 100% | Write true and false Boolean values. | ECMA-334 7th edition, clause 6.4.5.2 |
| Boolean type | In progress | 3 of 4, 75% | Represent logical truth values using bool. | ECMA-334 7th edition, clause 8.3.9 |
| Boxing conversions | Complete | 2 of 2, 100% | Wrap value-type values as object or interface references. | ECMA-334 7th edition, clause 10.2.9 |
| Break statement | In progress | 5 of 6, 83% | Exit the nearest enclosing loop or switch. | ECMA-334 7th edition, clause 13.10.2 |
| Cast expressions | Complete | 5 of 5, 100% | Request explicit conversions using a parenthesized target type. | ECMA-334 7th edition, clause 12.9.7 |
| Character literals | Complete | 4 of 4, 100% | Write individual characters, including escaped characters. | ECMA-334 7th edition, clause 6.4.5.5 |
| Character type | In progress | 23 of 32, 71% | Represent UTF-16 code units using char. | ECMA-334 7th edition, clause 8.3.6 |
| Checked and unchecked expressions | Complete | 19 of 19, 100% | Control overflow checking for integral operations and conversions. | ECMA-334 7th edition, clause 12.8.19 |
| Checked and unchecked statements | Complete | 4 of 4, 100% | Control integral overflow checking within a statement block. | ECMA-334 7th edition, clause 13.12 |
| Class inheritance | In progress | 23 of 30, 76% | Derive a class from a base class and inherit members. | ECMA-334 7th edition, clauses 15.2.4, 15.3.4 |
| Classes | In progress | 37 of 45, 82% | Declare reference types containing state and behavior. | ECMA-334 7th edition, clause 15.2 |
| Comments | Complete | 13 of 13, 100% | Annotate source with single-line or delimited comments. | ECMA-334 7th edition, clause 6.3.3 |
| Comparison operators | In progress | 79 of 151, 52% | Compare numeric or enumeration values using ordering operators. | ECMA-334 7th edition, clauses 12.12.2, 12.12.3, 12.12.4, 12.12.6 |
| Compound assignment | In progress | 1 of 4, 25% | Combine an operation with assignment to its left operand. | ECMA-334 7th edition, clause 12.21.4 |
| Conditional compilation directives | Complete | 10 of 10, 100% | Include source selectively using #if, #elif, #else, and #endif. | ECMA-334 7th edition, clause 6.5.5 |
| Conditional compilation symbols | Complete | 9 of 9, 100% | Define or undefine symbols used by preprocessing conditions. | ECMA-334 7th edition, clause 6.5.2 |
| Conditional logical operators | In progress | 2 of 4, 50% | Evaluate Boolean && or \|\| with short-circuiting. | ECMA-334 7th edition, clause 12.14.2 |
| Conditional methods | In progress | 2 of 9, 22% | Omit calls to methods according to compilation symbols. | ECMA-334 7th edition, clause 22.5.3 |
| Conditional operator | Complete | 4 of 4, 100% | Choose one of two expressions using a Boolean condition. | ECMA-334 7th edition, clause 12.18 |
| Constant expressions | In progress | 588 of 932, 63% | Evaluate permitted expressions at compile time. | ECMA-334 7th edition, clause 12.23 |
| Constants | In progress | 61 of 67, 91% | Declare compile-time values using const. | ECMA-334 7th edition, clauses 13.6.3, 15.4 |
| Constructor initializers | In progress | 10 of 24, 41% | Invoke another instance constructor using this or base. | ECMA-334 7th edition, clause 15.11.2 |
| Continue statement | Complete | 6 of 6, 100% | Advance to the next iteration of the nearest loop. | ECMA-334 7th edition, clause 13.10.3 |
| Custom event accessors | Complete | 6 of 6, 100% | Control event subscription using explicit add and remove accessors. | ECMA-334 7th edition, clause 15.8.3 |
| Decimal type | In progress | 15 of 16, 93% | Represent decimal arithmetic values using decimal. | ECMA-334 7th edition, clause 8.3.8 |
| Default value initialization | In progress | 21 of 30, 70% | Automatically initialize fields, array elements, and default-constructed value types. | ECMA-334 7th edition, clauses 9.3, 8.3.3 |
| Delegate creation | In progress | 5 of 9, 55% | Create delegates bound to instance or static methods. | ECMA-334 7th edition, clause 20.5 |
| Delegate invocation | In progress | 6 of 7, 85% | Invoke a delegate's callable target or invocation list. | ECMA-334 7th edition, clause 20.6 |
| Delegates | In progress | 10 of 11, 90% | Declare callable reference types with a specific signature. | ECMA-334 7th edition, clause 20.2 |
| Diagnostic directives | In progress | 13 of 37, 35% | Issue explicit compiler errors or warnings using #error and #warning. | ECMA-334 7th edition, clause 6.5.6 |
| Do statement | Complete | 7 of 7, 100% | Repeat a statement, testing its condition after each iteration. | ECMA-334 7th edition, clause 13.9.3 |
| Element access | In progress | 14 of 18, 77% | Read or write array elements and indexer values. | ECMA-334 7th edition, clause 12.8.11 |
| Empty statement | Complete | 1 of 1, 100% | Use a semicolon as a statement that performs no action. | ECMA-334 7th edition, clause 13.4 |
| Enumeration conversions | Complete | 1 of 1, 100% | Cast between enumeration types and numeric types. | ECMA-334 7th edition, clause 10.3.3 |
| Enums | In progress | 18 of 23, 78% | Declare named integral constants in a distinct value type. | ECMA-334 7th edition, clauses 19.2, 19.4 |
| Equality operators | In progress | 93 of 170, 54% | Compare numbers, Booleans, enumerations, references, strings, or delegates for equality. | ECMA-334 7th edition, clause 12.12 |
| Event assignment | Complete | 1 of 1, 100% | Subscribe or unsubscribe delegate handlers using += or -=. | ECMA-334 7th edition, clause 12.21.5 |
| Explicit interface implementation | In progress | 5 of 13, 38% | Qualify implementations with interface names to separate conflicting contracts. | ECMA-334 7th edition, clause 18.6.2 |
| Explicit numeric conversions | In progress | 952 of 976, 97% | Cast between numeric types, possibly losing range or precision. | ECMA-334 7th edition, clause 10.3.2 |
| Expression statements | Complete | 7 of 7, 100% | Execute permitted expressions as standalone statements. | ECMA-334 7th edition, clause 13.7 |
| External events | In progress | 28 of 50, 56% | Declare events whose accessors are implemented externally. | ECMA-334 7th edition, clause 15.8 |
| External indexers | In progress | 36 of 44, 81% | Declare indexers whose accessors are implemented externally. | ECMA-334 7th edition, clause 15.9 |
| External methods | Complete | 4 of 4, 100% | Declare method implementations supplied outside the compilation unit. | ECMA-334 7th edition, clause 15.6.8 |
| External properties | In progress | 67 of 84, 79% | Declare properties whose accessors are implemented externally. | ECMA-334 7th edition, clause 15.7 |
| Field initializers | In progress | 26 of 28, 92% | Initialize fields as part of instance or static initialization. | ECMA-334 7th edition, clauses 15.5.5, 15.5.6 |
| Field-like events | In progress | 1 of 3, 33% | Declare events whose delegate storage and accessors are synthesized. | ECMA-334 7th edition, clause 15.8.2 |
| Fields | In progress | 113 of 132, 85% | Declare instance or static storage inside classes and structs. | ECMA-334 7th edition, clause 15.5 |
| Finalizers | In progress | 20 of 25, 80% | Declare cleanup invoked by the runtime when objects become eligible. | ECMA-334 7th edition, clause 15.13 |
| Finally clauses | In progress | 21 of 24, 87% | Run cleanup when control leaves a try statement. | ECMA-334 7th edition, clause 13.11 |
| Fixed statement | In progress | 8 of 19, 42% | Pin movable managed storage while using its address. | ECMA-334 7th edition, clause 23.7 |
| Floating-point types | Complete | 5 of 5, 100% | Represent approximate real numbers using float and double. | ECMA-334 7th edition, clause 8.3.7 |
| For statement | Complete | 12 of 12, 100% | Repeat with initialization, condition, and iteration expressions. | ECMA-334 7th edition, clause 13.9.4 |
| Foreach statement | In progress | 16 of 33, 48% | Enumerate arrays or collections using the enumeration pattern or interfaces. | ECMA-334 7th edition, clause 13.9.5 |
| Goto statement | In progress | 6 of 12, 50% | Transfer control to a label or switch section. | ECMA-334 7th edition, clause 13.10.4 |
| If statement | In progress | 6 of 7, 85% | Execute branches according to a Boolean condition. | ECMA-334 7th edition, clause 13.8.2 |
| Implicit constant expression conversions | In progress | 57 of 71, 80% | Convert representable constant integers to permitted smaller integral types. | ECMA-334 7th edition, clause 10.2.11 |
| Implicit enumeration conversions | Complete | 1 of 1, 100% | Convert the integer constant zero to an enumeration type. | ECMA-334 7th edition, clause 10.2.4 |
| Implicit numeric conversions | Complete | 671 of 671, 100% | Convert numeric values without casts when permitted by the language. | ECMA-334 7th edition, clause 10.2.3 |
| Increment and decrement operators | In progress | 17 of 31, 54% | Increase or decrease variables with prefix or postfix operators. | ECMA-334 7th edition, clauses 12.8.15, 12.9.6 |
| Indexer overloading | No tests yet |  | Declare indexers with different parameter signatures. | ECMA-334 1st edition, clause 17.8.1 |
| IndexerName attribute | Complete | 1 of 1, 100% | Choose an indexer's metadata name through a compiler-recognized attribute. | ECMA-334 7th edition, clause 22.6 |
| Indexers | In progress | 36 of 44, 81% | Expose get and set accessors indexed by one or more parameters. | ECMA-334 7th edition, clause 15.9 |
| Inherited interface implementation | Complete | 4 of 4, 100% | Inherit interface implementations from a base class. | ECMA-334 7th edition, clause 18.6.6 |
| Instance constructors | In progress | 52 of 69, 75% | Initialize new class or struct instances. | ECMA-334 7th edition, clause 15.11 |
| Integer literals | Complete | 29 of 29, 100% | Write decimal or hexadecimal integers with optional type suffixes. | ECMA-334 7th edition, clause 6.4.5.3 |
| Integral types | In progress | 23 of 32, 71% | Store signed or unsigned integers using predefined fixed-width types. | ECMA-334 7th edition, clause 8.3.6 |
| Interface implementation | In progress | 33 of 44, 75% | Supply implementations of interface methods, properties, events, and indexers. | ECMA-334 7th edition, clause 18.6 |
| Interface inheritance | Complete | 4 of 4, 100% | Extend interface contracts through one or more base interfaces. | ECMA-334 7th edition, clause 18.2.4 |
| Interface reimplementation | In progress | 3 of 4, 75% | Replace a base class's interface mapping in a derived class. | ECMA-334 7th edition, clause 18.6.7 |
| Interfaces | In progress | 9 of 11, 81% | Declare contracts implemented by classes or structs. | ECMA-334 7th edition, clause 18.2 |
| Invocation expressions | In progress | 9 of 10, 90% | Call methods or delegates with argument lists. | ECMA-334 7th edition, clause 12.8.9 |
| Is operator | In progress | 2 of 3, 66% | Test whether a value is compatible with a type. | ECMA-334 7th edition, clause 12.12.12 |
| Jagged arrays | Complete | 10 of 10, 100% | Create arrays whose elements are other arrays. | ECMA-334 7th edition, clause 17.2 |
| Labeled statements | In progress | 2 of 5, 40% | Name statement locations for goto transfers. | ECMA-334 7th edition, clause 13.5 |
| Line directives | In progress | 11 of 35, 31% | Remap reported source line numbers and filenames, or restore defaults. | ECMA-334 7th edition, clause 6.5.8 |
| Local variables | Complete | 17 of 17, 100% | Declare explicitly typed local storage with optional initializers. | ECMA-334 7th edition, clause 13.6.2 |
| Lock statement | Complete | 6 of 6, 100% | Acquire a mutual-exclusion monitor for a statement's duration. | ECMA-334 7th edition, clause 13.13 |
| Logical negation operator | No tests yet |  | Invert a Boolean value using !. | ECMA-334 7th edition, clause 12.9.4 |
| Member access | In progress | 40 of 41, 97% | Select members with qualified names and the dot operator. | ECMA-334 7th edition, clause 12.8.7 |
| Member hiding | In progress | 1 of 4, 25% | Hide inherited members explicitly using new. | ECMA-334 7th edition, clause 15.3.5 |
| Method overloading | In progress | 49 of 54, 90% | Declare methods sharing a name with distinct signatures. | ECMA-334 7th edition, clauses 7.6, 12.6.4 |
| Methods | In progress | 148 of 166, 89% | Declare callable instance or static members with bodies. | ECMA-334 7th edition, clause 15.6 |
| Module attributes | In progress | 16 of 31, 51% | Attach metadata attributes to the current module using the module target. | ECMA-334 7th edition, clause 22.3 |
| Multicast delegates | In progress | 47 of 129, 36% | Combine or remove delegate targets using addition and subtraction. | ECMA-334 7th edition, clauses 12.10.5, 12.10.6 |
| Multidimensional arrays | Complete | 10 of 10, 100% | Store elements indexed by multiple dimensions. | ECMA-334 7th edition, clause 17.2 |
| Namespaces | In progress | 4 of 5, 80% | Organize type declarations within named or nested namespaces. | ECMA-334 7th edition, clause 14.3 |
| Nested types | In progress | 27 of 30, 90% | Declare types within other types. | ECMA-334 7th edition, clause 15.3.9 |
| Null literal | Complete | 1 of 1, 100% | Represent the absence of a reference value. | ECMA-334 7th edition, clause 6.4.5.7 |
| Object creation expressions | In progress | 23 of 25, 92% | Construct instances using new and constructor arguments. | ECMA-334 7th edition, clause 12.8.16 |
| Object type | Complete | 1 of 1, 100% | Treat values through the common object base type. | ECMA-334 7th edition, clause 8.2.3 |
| Obsolete attribute | In progress | 1 of 2, 50% | Report warnings or errors when obsolete declarations are used. | ECMA-334 7th edition, clause 22.5.4 |
| Output parameters | In progress | 53 of 54, 98% | Return values through definitely assigned out parameters. | ECMA-334 7th edition, clause 15.6.2 |
| Override methods | In progress | 18 of 19, 94% | Provide implementations for inherited virtual or abstract methods. | ECMA-334 7th edition, clause 15.6.5 |
| Parameter arrays | In progress | 53 of 54, 98% | Accept a variable number of arguments through a final params array. | ECMA-334 7th edition, clause 15.6.2 |
| Parenthesized expressions | In progress | 2 of 4, 50% | Group expressions explicitly to control evaluation precedence. | ECMA-334 7th edition, clause 12.8.5 |
| Pointer arithmetic | In progress | 15 of 22, 68% | Advance pointers or calculate element distances. | ECMA-334 7th edition, clauses 23.6.6, 23.6.7 |
| Pointer arrays | In progress | 30 of 38, 78% | Store pointer values as elements of managed arrays in unsafe code. | ECMA-334 7th edition, clauses 23.3, 17.2 |
| Pointer comparison | In progress | 1 of 7, 14% | Compare pointer addresses with equality or ordering operators. | ECMA-334 7th edition, clause 23.6.8 |
| Pointer conversions | In progress | 29 of 40, 72% | Cast between pointer types and supported integral types. | ECMA-334 7th edition, clause 23.5 |
| Pointer element access | Complete | 9 of 9, 100% | Index pointer-addressed storage using brackets. | ECMA-334 7th edition, clause 23.6.4 |
| Pointer indirection | In progress | 2 of 3, 66% | Read or write the value addressed by a pointer. | ECMA-334 7th edition, clause 23.6.2 |
| Pointer member access | In progress | 3 of 4, 75% | Access pointed-to struct members using ->. | ECMA-334 7th edition, clause 23.6.3 |
| Pointer types | In progress | 20 of 28, 71% | Represent addresses of unmanaged values using typed or void pointers. | ECMA-334 7th edition, clause 23.3 |
| Properties | In progress | 67 of 84, 79% | Expose instance or static values through get and set accessors. | ECMA-334 7th edition, clause 15.7 |
| Readonly fields | In progress | 16 of 25, 64% | Restrict field assignments to declarations and appropriate constructors. | ECMA-334 7th edition, clause 15.5.3 |
| Real literals | Complete | 9 of 9, 100% | Write floating-point or decimal constants with optional exponents and suffixes. | ECMA-334 7th edition, clause 6.4.5.4 |
| Reference conversions | Complete | 2 of 2, 100% | Convert compatible class, interface, array, and delegate references. | ECMA-334 7th edition, clauses 10.2.8, 10.3.5 |
| Reference parameters | In progress | 53 of 54, 98% | Pass writable variable references using ref. | ECMA-334 7th edition, clause 15.6.2 |
| Region directives | Complete | 3 of 3, 100% | Group source text with #region and #endregion. | ECMA-334 7th edition, clause 6.5.7 |
| Regular string literals | Complete | 4 of 4, 100% | Write strings with backslash escapes. | ECMA-334 7th edition, clause 6.4.5.6 |
| Return statement | Complete | 11 of 11, 100% | Exit a function member, optionally returning a value. | ECMA-334 7th edition, clause 13.10.5 |
| Sealed classes | In progress | 19 of 20, 95% | Prevent further class inheritance. | ECMA-334 7th edition, clause 15.2.2 |
| Sealed overrides | Complete | 3 of 3, 100% | Prevent further overriding of an overridden member. | ECMA-334 7th edition, clause 15.6.6 |
| Shift operators | In progress | 18 of 20, 90% | Shift integral bits left or right. | ECMA-334 7th edition, clause 12.11 |
| Simple assignment | Complete | 2 of 2, 100% | Store a computed value in a variable, property, or indexer. | ECMA-334 7th edition, clause 12.21.2 |
| Single-dimensional arrays | Complete | 10 of 10, 100% | Store indexed elements in a single dimension. | ECMA-334 7th edition, clause 17.2 |
| Sizeof operator | In progress | 2 of 4, 50% | Obtain a value type's storage size. | ECMA-334 7th edition, clause 23.6.9 |
| Stack allocation | In progress | 1 of 2, 50% | Allocate unmanaged storage on the stack using stackalloc. | ECMA-334 7th edition, clause 23.9 |
| Static constructors | In progress | 27 of 38, 71% | Initialize type-level state before its prescribed first use. | ECMA-334 7th edition, clause 15.12 |
| String concatenation | In progress | 24 of 61, 39% | Join string values using addition. | ECMA-334 7th edition, clause 12.10.5 |
| String type | Complete | 2 of 2, 100% | Represent immutable character sequences using string. | ECMA-334 7th edition, clause 8.2.5 |
| Structs | Complete | 9 of 9, 100% | Declare value types containing fields and behavior. | ECMA-334 7th edition, clause 16.2 |
| Switch statement | Complete | 14 of 14, 100% | Select statement sections using constant case labels or default. | ECMA-334 7th edition, clause 13.8.3 |
| This access | In progress | 6 of 9, 66% | Refer to the current instance using this. | ECMA-334 7th edition, clause 12.8.13 |
| Throw statement | In progress | 8 of 12, 66% | Raise an exception or rethrow the current exception. | ECMA-334 7th edition, clause 13.10.6 |
| True and false operators | In progress | 18 of 43, 41% | Declare truth operators used in Boolean contexts and conditional logical operations. | ECMA-334 7th edition, clauses 15.10.2, 12.24 |
| Try-catch statements | In progress | 21 of 24, 87% | Handle exceptions using typed or general catch clauses. | ECMA-334 7th edition, clause 13.11 |
| Typeof operator | Complete | 3 of 3, 100% | Obtain a System.Type object describing a type. | ECMA-334 7th edition, clause 12.8.17 |
| Unary plus and minus operators | In progress | 3 of 4, 75% | Preserve or negate numeric values. | ECMA-334 7th edition, clauses 12.9.2, 12.9.3 |
| Unboxing conversions | Complete | 3 of 3, 100% | Recover value-type values from boxed object or interface references. | ECMA-334 7th edition, clause 10.3.7 |
| Unicode escape sequences | Complete | 8 of 8, 100% | Represent Unicode characters with hexadecimal escapes in identifiers and literals. | ECMA-334 7th edition, clause 6.4.2 |
| Unicode identifiers | In progress | 196 of 198, 98% | Name program elements using Unicode letters and permitted combining characters. | ECMA-334 7th edition, clause 6.4.3 |
| Unsafe contexts | In progress | 12 of 13, 92% | Permit pointer operations within explicitly unsafe declarations or blocks. | ECMA-334 7th edition, clause 23.2 |
| User-defined binary operators | In progress | 14 of 39, 35% | Declare overloads for binary operations on user-defined types. | ECMA-334 7th edition, clause 15.10.3 |
| User-defined conditional logical operators | Not yet | 0 of 2, 0% | Short-circuit user-defined types through overloaded logical and truth operators. | ECMA-334 7th edition, clause 12.14.3 |
| User-defined explicit conversions | In progress | 7 of 16, 43% | Declare conversion operators requiring explicit casts. | ECMA-334 7th edition, clause 15.10.4 |
| User-defined implicit conversions | In progress | 7 of 16, 43% | Declare conversion operators invoked without explicit casts. | ECMA-334 7th edition, clause 15.10.4 |
| User-defined unary operators | In progress | 13 of 37, 35% | Declare overloads for unary operations on user-defined types. | ECMA-334 7th edition, clause 15.10.2 |
| Using alias directives | In progress | 5 of 7, 71% | Give a namespace or named type a local alias. | ECMA-334 7th edition, clause 14.5.2 |
| Using namespace directives | In progress | 5 of 6, 83% | Import namespace members for unqualified type references. | ECMA-334 7th edition, clause 14.5.3 |
| Using statement | In progress | 11 of 17, 64% | Dispose a resource when control leaves its protected statement. | ECMA-334 7th edition, clause 13.14 |
| Value parameters | In progress | 53 of 54, 98% | Pass parameter values by copying. | ECMA-334 7th edition, clause 15.6.2 |
| Verbatim identifiers | In progress | 196 of 198, 98% | Prefix identifiers with @ to use keywords as names. | ECMA-334 7th edition, clause 6.4.3 |
| Verbatim string literals | Complete | 4 of 4, 100% | Write multiline strings that preserve backslashes and escape quotes by doubling. | ECMA-334 7th edition, clause 6.4.5.6 |
| Virtual events | In progress | 10 of 16, 62% | Declare virtual or abstract events and specialize or seal their overrides. | ECMA-334 7th edition, clause 15.8.5 |
| Virtual indexers | In progress | 36 of 44, 81% | Declare virtual or abstract indexers and specialize or seal their overrides. | ECMA-334 7th edition, clause 15.9 |
| Virtual methods | Complete | 4 of 4, 100% | Permit derived classes to replace method behavior. | ECMA-334 7th edition, clause 15.6.4 |
| Virtual properties | In progress | 18 of 22, 81% | Declare virtual or abstract properties and specialize or seal their overrides. | ECMA-334 7th edition, clause 15.7.6 |
| Volatile fields | In progress | 29 of 34, 85% | Apply volatile access semantics to permitted field types. | ECMA-334 7th edition, clause 15.5.4 |
| While statement | Complete | 7 of 7, 100% | Repeat a statement while a condition remains true. | ECMA-334 7th edition, clause 13.9.2 |
| XML documentation comments | No tests yet |  | Attach structured documentation to declarations using XML comments. | ECMA-334 1st edition, clause E.1 |
| XML documentation inclusion | No tests yet |  | Include selected external XML fragments in generated API documentation. | ECMA-334 5th edition, clause D.3.6 |

## C# 2.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Accessor accessibility | In progress | 77 of 90, 85% | Give one property or indexer accessor more restrictive accessibility. | ECMA-334 7th edition, clauses 15.7.3, 15.9 |
| Anonymous methods | Not yet | 0 of 7, 0% | Create delegate bodies using delegate expressions. | ECMA-334 7th edition, clause 12.19 |
| Array conversions to generic interfaces | Not yet | 0 of 8, 0% | Convert single-dimensional arrays to compatible generic list and collection interfaces. | ECMA-334 7th edition, clause 17.2.3 |
| As operator with nullable value types | No tests yet |  | Attempt conversions to nullable value types without throwing on incompatibility. | ECMA-334 4th edition, clause 14.9.11 |
| Attributes on type parameters | No tests yet |  | Attach metadata attributes to generic type or method parameters. | ECMA-334 3rd edition, clauses 24.2, 25.1.1 |
| Conditional attribute classes | Not yet | 0 of 1, 0% | Omit attribute applications according to compilation symbols. | ECMA-334 7th edition, clause 22.5.3.3 |
| Default value expressions | In progress | 3 of 5, 60% | Obtain a type's default value using default(T). | ECMA-334 7th edition, clause 12.8.20 |
| Delegate covariance and contravariance | In progress | 2 of 9, 22% | Bind delegate targets with compatible reference return and parameter types. | ECMA-334 7th edition, clause 10.8 |
| Extern aliases | In progress | 1 of 4, 25% | Disambiguate namespaces from separately aliased referenced assemblies. | ECMA-334 7th edition, clause 14.4 |
| Fixed-size buffers | In progress | 18 of 39, 46% | Embed fixed-length primitive buffers inside unsafe structs. | ECMA-334 7th edition, clause 23.8 |
| Generic classes | In progress | 27 of 42, 64% | Declare classes parameterized by one or more types. | ECMA-334 7th edition, clauses 15.2.3, 8.4 |
| Generic delegates | No tests yet |  | Declare callable types parameterized by one or more types. | ECMA-334 3rd edition, clause 25.4 |
| Generic interfaces | No tests yet |  | Declare contracts parameterized by one or more types. | ECMA-334 3rd edition, clause 25.3 |
| Generic method type inference | In progress | 11 of 17, 64% | Infer generic method arguments from supplied argument types. | ECMA-334 7th edition, clause 12.6.3 |
| Generic methods | No tests yet |  | Declare methods with their own type parameters. | ECMA-334 3rd edition, clause 25.6 |
| Generic structs | No tests yet |  | Declare value types parameterized by one or more types. | ECMA-334 3rd edition, clause 25.2 |
| Generic type constraints | In progress | 47 of 106, 44% | Restrict type arguments by base types, interfaces, class, struct, or new(). | ECMA-334 7th edition, clauses 15.2.5, 8.4.5 |
| Hidden line directives | In progress | 11 of 35, 31% | Hide debugger sequence points using #line hidden. | ECMA-334 7th edition, clause 6.5.8 |
| Iterators | In progress | 2 of 67, 2% | Produce enumerable sequences incrementally using yield return and yield break. | ECMA-334 7th edition, clauses 15.14, 13.15 |
| Lifted operators | In progress | 2 of 4, 50% | Apply supported operators to nullable value operands. | ECMA-334 7th edition, clause 12.4.8 |
| Lifted user-defined conversions | No tests yet |  | Apply value-type conversion operators to nullable values, preserving null. | ECMA-334 7th edition, clause 10.6.2 |
| Method group conversions | In progress | 2 of 9, 22% | Convert compatible method groups directly to delegates. | ECMA-334 7th edition, clause 10.8 |
| Namespace alias qualifier | In progress | 14 of 15, 93% | Resolve namespace or type names through alias::, including global::. | ECMA-334 7th edition, clause 14.8 |
| Null-coalescing operator | No tests yet |  | Select a fallback value when the first operand is null. | ECMA-334 7th edition, clause 12.15 |
| Nullable Boolean logical operators | Not yet | 0 of 2, 0% | Apply three-valued logic to nullable Boolean operands with & and \|. | ECMA-334 7th edition, clause 12.13.5 |
| Nullable boxing and unboxing | In progress | 4 of 5, 80% | Box underlying values or null; unbox into compatible nullable values. | ECMA-334 7th edition, clauses 10.2.9, 10.3.7 |
| Nullable conversions | In progress | 1,559 of 2,922, 53% | Convert between nullable and underlying values with wrapping or unwrapping. | ECMA-334 7th edition, clause 10.6 |
| Nullable value types | In progress | 7 of 8, 87% | Represent either a value-type value or null using T?. | ECMA-334 7th edition, clause 8.3.12 |
| Partial types | Complete | 9 of 9, 100% | Split class, struct, or interface declarations across source locations. | ECMA-334 7th edition, clause 15.2.7 |
| Pragma checksum directives | More tests required | 2 of 2, 100% | Specify external source-document checksums with #pragma checksum. | ECMA-334 7th edition, clause 6.5.9 |
| Pragma warning directives | Complete | 2 of 2, 100% | Disable or restore compiler warnings with #pragma warning. | ECMA-334 7th edition, clause 6.5.9 |
| Sizeof in safe code | In progress | 2 of 4, 50% | Use sizeof for predefined unmanaged types outside unsafe contexts. | ECMA-334 7th edition, clause 23.6.9 |
| Static classes | In progress | 9 of 22, 40% | Declare types containing only static members and prohibit instantiation. | ECMA-334 7th edition, clause 15.2.2.4 |
| Switch on Boolean values | In progress | 14 of 15, 93% | Use true and false as switch case labels. | ECMA-334 7th edition, clause 13.8.3 |
| Switch on nullable values | In progress | 14 of 15, 93% | Use nullable switch operands and match null with a case label. | ECMA-334 7th edition, clause 13.8.3 |
| Type parameter conversions | In progress | 1 of 3, 33% | Convert generic parameter values according to their declared constraints. | ECMA-334 7th edition, clauses 10.2.12, 10.3.9 |
| Unbound generic types in typeof | In progress | 5 of 6, 83% | Refer to generic definitions with omitted arguments inside typeof. | ECMA-334 7th edition, clause 12.8.17 |
| XML documentation for type parameters | No tests yet |  | Document and reference generic type parameters in XML comments. | ECMA-334 3rd edition, clauses E.1, E.2.15, E.2.16 |

## C# 3.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Anonymous types | Not yet | 0 of 13, 0% | Create unnamed immutable types with inferred properties. | ECMA-334 7th edition, clause 12.8.16.7 |
| Auto-implemented properties | Complete | 3 of 3, 100% | Synthesize backing storage for properties with semicolon accessors. | ECMA-334 7th edition, clause 15.7.4 |
| Collection initializers | In progress | 6 of 7, 85% | Add elements to a new collection using initializer syntax. | ECMA-334 7th edition, clause 12.8.16.4 |
| Expression trees | Not yet | 0 of 1, 0% | Convert supported lambdas into inspectable expression-tree objects. | ECMA-334 7th edition, clause 8.6 |
| Extension methods | Not yet | 0 of 9, 0% | Invoke static methods as instance-style calls through a this parameter. | ECMA-334 7th edition, clause 15.6.10 |
| Implicitly typed arrays | In progress | 19 of 28, 67% | Infer an array's element type from its initializer values. | ECMA-334 7th edition, clause 12.8.16.5 |
| Implicitly typed local variables | In progress | 41 of 63, 65% | Infer local variable types using var and an initializer. | ECMA-334 7th edition, clauses 13.6.2, 13.9.5 |
| Lambda expressions | In progress | 24 of 51, 47% | Write anonymous functions using =>. | ECMA-334 7th edition, clause 12.19 |
| Object initializers | In progress | 6 of 9, 66% | Assign accessible members when constructing an object. | ECMA-334 7th edition, clause 12.8.16.3 |
| Partial methods | In progress | 27 of 34, 79% | Declare optional private void hooks whose unused calls can disappear. | ECMA-334 7th edition, clause 15.6.9 |
| Query expressions | In progress | 8 of 33, 24% | Express filtering, projection, ordering, grouping, joins, and continuations with query syntax. | ECMA-334 7th edition, clause 12.20 |

## C# 4.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Dynamic binding | In progress | 7 of 14, 50% | Resolve selected operations at runtime using dynamic. | ECMA-334 7th edition, clauses 8.7, 12.3.3 |
| Generic covariance and contravariance | In progress | 35 of 44, 79% | Permit compatible conversions between variant generic interfaces and delegates. | ECMA-334 7th edition, clauses 18.2.3, 20.2 |
| Named arguments | In progress | 19 of 22, 86% | Identify call arguments by their parameter names. | ECMA-334 7th edition, clause 12.6.2 |
| Optional parameters | In progress | 63 of 86, 73% | Declare default parameter values used when arguments are omitted. | ECMA-334 7th edition, clause 15.6.2 |

## C# 5.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Asynchronous functions | In progress | 39 of 70, 55% | Suspend async methods or anonymous functions using await. | ECMA-334 7th edition, clauses 15.15, 12.9.8 |
| Caller information attributes | Not yet | 0 of 28, 0% | Supply caller member names, file paths, or line numbers to optional parameters. | ECMA-334 7th edition, clause 22.5.5 |

## C# 6.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Auto-property initializers | In progress | 6 of 7, 85% | Initialize automatically implemented properties directly in their declarations. | ECMA-334 7th edition, clause 15.7.4 |
| Await in catch and finally blocks | In progress | 15 of 19, 78% | Await asynchronous operations within catch or finally blocks. | ECMA-334 7th edition, clause 12.9.8 |
| Enum underlying type names | No tests yet |  | Specify an enum's integral underlying type using a qualified type name. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-6.0/enum-base-type.md) |
| Exception filters | In progress | 24 of 31, 77% | Select catch clauses using when expressions before stack unwinding. | ECMA-334 7th edition, clause 13.11 |
| Expression-bodied conversion operators | No tests yet |  | Define conversion bodies with a single expression. | ECMA-334 6th edition, clause 14.10.1 |
| Expression-bodied indexers | In progress | 62 of 78, 79% | Define indexer getters directly using =>. | ECMA-334 7th edition, clause 15.9 |
| Expression-bodied methods | In progress | 66 of 90, 73% | Define method bodies with a single expression. | ECMA-334 7th edition, clause 15.6.1 |
| Expression-bodied operators | No tests yet |  | Define operator bodies with a single expression. | ECMA-334 6th edition, clause 14.10.1 |
| Expression-bodied read-only properties | In progress | 25 of 31, 80% | Define property getters directly using =>. | ECMA-334 7th edition, clause 15.7.1 |
| Extension Add methods in collection initializers | In progress | 6 of 8, 75% | Use extension Add methods when processing collection initializer elements. | ECMA-334 7th edition, clause 12.8.16.4 |
| Getter-only auto-properties | In progress | 6 of 7, 85% | Synthesize read-only properties assignable in constructors or initializers. | ECMA-334 7th edition, clause 15.7.4 |
| Indexer initializers | In progress | 7 of 14, 50% | Assign indexed elements inside object or collection initializers. | ECMA-334 7th edition, clause 12.8.16.3 |
| Interpolated strings | In progress | 22 of 28, 78% | Embed expressions and formatting into string literals using $ syntax. | ECMA-334 7th edition, clause 12.8.3 |
| Nameof expressions | In progress | 15 of 18, 83% | Produce names of referenced program elements as constant strings. | ECMA-334 7th edition, clause 12.8.22 |
| Null-conditional element access | In progress | 6 of 9, 66% | Access an indexed element only when its receiver is non-null. | ECMA-334 7th edition, clause 12.8.12 |
| Null-conditional member access | In progress | 11 of 17, 64% | Access members or invoke delegates only when receivers are non-null. | ECMA-334 7th edition, clauses 12.8.8, 12.8.10 |
| Null-conditional projection initializers | In progress | 8 of 27, 29% | Infer anonymous-type property names from null-conditional member access. | ECMA-334 7th edition, clauses 12.8.8, 12.8.16.7 |
| Using static directives | In progress | 4 of 6, 66% | Import accessible static members and nested types into scope. | ECMA-334 7th edition, clause 14.5.4 |

## C# 7.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Binary integer literals | Complete | 43 of 43, 100% | Write integers in base two using a 0b prefix. | ECMA-334 7th edition, clause 6.4.5.3 |
| Constant patterns | In progress | 3 of 7, 42% | Match values against constants, including null. | ECMA-334 7th edition, clause 11.2.3 |
| Declaration patterns | In progress | 10 of 11, 90% | Test a value's type and introduce a typed variable. | ECMA-334 7th edition, clause 11.2.2 |
| Deconstruction | In progress | 5 of 7, 71% | Extract tuple or Deconstruct results into separately assigned variables. | ECMA-334 7th edition, clause 12.7 |
| Digit separators | Complete | 53 of 53, 100% | Separate digits in numeric literals with underscores. | ECMA-334 7th edition, clauses 6.4.5.3, 6.4.5.4 |
| Discards | More tests required | 2 of 2, 100% | Use _ where an output, deconstruction result, or pattern value is unnecessary. | ECMA-334 7th edition, clause 9.2.9.1 |
| Expression-bodied accessors | In progress | 58 of 62, 93% | Define property, indexer, or event accessors using =>. | ECMA-334 7th edition, clauses 15.7.3, 15.8.3 |
| Expression-bodied constructors | In progress | 66 of 82, 80% | Define instance or static constructor bodies using =>. | ECMA-334 7th edition, clauses 15.11.1, 15.12 |
| Expression-bodied finalizers | In progress | 21 of 28, 75% | Define finalizer bodies using =>. | ECMA-334 7th edition, clause 15.13 |
| Generalized async return types | In progress | 15 of 24, 62% | Return task-like types implementing the async builder pattern. | ECMA-334 7th edition, clause 15.15.2 |
| Local functions | Not yet | 0 of 28, 0% | Declare named functions inside another function body. | ECMA-334 7th edition, clause 13.6.4 |
| Out variable declarations | In progress | 10 of 11, 90% | Declare explicitly or implicitly typed variables within out arguments. | ECMA-334 7th edition, clause 12.17 |
| Pattern matching in switch statements | In progress | 26 of 34, 76% | Select switch sections using patterns and optional when guards. | ECMA-334 7th edition, clause 13.8.3 |
| Ref locals | In progress | 3 of 6, 50% | Keep references to existing storage in local variables. | ECMA-334 7th edition, clause 13.6.2.4 |
| Ref returns | In progress | 169 of 229, 73% | Return references to storage from methods, delegates, properties, or indexers. | ECMA-334 7th edition, clauses 15.6.1, 15.7.1, 15.9.1, 20.2, 12.19.1 |
| Throw expressions | In progress | 6 of 9, 66% | Use throw within supported expression contexts. | ECMA-334 7th edition, clause 12.16 |
| Tuple conversions | In progress | 5 of 7, 71% | Convert tuples element by element between compatible tuple types. | ECMA-334 7th edition, clauses 10.2.13, 10.3.6 |
| Tuple types and expressions | In progress | 22 of 27, 81% | Represent ordered heterogeneous values with optional element names. | ECMA-334 7th edition, clauses 8.3.11, 12.8.6 |
| Var patterns | Not yet | 0 of 4, 0% | Match any value and bind it to an inferred variable. | ECMA-334 7th edition, clause 11.2.4 |

## C# 7.1

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Async Main | In progress | 20 of 30, 66% | Use task-returning Main methods as executable entry points. | ECMA-334 7th edition, clause 7.1 |
| Default literal expressions | In progress | 5 of 7, 71% | Infer the type of a default value from its context. | ECMA-334 7th edition, clause 12.8.20 |
| Inferred tuple element names | In progress | 7 of 14, 50% | Infer tuple element names from source variables or member accesses. | ECMA-334 7th edition, clause 12.8.6 |
| Pattern matching with generic type parameters | In progress | 10 of 12, 83% | Match values whose static type is a generic type parameter. | ECMA-334 7th edition, clause 11.2.2 |

## C# 7.2

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Conditional ref expressions | In progress | 21 of 29, 72% | Select a reference to storage using a conditional expression. | ECMA-334 7th edition, clause 12.18 |
| In parameters | In progress | 13 of 17, 76% | Pass values by readonly reference using in. | ECMA-334 7th edition, clause 15.6.2.3 |
| Leading digit separators | Complete | 45 of 45, 100% | Place underscores after binary or hexadecimal integer prefixes. | ECMA-334 7th edition, clause 6.4.5.3 |
| Non-trailing named arguments | In progress | 10 of 11, 90% | Allow positional arguments after correctly positioned named arguments. | ECMA-334 7th edition, clause 12.6.2.2 |
| Private protected access | In progress | 37 of 38, 97% | Permit member access only to derived types within the assembly. | ECMA-334 7th edition, clause 7.5.2 |
| Readonly structs | In progress | 7 of 12, 58% | Declare structs whose instance fields and behavior preserve readonly semantics. | ECMA-334 7th edition, clause 16.2.2 |
| Ref extension methods | Not yet | 0 of 14, 0% | Pass struct extension-method receivers by writable or readonly reference. | ECMA-334 7th edition, clause 15.6.10 |
| Ref readonly locals | In progress | 4 of 9, 44% | Keep readonly references to existing storage in local variables. | ECMA-334 7th edition, clause 13.6.2.4 |
| Ref readonly returns | In progress | 167 of 217, 76% | Return readonly references from methods, delegates, properties, or indexers. | ECMA-334 7th edition, clauses 15.6.1, 15.7.1, 15.9.1, 20.2 |
| Ref structs | In progress | 9 of 17, 52% | Declare stack-confined structs that cannot become ordinary heap objects. | ECMA-334 7th edition, clause 16.2.3 |
| Stackalloc to Span | In progress | 1 of 2, 50% | Convert stackalloc storage to Span<T> without exposing pointers. | ECMA-334 7th edition, clause 23.9 |

## C# 7.3

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Attributes on backing fields | In progress | 20 of 53, 37% | Apply field-targeted attributes to auto-property backing fields. | ECMA-334 7th edition, clause 22.3 |
| Delegate constraints | In progress | 38 of 106, 35% | Allow System.Delegate or MulticastDelegate as generic base-type constraints. | ECMA-334 7th edition, clause 15.2.5 |
| Enum constraints | In progress | 38 of 106, 35% | Allow System.Enum as a generic base-type constraint. | ECMA-334 7th edition, clause 15.2.5 |
| Expression variables in initializers and queries | In progress | 61 of 97, 62% | Allow out variables and patterns in additional initialization and query contexts. | ECMA-334 7th edition, clauses 7.3, 12.20 |
| Pattern-based fixed statements | In progress | 15 of 41, 36% | Pin types exposing a suitable GetPinnableReference method. | ECMA-334 7th edition, clause 23.7 |
| Ref for-loop variables | In progress | 40 of 46, 86% | Declare reference variables in a for loop's initializer. | ECMA-334 7th edition, clauses 13.9.4, 13.6.2 |
| Ref foreach iteration variables | In progress | 26 of 55, 47% | Iterate using writable or readonly references to collection elements. | ECMA-334 7th edition, clause 13.9.5 |
| Ref reassignment | In progress | 6 of 10, 60% | Rebind a ref local to reference different storage. | ECMA-334 7th edition, clause 12.21.3 |
| Stackalloc initializers | In progress | 6 of 7, 85% | Initialize stack-allocated storage using explicit or inferred element lists. | ECMA-334 7th edition, clause 23.9 |
| Tuple equality operators | In progress | 3 of 9, 33% | Compare tuples element by element with == and !=. | ECMA-334 7th edition, clause 12.12.11 |
| Unmanaged constraints | In progress | 38 of 106, 35% | Require generic type arguments to be unmanaged types. | ECMA-334 7th edition, clause 15.2.5 |
| Unpinned fixed buffer indexing | In progress | 6 of 11, 54% | Index fixed buffers through movable receivers without a fixed statement. | ECMA-334 7th edition, clause 23.8.3 |

## C# 8.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Alternative interpolated verbatim strings | No tests yet |  | Start interpolated verbatim strings with @$ as well as $@. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/alternative-interpolated-verbatim.md) |
| Async iterators | No tests yet |  | Produce asynchronous sequences with async methods containing yield statements. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/async-streams.md) |
| Await foreach | No tests yet |  | Consume asynchronous enumerations using await foreach. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/async-streams.md) |
| Await using declarations | No tests yet |  | Await resource disposal when a using declaration's scope ends. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/async-using.md) |
| Await using statements | No tests yet |  | Await resource disposal after leaving a using statement's body. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/async-using.md) |
| Constraints on overrides and explicit implementations | No tests yet |  | Specify class or struct constraints to disambiguate inherited nullable type parameters. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/constraints-in-overrides.md) |
| Default interface members | No tests yet |  | Provide default implementations and static or nonpublic members inside interfaces. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/default-interface-methods.md) |
| Discard patterns | No tests yet |  | Match any input without binding a variable using _. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/patterns.md) |
| Indices from the end | No tests yet |  | Use ^ to count backwards from a collection's end. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/ranges.md) |
| Notnull constraints | No tests yet |  | Require generic arguments to be nonnullable reference or value types. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/notnull-constraint.md) |
| Null constant patterns on pointers | No tests yet |  | Test pointer values for null using a constant pattern. | [compiler source](https://github.com/dotnet/roslyn/blob/67ebca858d83d631030cb1b17d97ae96f7c6a5e8/src/Compilers/CSharp/Portable/Errors/MessageID.cs) |
| Null-coalescing assignment | No tests yet |  | Assign a fallback value only when the destination is null. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/null-coalescing-assignment.md) |
| Null-coalescing with unconstrained type parameters | No tests yet |  | Use unconstrained generic values as the left operand of ??. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/unconstrained-null-coalescing.md) |
| Null-forgiving operator | Complete | 1 of 1, 100% | Suppress nullable flow warnings for an expression using postfix !. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/nullable-reference-types-specification.md) |
| Nullable analysis attributes | No tests yet |  | Describe parameter and return nullability contracts recognized by compiler flow analysis. | [Microsoft Learn](https://learn.microsoft.com/en-us/dotnet/csharp/language-reference/attributes/nullable-analysis) |
| Nullable directives | No tests yet |  | Control nullable annotation and warning contexts within source files. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/nullable-reference-types-specification.md) |
| Nullable reference type constraints | Complete | 1 of 1, 100% | Use class? to allow nullable reference type arguments. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/nullable-reference-types-specification.md) |
| Nullable reference types | Complete | 1 of 1, 100% | Express reference nullability and receive warnings for potentially unsafe null uses. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/nullable-reference-types-specification.md) |
| Obsolete property accessors | No tests yet |  | Mark individual property accessors obsolete without marking the entire property. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/obsolete-accessor.md) |
| Pattern-based disposal | No tests yet |  | Dispose ref structs through a suitable Dispose method without implementing IDisposable. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/using.md) |
| Positional patterns | No tests yet |  | Match values obtained through a type's Deconstruct method. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/patterns.md) |
| Property patterns | No tests yet |  | Match an object's accessible properties or fields against nested patterns. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/patterns.md) |
| Ranges | No tests yet |  | Use .. to describe contiguous slices of supported collections. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/ranges.md) |
| Readonly members | No tests yet |  | Mark individual struct members readonly to prevent changes to instance state. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/readonly-instance-members.md) |
| Stackalloc in nested expressions | Complete | 1 of 1, 100% | Use stackalloc expressions within additional expression contexts. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/nested-stackalloc.md) |
| Static local functions | No tests yet |  | Declare local functions that cannot capture enclosing variables or instance state. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/static-local-functions.md) |
| Switch expressions | Complete | 3 of 3, 100% | Select an expression result using ordered pattern arms and optional guards. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/patterns.md) |
| Tuple patterns | No tests yet |  | Match tuple elements positionally using nested patterns. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/patterns.md) |
| Unmanaged constructed types | No tests yet |  | Treat suitable constructed generic structs as unmanaged types. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/constructed-unmanaged.md) |
| Using declarations | No tests yet |  | Dispose local resources when control leaves their containing scope. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/using.md) |

## C# 9.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Attributes on local functions | No tests yet |  | Attach attributes to local functions and their parameters or type parameters. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/local-function-attributes.md) |
| Covariant return types | No tests yet |  | Return more derived reference types from overriding methods or readonly properties. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/covariant-returns.md) |
| Default constraints | No tests yet |  | Disambiguate inherited unconstrained nullable type parameters using where T : default. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/nullable-reference-types-specification.md) |
| Extended partial methods | No tests yet |  | Declare implemented partial methods with accessibility, nonvoid returns, or out parameters. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/extending-partial-methods.md) |
| Extension GetAsyncEnumerator | No tests yet |  | Use extension GetAsyncEnumerator methods to supply await foreach patterns. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/extension-getenumerator.md) |
| Extension GetEnumerator | No tests yet |  | Use extension GetEnumerator methods to supply foreach enumeration patterns. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/extension-getenumerator.md) |
| Extern local functions | No tests yet |  | Declare static local functions implemented outside the compilation. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/local-function-attributes.md) |
| Function pointers | Not yet | 0 of 10, 0% | Declare and invoke typed managed or unmanaged function pointers. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/function-pointers.md) |
| Init-only setters | No tests yet |  | Allow property or indexer assignment during construction using init accessors. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/init.md) |
| Lambda discard parameters | No tests yet |  | Declare multiple ignored parameters named _ in anonymous functions. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/lambda-discard-parameters.md) |
| Logical patterns | No tests yet |  | Combine patterns using and, or, and not. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/patterns3.md) |
| MemberNotNull attributes | No tests yet |  | Declare member initialization postconditions recognized by nullable flow analysis. | [Microsoft Learn](https://learn.microsoft.com/en-us/dotnet/csharp/language-reference/attributes/nullable-analysis#helper-methods-membernotnull-and-membernotnullwhen) |
| Module initializers | No tests yet |  | Run designated static methods before other code in their module. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/module-initializers.md) |
| Native-sized integers | In progress | 288 of 1,479, 19% | Use nint and nuint for integer arithmetic matching the platform pointer size. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/native-integers.md) |
| Parenthesized patterns | No tests yet |  | Group patterns explicitly to control combinator precedence. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/patterns3.md) |
| Record classes | No tests yet |  | Declare reference types with synthesized equality, printing, and optional positional members. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/records.md) |
| Relational patterns | No tests yet |  | Match values against ordered comparisons with constants. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/patterns3.md) |
| SkipLocalsInit attribute | No tests yet |  | Request omission of automatic local-storage initialization through a compiler-recognized attribute. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/skip-localsinit.md) |
| Static anonymous functions | No tests yet |  | Prevent lambdas and anonymous methods from capturing surrounding state. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/static-anonymous-functions.md) |
| Target-typed conditional expressions | Complete | 4 of 4, 100% | Convert conditional branches using a shared target type. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/target-typed-conditional-expression.md) |
| Target-typed new expressions | In progress | 5 of 7, 71% | Omit constructed type names when the target provides the type. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/target-typed-new.md) |
| Top-level statements | No tests yet |  | Write an executable entry point directly at compilation-unit scope. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/top-level-statements.md) |
| Type patterns | No tests yet |  | Match a type without declaring a pattern variable. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/patterns3.md) |
| Unconstrained type parameter annotations | Complete | 1 of 1, 100% | Annotate unconstrained generic parameters with ? to describe nullable values. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/unconstrained-type-parameter-annotations.md) |
| With expressions on records | No tests yet |  | Create modified copies of record values using with initializers. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-9.0/records.md) |

## C# 10.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Attributes on lambdas | No tests yet |  | Attach attributes to lambdas, their parameters, or their return values. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/lambda-improvements.md) |
| CallerArgumentExpression attribute | Not yet | 0 of 2, 0% | Capture a caller's argument source expression as an optional string parameter. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/caller-argument-expression.md) |
| Constant interpolated strings | Complete | 1 of 1, 100% | Use interpolation consisting of constant strings in constant expressions. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/constant_interpolated_strings.md) |
| Enhanced line directives | No tests yet |  | Map generated code spans to source line and character positions. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/enhanced-line-directives.md) |
| Explicit lambda return types | No tests yet |  | Specify a lambda's return type before its parameter list. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/lambda-improvements.md) |
| Explicit record class declarations | No tests yet |  | Spell record class explicitly when declaring a reference record. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/record-structs.md) |
| Extended property patterns | No tests yet |  | Match nested members using dotted property paths. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/extended-property-patterns.md) |
| File-scoped namespaces | No tests yet |  | Declare a namespace covering the rest of a file using a semicolon. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/file-scoped-namespaces.md) |
| Global using directives | No tests yet |  | Apply namespace imports, aliases, or static imports throughout a compilation. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/GlobalUsingDirective.md) |
| Implicit implementation of nonpublic interface members | No tests yet |  | Implement accessible nonpublic interface members without explicit interface qualification. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-8.0/default-interface-methods.md) |
| Interpolated string handlers | In progress | 6 of 9, 66% | Build interpolated values through compiler-recognized handler constructors and append methods. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/improved-interpolated-strings.md) |
| Method-level async builders | No tests yet |  | Override the async builder type for an individual method. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/async-method-builders.md) |
| Mixed deconstruction declarations and assignments | No tests yet |  | Combine newly declared and existing variables in one deconstruction. | [Microsoft Learn](https://learn.microsoft.com/en-us/dotnet/csharp/whats-new/csharp-version-history#c-version-10) |
| Natural delegate types | In progress | 2 of 7, 28% | Infer delegate types for lambdas and unambiguous method groups without explicit targets. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/lambda-improvements.md) |
| Parameterless struct constructors | No tests yet |  | Declare public parameterless instance constructors on structs. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/parameterless-struct-constructors.md) |
| Record positional fields | No tests yet |  | Use existing fields to represent matching positional record parameters. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/record-structs.md) |
| Record structs | No tests yet |  | Declare value types with synthesized equality, printing, and optional positional members. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/record-structs.md) |
| Sealed record ToString | No tests yet |  | Seal a record's ToString override while allowing further record inheritance. | [Microsoft Learn](https://learn.microsoft.com/en-us/dotnet/csharp/whats-new/csharp-version-history#c-version-10) |
| Struct field initializers | No tests yet |  | Initialize struct instance fields or auto-properties in their declarations. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/parameterless-struct-constructors.md) |
| With expressions on anonymous types | No tests yet |  | Create modified anonymous-type values using with initializers. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/record-structs.md) |
| With expressions on structs | No tests yet |  | Create modified copies of ordinary struct values using with initializers. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-10.0/record-structs.md) |

## C# 11.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Auto-default structs | No tests yet |  | Automatically initialize struct fields left unassigned by constructors. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/auto-default-structs.md) |
| Checked user-defined conversions | In progress | 2 of 5, 40% | Declare explicit conversions selected within checked conversion contexts. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/checked-user-defined-operators.md) |
| Checked user-defined operators | In progress | 2 of 5, 40% | Declare operator implementations selected within checked arithmetic contexts. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/checked-user-defined-operators.md) |
| Extended nameof scope | No tests yet |  | Refer to parameters in nameof expressions inside relevant attributes. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/extended-nameof-scope.md) |
| File-local types | No tests yet |  | Restrict top-level types to the source file declaring them. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/file-local-types.md) |
| Generic attributes | No tests yet |  | Declare and apply attribute classes with generic type arguments. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/generic-attributes.md) |
| Interpolated raw string literals | Not yet | 0 of 1, 0% | Interpolate raw strings using dollar counts to control brace delimiters. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/raw-string-literal.md) |
| List patterns | No tests yet |  | Match indexable collections against ordered element patterns. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/list-patterns.md) |
| Newlines in interpolation expressions | No tests yet |  | Write interpolation expressions across lines inside ordinary interpolated strings. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/new-line-in-interpolation.md) |
| Numeric IntPtr and UIntPtr | Not yet | 0 of 1, 0% | Treat native integer keywords as aliases for IntPtr and UIntPtr. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/numeric-intptr.md) |
| Raw string literals | No tests yet |  | Write strings with flexible quote delimiters and controlled multiline indentation. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/raw-string-literal.md) |
| Ref fields | No tests yet |  | Store references in fields of ref structs. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/low-level-struct-improvements.md) |
| Relaxed shift operator requirements | No tests yet |  | Declare shift operators whose right operand has a type other than int. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/relaxing_shift_operator_requirements.md) |
| Required members | No tests yet |  | Require initialization of marked fields or properties during object construction. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/required-members.md) |
| Scoped references and values | In progress | 1 of 3, 33% | Limit parameter or local reference lifetimes explicitly using scoped. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/low-level-struct-improvements.md) |
| Slice patterns | No tests yet |  | Match variable-length portions inside list patterns using two dots. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/list-patterns.md) |
| Span character constant patterns | No tests yet |  | Match Span<char> or ReadOnlySpan<char> values against constant string patterns. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/pattern-match-span-of-char-on-string.md) |
| Static abstract interface members | In progress | 1 of 3, 33% | Declare static contracts callable through constrained type parameters. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/static-abstracts-in-interfaces.md) |
| Static virtual interface members | No tests yet |  | Provide overridable default static implementations in interfaces. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/static-abstracts-in-interfaces.md) |
| UnscopedRef attribute | No tests yet |  | Override default reference escape restrictions on supported members or parameters. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/low-level-struct-improvements.md) |
| Unsigned right shift | No tests yet |  | Shift integer bits right with zero extension using >>> or >>>=. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/unsigned-right-shift-operator.md) |
| UTF-8 string literals | Not yet | 0 of 1, 0% | Produce UTF-8 byte data directly from literals suffixed with u8. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-11.0/utf8-string-literals.md) |

## C# 12.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Alias any type | No tests yet |  | Alias tuple, array, pointer, and other supported types with using directives. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/using-alias-types.md) |
| Collection expression spread elements | No tests yet |  | Insert all elements of another collection using .. inside a collection expression. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/collection-expressions.md) |
| Collection expressions | No tests yet |  | Create arrays, spans, collections, or builder-backed values with bracketed element lists. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/collection-expressions.md) |
| Experimental attribute | No tests yet |  | Report designated diagnostics when consuming APIs marked experimental. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/experimental-attribute.md) |
| Inline arrays | No tests yet |  | Declare fixed-length struct storage with compiler-supported indexing, iteration, and span conversions. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/inline-arrays.md) |
| Nameof instance member access | No tests yet |  | Reference instance members in nameof without constructing an instance receiver. | [proposal](https://github.com/dotnet/csharplang/issues/4037) |
| Optional and params method group inference | No tests yet |  | Infer delegates preserving method group parameter defaults and params behavior. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/lambda-method-group-defaults.md) |
| Optional lambda parameters | No tests yet |  | Declare default parameter values on lambda expressions. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/lambda-method-group-defaults.md) |
| Params arrays in lambdas | No tests yet |  | Declare variable-length array parameters on lambda expressions. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/lambda-method-group-defaults.md) |
| Primary constructors for classes | No tests yet |  | Declare ordinary class constructor parameters directly after the class name. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/primary-constructors.md) |
| Primary constructors for structs | No tests yet |  | Declare ordinary struct constructor parameters directly after the struct name. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/primary-constructors.md) |
| Ref readonly parameters | No tests yet |  | Request readonly references while warning about omitted annotations or temporary arguments. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-12.0/ref-readonly-parameters.md) |

## C# 13.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Allows ref struct anti-constraints | No tests yet |  | Permit stack-confined types as generic arguments through allows ref struct. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/ref-struct-interfaces.md) |
| Escape character escape sequence | No tests yet |  | Represent the ESC character using the string or character escape \e. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/esc-escape-sequence.md) |
| Implicit indexer access in object initializers | No tests yet |  | Use from-end indices with implicit indexers inside object initializers. | [Microsoft Learn](https://learn.microsoft.com/en-us/dotnet/csharp/whats-new/csharp-13#implicit-index-access) |
| Lock object semantics | No tests yet |  | Use System.Threading.Lock's scope-based API when locking a statically typed Lock. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/lock-object.md) |
| Overload resolution priority | No tests yet |  | Prefer overloads according to compiler-recognized priority attributes. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/overload-resolution-priority.md) |
| Params collections | No tests yet |  | Accept expanded arguments through supported collection types beyond arrays. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/params-collections.md) |
| Partial indexers | No tests yet |  | Split indexer declarations and implementations across partial type declarations. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/partial-properties.md) |
| Partial properties | No tests yet |  | Split property declarations and implementations across partial type declarations. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/partial-properties.md) |
| Ref locals in async methods and iterators | No tests yet |  | Use reference locals between suspension points in async methods or iterators. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/ref-unsafe-in-iterators-async.md) |
| Ref struct interfaces | No tests yet |  | Allow ref structs to implement interfaces without boxing conversions. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/ref-struct-interfaces.md) |
| Ref struct locals in async methods and iterators | No tests yet |  | Use stack-confined values between suspension points in async methods or iterators. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/ref-unsafe-in-iterators-async.md) |
| Unsafe contexts in iterators | No tests yet |  | Allow unsafe code in iterator segments outside yield statements. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-13.0/ref-unsafe-in-iterators-async.md) |

## C# 14.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Extension block methods | No tests yet |  | Declare instance extension methods inside shared receiver blocks. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/extensions.md) |
| Extension operators | No tests yet |  | Declare operators extending existing types inside extension blocks. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/extension-operators.md) |
| Extension properties | No tests yet |  | Declare instance properties extending an existing type. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/extensions.md) |
| Field-backed properties | No tests yet |  | Access compiler-synthesized property storage using the contextual keyword field. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/field-keyword.md) |
| First-class span conversions | No tests yet |  | Use built-in implicit array, string, and span conversions in additional language contexts. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/first-class-span-types.md) |
| Ignored directives for file-based apps | No tests yet |  | Permit #: directives that C# ignores while SDK tooling interprets them. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/ignored-directives.md) |
| Named arguments in expression trees | No tests yet |  | Name call arguments when creating expression trees. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/optional-and-named-parameters-in-expression-trees.md) |
| Null-conditional assignment | No tests yet |  | Assign through member or element access only when its receiver is nonnull. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/null-conditional-assignment.md) |
| Optional arguments in expression trees | No tests yet |  | Omit optional call arguments when creating expression trees. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/optional-and-named-parameters-in-expression-trees.md) |
| Partial constructors | No tests yet |  | Split instance constructor declarations and implementations across partial declarations. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/partial-events-and-constructors.md) |
| Partial events | No tests yet |  | Split event declarations and accessor implementations across partial declarations. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/partial-events-and-constructors.md) |
| Simple lambda parameters with modifiers | No tests yet |  | Use lambda parameter modifiers while omitting explicit parameter types. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/simple-lambda-parameters-with-modifiers.md) |
| Static extension methods | No tests yet |  | Declare methods callable as static members of an extended type. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/extensions.md) |
| Static extension properties | No tests yet |  | Declare properties callable as static members of an extended type. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/extensions.md) |
| Unbound generic types in nameof | No tests yet |  | Obtain generic type names without supplying unused type arguments. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/unbound-generic-types-in-nameof.md) |
| User-defined compound assignment operators | No tests yet |  | Define instance operators that modify compound-assignment targets directly. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/user-defined-compound-assignment.md) |
| User-defined instance increment and decrement | No tests yet |  | Define instance increment or decrement operators that modify their receivers directly. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-14.0/user-defined-compound-assignment.md) |

## C# 15.0

| feature | status | suite tests passed | what it is | specification |
|---|---|---|---|---|
| Closed hierarchies | No tests yet |  | Restrict direct derivation to an assembly and recognize exhaustive descendant matches. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/closed-hierarchies.md) |
| Collection expression arguments | No tests yet |  | Configure collection construction or builder calls using an initial with(...) element. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/collection-expression-arguments.md) |
| Custom union types | No tests yet |  | Mark suitable classes or structs with UnionAttribute to enable union behavior. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/unions.md) |
| Extension indexers | No tests yet |  | Declare indexers extending existing types inside extension blocks. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/extension-indexers.md) |
| Labeled break statements | No tests yet |  | Exit a labeled enclosing loop or switch directly. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/labeled-break-continue.md) |
| Labeled continue statements | No tests yet |  | Begin the next iteration of a labeled enclosing loop. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/labeled-break-continue.md) |
| Non-boxing union access pattern | No tests yet |  | Provide HasValue and TryGetValue members for strongly typed union matching. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/unions.md) |
| Union conversions | No tests yet |  | Convert case-type values implicitly into compatible union types. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/unions.md) |
| Union declarations | No tests yet |  | Declare types containing one of several case types using union syntax. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/unions.md) |
| Union exhaustiveness | No tests yet |  | Recognize switches covering every union case type without requiring a fallback. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/unions.md) |
| Union member providers | No tests yet |  | Define union creation and access members through a nested IUnionMembers interface. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/unions.md) |
| Union pattern matching | No tests yet |  | Apply supported patterns to a union's contained value. | [proposal](https://github.com/dotnet/csharplang/blob/0df79080bfbd3d0120ad1904ed0a662fd69be6b9/proposals/csharp-15.0/unions.md) |
