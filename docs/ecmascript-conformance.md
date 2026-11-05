# ECMAScript language support

This page lists the named features of ECMAScript, by the edition that introduces them, and how
much of each one Lamella's ECMAScript engine supports, measured against Test262.

202 features, of which 64 are complete, 97 are in progress,
40 are not yet supported and 1 has no tests yet.
Across the whole corpus in scope, 50,128 of 82,592 Test262 runs pass (60%).

## How each feature is measured

A feature's status comes from Test262:

- **Complete:** every run that belongs to the feature passes.
- **In progress:** some of them pass.
- **Not yet:** none of them passes.
- **No tests yet:** no run checks the feature.

The Test262 runs passed column gives the count and the share, such as "19 of 20, 95%"; it is
empty for a feature with no tests yet.

Test262 is the [official ECMAScript conformance suite](https://github.com/tc39/test262), maintained
by TC39 and published by Ecma International; its programs must produce the required result or
reject invalid code at the required phase.

The feature list follows [ECMA-262, 17th edition](https://www.ecma-international.org/wp-content/uploads/ECMA-262_17th_edition_june_2026.pdf),
with features grouped under the edition that introduces them; features already defined in edition 5.1 appear there.

Unless its flags specify a single mode, each Test262 file counts as two runs: non-strict code and
strict code, whose additional rules reject some unsafe constructs and turn some silent failures into errors.
The denominator includes every run in scope, including runs the test runner does not attempt;
those runs count as not passed.

Builds for small devices can leave eval out; in a second pass without it, 945 otherwise passing
runs in scope do not pass.

Annex B is outside the totals: 1,555 runs check legacy behavior that ECMA-262 17th edition makes
normative optional for a host that is not a web browser, including legacy clauses marked in the main text.

Internationalization is outside the totals: 6,714 runs under Test262's intl402 and staging/intl402 paths check
ECMA-402, a separate standard.

The results on this page:

- **Suite:** Test262 commit `b363f29d3c43c626dc852744ad64a0b48a003693`, dated 2026-07-31.
- **Engine:** Lamella as of 2026-10-28.
- **Passes:** 2026-10-30 without eval (UTC).
- **Platform:** Microsoft Windows 10.0.26300, x86_64-pc-windows-msvc.
- **Build options:** release build with eval enabled; the second pass uses the default build without eval.

Running on a microcontroller is measured separately.

## Summary

| ECMAScript edition | features | complete | in progress | not yet | no tests yet | Test262 runs passed |
|---|---:|---:|---:|---:|---:|---|
| [ECMAScript 5.1](#ecmascript-51) | 75 | 36 | 39 | 0 | 0 | 25,844 of 26,887, 96% |
| [ECMAScript 2015](#ecmascript-2015) | 52 | 19 | 31 | 2 | 0 | 21,433 of 23,434, 91% |
| [ECMAScript 2016](#ecmascript-2016) | 2 | 1 | 1 | 0 | 0 | 206 of 208, 99% |
| [ECMAScript 2017](#ecmascript-2017) | 6 | 3 | 3 | 0 | 0 | 224 of 1,699, 13% |
| [ECMAScript 2018](#ecmascript-2018) | 9 | 2 | 6 | 1 | 0 | 816 of 9,519, 8% |
| [ECMAScript 2019](#ecmascript-2019) | 8 | 1 | 3 | 4 | 0 | 170 of 349, 48% |
| [ECMAScript 2020](#ecmascript-2020) | 9 | 0 | 4 | 5 | 0 | 200 of 3,962, 5% |
| [ECMAScript 2021](#ecmascript-2021) | 5 | 0 | 4 | 1 | 0 | 477 of 924, 51% |
| [ECMAScript 2022](#ecmascript-2022) | 8 | 1 | 3 | 4 | 0 | 224 of 10,447, 2% |
| [ECMAScript 2023](#ecmascript-2023) | 4 | 0 | 0 | 4 | 0 | 0 of 537, 0% |
| [ECMAScript 2024](#ecmascript-2024) | 7 | 0 | 1 | 6 | 0 | 6 of 1,683, 0% |
| [ECMAScript 2025](#ecmascript-2025) | 10 | 1 | 2 | 6 | 1 | 528 of 2,323, 22% |
| [ECMAScript 2026](#ecmascript-2026) | 7 | 0 | 0 | 7 | 0 | 0 of 620, 0% |

Each run counts once, under the newest edition among the features it needs, so the summary rows
add up to the whole-corpus total.

## ECMAScript 5.1

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Addition and subtraction | In progress | 142 of 144, 98% | Add numbers, concatenate strings, or subtract numbers. | ECMA-262 17th edition, clause 13.8 |
| Arguments objects | In progress | 188 of 193, 97% | Access a call's supplied arguments through its arguments object. | ECMA-262 17th edition, clause 10.4.4 |
| Array literals | Complete | 22 of 22, 100% | Create arrays with element lists and elisions. | ECMA-262 17th edition, clause 13.2.4 |
| Array objects and methods | In progress | 4,771 of 4,787, 99% | Create indexed collections and search, iterate, or change their elements. | ECMA-262 17th edition, clause 23.1 |
| Assignment | In progress | 788 of 796, 98% | Store a value in a binding or property. | ECMA-262 17th edition, clause 13.15 |
| Automatic semicolon insertion | Complete | 204 of 204, 100% | Terminate statements where the grammar permits an omitted semicolon. | ECMA-262 17th edition, clause 12.10 |
| Binary bitwise operators | Complete | 135 of 135, 100% | Combine integer bits with AND, OR, and XOR. | ECMA-262 17th edition, clause 13.12 |
| Binding and identifier resolution | Complete | 18 of 18, 100% | Resolve names through nested environments and binding rules. | ECMA-262 17th edition, clauses 8.1, 9.1, 13.1 |
| Bitwise complement | Complete | 28 of 28, 100% | Invert the bits of an integer conversion. | ECMA-262 17th edition, clause 13.5 |
| Bitwise shift operators | Complete | 209 of 209, 100% | Shift an integer's bits left or right. | ECMA-262 17th edition, clause 13.9 |
| Blocks and statement lists | In progress | 110 of 112, 98% | Group statements for sequential execution. | ECMA-262 17th edition, clauses 14.1, 14.2 |
| Boolean objects | In progress | 93 of 95, 97% | Construct and convert Boolean wrapper objects. | ECMA-262 17th edition, clause 20.3 |
| Break statement | Complete | 38 of 38, 100% | Exit an enclosing loop, switch, or labelled statement. | ECMA-262 17th edition, clause 14.9 |
| Comma operator | Complete | 10 of 10, 100% | Evaluate expressions in order and return the last value. | ECMA-262 17th edition, clause 13.16 |
| Comments | Complete | 40 of 40, 100% | Annotate programs with single-line and multiline comments. | ECMA-262 17th edition, clause 12.4 |
| Compound assignment | In progress | 679 of 690, 98% | Combine a numeric or bitwise operation with assignment. | ECMA-262 17th edition, clause 13.15 |
| Conditional operator | Complete | 38 of 38, 100% | Choose between expressions according to a condition. | ECMA-262 17th edition, clause 13.14 |
| Continue statement | Complete | 46 of 46, 100% | Start the next iteration of an enclosing loop. | ECMA-262 17th edition, clause 14.8 |
| Date objects | In progress | 1,001 of 1,008, 99% | Represent timestamps and read, change, parse, or format dates. | ECMA-262 17th edition, clause 21.4 |
| Debugger statement | Complete | 4 of 4, 100% | Request a debugging action from the host. | ECMA-262 17th edition, clause 14.16 |
| Delete operator | In progress | 103 of 107, 96% | Remove a configurable property from an object. | ECMA-262 17th edition, clause 13.5.1 |
| Do-while statement | In progress | 51 of 63, 80% | Repeat a statement and test its condition afterward. | ECMA-262 17th edition, clause 14.7.2 |
| Empty statement | Complete | 4 of 4, 100% | Use a semicolon as a statement that performs no action. | ECMA-262 17th edition, clause 14.4 |
| Equality operators | Complete | 206 of 206, 100% | Compare values with or without type conversion. | ECMA-262 17th edition, clause 13.11 |
| Error objects | In progress | 244 of 246, 99% | Create, throw, and format standard error objects. | ECMA-262 17th edition, clause 20.5 |
| Eval | In progress | 170 of 332, 51% | Compile and evaluate source text at run time. | ECMA-262 17th edition, clause 19.2.1 |
| Expression statements | Complete | 6 of 6, 100% | Evaluate an expression as a statement. | ECMA-262 17th edition, clause 14.5 |
| For statement | In progress | 168 of 174, 96% | Repeat a statement with initialization, a condition, and an update. | ECMA-262 17th edition, clause 14.7.4 |
| For-in statement | In progress | 121 of 141, 85% | Enumerate an object's enumerable property names. | ECMA-262 17th edition, clause 14.7.5 |
| Function calls | In progress | 127 of 130, 97% | Invoke a callable value with arguments and a receiver. | ECMA-262 17th edition, clause 13.3 |
| Function declarations and expressions | In progress | 668 of 728, 91% | Declare callable functions with parameters and local bindings. | ECMA-262 17th edition, clauses 15.1, 15.2 |
| Function objects and Function constructor | In progress | 467 of 767, 60% | Construct functions and use call, apply, and bind. | ECMA-262 17th edition, clause 20.2 |
| Global numeric functions | Complete | 208 of 208, 100% | Parse numbers and test numeric finiteness or NaN. | ECMA-262 17th edition, clauses 19.2.2, 19.2.3, 19.2.4, 19.2.5 |
| Global values | Complete | 35 of 35, 100% | Use undefined, NaN, and Infinity. | ECMA-262 17th edition, clause 19.1 |
| Grouping operator | Complete | 16 of 16, 100% | Group an expression with parentheses. | ECMA-262 17th edition, clause 13.2 |
| Identifiers and reserved words | In progress | 626 of 660, 94% | Name bindings and distinguish keywords from identifiers. | ECMA-262 17th edition, clause 12.7 |
| If statement | In progress | 101 of 103, 98% | Execute a statement according to a condition. | ECMA-262 17th edition, clause 14.6 |
| In operator | Complete | 33 of 33, 100% | Test whether a property exists on an object or its prototype chain. | ECMA-262 17th edition, clause 13.10 |
| Increment and decrement | Complete | 214 of 214, 100% | Increase or decrease a numeric value before or after reading it. | ECMA-262 17th edition, clause 13.4 |
| Instanceof operator | In progress | 49 of 77, 63% | Test an object's relationship to a constructor's prototype. | ECMA-262 17th edition, clause 13.10 |
| JSON parsing and serialization | Complete | 298 of 298, 100% | Convert between ECMAScript values and JSON text. | ECMA-262 17th edition, clause 25.5 |
| Labelled statements | In progress | 26 of 32, 81% | Name a statement as a break or continue target. | ECMA-262 17th edition, clause 14.13 |
| Language types and conversions | In progress | 205 of 206, 99% | Represent primitive values and objects and convert between types. | ECMA-262 17th edition, clauses 6.1, 7.1, 7.2 |
| Logical AND and OR | Complete | 62 of 62, 100% | Choose values with short-circuit evaluation. | ECMA-262 17th edition, clause 13.13 |
| Logical negation | In progress | 32 of 34, 94% | Invert the Boolean interpretation of a value. | ECMA-262 17th edition, clause 13.5 |
| Math constants and functions | In progress | 424 of 426, 99% | Calculate standard numeric functions and access mathematical constants. | ECMA-262 17th edition, clause 21.3 |
| Multiplication, division, and remainder | Complete | 207 of 207, 100% | Combine numeric values with multiplicative operators. | ECMA-262 17th edition, clause 13.7 |
| New operator | In progress | 42 of 44, 95% | Construct an object from a constructor and arguments. | ECMA-262 17th edition, clause 13.3 |
| Null and Boolean literals | Complete | 14 of 14, 100% | Write null, true, and false values. | ECMA-262 17th edition, clauses 12.9.1, 12.9.2 |
| Number literals | Complete | 155 of 155, 100% | Write decimal and hexadecimal numbers. | ECMA-262 17th edition, clause 12.9.3 |
| Number objects | Complete | 554 of 554, 100% | Construct numeric wrappers and format numbers. | ECMA-262 17th edition, clause 21.1 |
| Object literals | In progress | 434 of 436, 99% | Create objects with data properties and accessors. | ECMA-262 17th edition, clause 13.2.5 |
| Object objects and property descriptors | In progress | 6,120 of 6,152, 99% | Create objects and inspect, define, or restrict their properties. | ECMA-262 17th edition, clause 20.1 |
| Property access | Complete | 48 of 48, 100% | Read or write an object's named or indexed properties. | ECMA-262 17th edition, clause 13.3 |
| Punctuators | Complete | 22 of 22, 100% | Use punctuation to delimit expressions and statements. | ECMA-262 17th edition, clause 12.8 |
| Regular expressions | In progress | 1,364 of 1,462, 93% | Match strings with regular expression patterns. | ECMA-262 17th edition, clauses 12.9.5, 22.2 |
| Relational comparison | Complete | 310 of 310, 100% | Compare values using ordered comparisons. | ECMA-262 17th edition, clause 13.10 |
| Return statement | Complete | 30 of 30, 100% | Return a value from a function call. | ECMA-262 17th edition, clause 14.10 |
| Scripts and global declarations | In progress | 122 of 134, 91% | Evaluate scripts and initialize global bindings. | ECMA-262 17th edition, clause 16.1 |
| Source text | Complete | 2 of 2, 100% | Write source as Unicode text. | ECMA-262 17th edition, clause 11 |
| Strict mode | In progress | 107 of 183, 58% | Apply the strict variant's binding, assignment, and syntax rules. | ECMA-262 17th edition, clauses 11.2.2, 16.1 |
| String literals | Complete | 109 of 109, 100% | Write quoted sequences of characters and escape sequences. | ECMA-262 17th edition, clause 12.9.4 |
| String objects and methods | In progress | 1,669 of 1,707, 97% | Construct strings and search, slice, transform, or format their contents. | ECMA-262 17th edition, clause 22.1 |
| Switch statement | In progress | 89 of 127, 70% | Select a statement sequence by comparing case values. | ECMA-262 17th edition, clause 14.12 |
| This expression | In progress | 9 of 15, 60% | Refer to the current call's receiver. | ECMA-262 17th edition, clause 13.2 |
| Throw statement | Complete | 28 of 28, 100% | Raise a value as an exception. | ECMA-262 17th edition, clause 14.14 |
| Try, catch, and finally | In progress | 181 of 187, 96% | Handle exceptions and execute cleanup statements. | ECMA-262 17th edition, clause 14.15 |
| Typeof operator | In progress | 28 of 30, 93% | Obtain a value's type name. | ECMA-262 17th edition, clause 13.5.3 |
| Unary plus and minus | Complete | 56 of 56, 100% | Convert to a number or negate a numeric value. | ECMA-262 17th edition, clause 13.5 |
| URI handling functions | Complete | 338 of 338, 100% | Encode and decode URIs and URI components. | ECMA-262 17th edition, clause 19.2.6 |
| Variable declarations | In progress | 96 of 109, 88% | Declare function-scoped or global variables with var. | ECMA-262 17th edition, clause 14.3.2 |
| Void operator | Complete | 18 of 18, 100% | Evaluate an expression and produce undefined. | ECMA-262 17th edition, clause 13.5.2 |
| While statement | In progress | 55 of 65, 84% | Repeat a statement while its condition holds. | ECMA-262 17th edition, clause 14.7.3 |
| White space and line terminators | In progress | 254 of 260, 97% | Separate tokens with white space and line endings. | ECMA-262 17th edition, clauses 12.1, 12.2, 12.3 |
| With statement | In progress | 155 of 160, 96% | Add an object to name lookup in non-strict code. | ECMA-262 17th edition, clause 14.11 |

## ECMAScript 2015

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Additional Math functions | Complete | 186 of 186, 100% | Calculate hyperbolic functions, logarithms, integer multiplication, and rounding. | ECMA-262 17th edition, clause 21.3.2 |
| Array fill and copyWithin | Complete | 96 of 96, 100% | Fill array elements or copy a range within the same array. | ECMA-262 17th edition, clause 23.1.3 |
| Array find and findIndex | Complete | 56 of 56, 100% | Find the first element or index that satisfies a predicate. | ECMA-262 17th edition, clause 23.1.3 |
| Array iterator methods | Complete | 43 of 43, 100% | Iterate an array's keys, values, and entries. | ECMA-262 17th edition, clause 23.1.3 |
| Array.from and Array.of | In progress | 95 of 99, 95% | Create arrays from iterables, array-like objects, or arguments. | ECMA-262 17th edition, clause 23.1.2 |
| ArrayBuffer objects | In progress | 98 of 295, 33% | Store binary data in detachable byte buffers. | ECMA-262 17th edition, clause 25.1 |
| Arrow functions | In progress | 917 of 981, 93% | Create functions that capture the enclosing this value. | ECMA-262 17th edition, clause 15.3 |
| Binary and octal number literals | Complete | 24 of 24, 100% | Write integers using binary and octal prefixes. | ECMA-262 17th edition, clause 12.9.3 |
| Classes and inheritance | In progress | 1,287 of 1,389, 92% | Declare constructors, methods, and an inheritance relationship. | ECMA-262 17th edition, clause 15.7 |
| Computed property names | In progress | 346 of 357, 96% | Choose a property name by evaluating an expression. | ECMA-262 17th edition, clauses 13.2.5, 15.4 |
| Const declarations | In progress | 99 of 106, 93% | Declare block-scoped bindings that cannot be reassigned. | ECMA-262 17th edition, clause 14.3.1 |
| DataView objects | In progress | 404 of 640, 63% | Read and write numeric values in a byte buffer. | ECMA-262 17th edition, clause 25.3 |
| Default parameters | In progress | 3,084 of 3,091, 99% | Supply a default value for a missing or undefined argument. | ECMA-262 17th edition, clause 15.1 |
| Destructuring assignment | Complete | 225 of 225, 100% | Assign bindings or properties by unpacking arrays and objects. | ECMA-262 17th edition, clause 13.15.5 |
| Destructuring bindings | In progress | 8,193 of 8,259, 99% | Bind names by unpacking arrays and objects. | ECMA-262 17th edition, clause 14.3.3 |
| For-of statement | In progress | 153 of 165, 92% | Iterate over the values supplied by an iterable. | ECMA-262 17th edition, clause 14.7.5 |
| Function names | Complete | 10 of 10, 100% | Expose declared and inferred function names through the name property. | ECMA-262 17th edition, clause 20.2.4.2 |
| Generator functions and objects | In progress | 4,682 of 4,827, 96% | Suspend and resume a function using yield and yield*. | ECMA-262 17th edition, clauses 15.5, 27.3, 27.5 |
| Iteration protocols and iterators | In progress | 1,524 of 1,584, 96% | Produce values through iterator and iterable protocols. | ECMA-262 17th edition, clauses 7.4, 27.1 |
| Let declarations | In progress | 384 of 415, 92% | Declare mutable block-scoped bindings. | ECMA-262 17th edition, clause 14.3.1 |
| Map objects | Complete | 240 of 240, 100% | Store and iterate key-value pairs with arbitrary keys. | ECMA-262 17th edition, clause 24.1 |
| Method definitions | In progress | 72 of 76, 94% | Define concise methods and accessors in objects. | ECMA-262 17th edition, clause 15.4 |
| Modules, imports, and exports | Not yet | 0 of 277, 0% | Organize bindings into modules and link imports and exports. | ECMA-262 17th edition, clause 16.2 |
| New.target | In progress | 70 of 73, 95% | Read the constructor that a construction call targets. | ECMA-262 17th edition, clause 13.3 |
| Number predicates and integer constants | Complete | 52 of 52, 100% | Test numeric values and access safe integer limits. | ECMA-262 17th edition, clause 21.1.2 |
| Object literal prototype initialization | In progress | 24 of 26, 92% | Set an object's prototype when creating an object literal. | ECMA-262 17th edition, clause 13.2.5 |
| Object property shorthand | Complete | 14 of 14, 100% | Create an object property from a binding with the same name. | ECMA-262 17th edition, clause 13.2.5 |
| Object.assign | Complete | 50 of 50, 100% | Copy enumerable own properties between objects. | ECMA-262 17th edition, clause 20.1.2 |
| Object.is | Complete | 38 of 38, 100% | Compare values using the SameValue relation. | ECMA-262 17th edition, clause 20.1.2 |
| Object.setPrototypeOf | Complete | 14 of 14, 100% | Change an object's prototype. | ECMA-262 17th edition, clause 20.1.2 |
| Promises | In progress | 569 of 617, 92% | Represent eventual completion and compose reactions. | ECMA-262 17th edition, clause 27.2 |
| Proper tail calls | In progress | 2 of 35, 5% | Evaluate calls in tail position without retaining the calling frame. | ECMA-262 17th edition, clause 15.10 |
| Proxy objects | In progress | 779 of 886, 87% | Intercept object operations through handler traps. | ECMA-262 17th edition, clauses 10.5, 28.2 |
| Realms | Not yet | 0 of 263, 0% | Separate global environments and intrinsic objects. | ECMA-262 17th edition, clause 9.3 |
| Reflect methods | In progress | 1,172 of 1,450, 80% | Perform object operations through functions. | ECMA-262 17th edition, clause 28.1 |
| Rest parameters | Complete | 142 of 142, 100% | Collect remaining arguments in an array. | ECMA-262 17th edition, clause 15.1 |
| Set objects | In progress | 374 of 388, 96% | Store and iterate unique values. | ECMA-262 17th edition, clause 24.2 |
| Spread in arrays and calls | Complete | 120 of 120, 100% | Expand an iterable into array elements or call arguments. | ECMA-262 17th edition, clauses 13.2.4, 13.3 |
| Sticky regular expressions | Complete | 12 of 12, 100% | Require a match at the current lastIndex position using the y flag. | ECMA-262 17th edition, clause 22.2.6 |
| String code point methods | In progress | 46 of 70, 65% | Create and inspect strings using Unicode code points. | ECMA-262 17th edition, clauses 22.1.2, 22.1.3 |
| String includes, startsWith, and endsWith | In progress | 82 of 138, 59% | Test whether a string contains, starts with, or ends with another string. | ECMA-262 17th edition, clause 22.1.3 |
| String.prototype.normalize | In progress | 16 of 22, 72% | Normalize a string to a specified Unicode normalization form. | ECMA-262 17th edition, clause 22.1.3 |
| String.prototype.repeat | Complete | 26 of 26, 100% | Repeat a string a requested number of times. | ECMA-262 17th edition, clause 22.1.3 |
| String.raw | Complete | 52 of 52, 100% | Build a string from a template's raw text and substitutions. | ECMA-262 17th edition, clause 22.1.2 |
| Super expressions | In progress | 96 of 102, 94% | Access an inherited method or call a base constructor. | ECMA-262 17th edition, clauses 13.3, 15.7 |
| Symbol values | In progress | 2,045 of 2,384, 85% | Create unique property keys and use well-known symbol hooks. | ECMA-262 17th edition, clauses 6.1.5, 20.4 |
| Template literals | In progress | 147 of 153, 96% | Interpolate expressions into strings and pass templates to tags. | ECMA-262 17th edition, clauses 12.9.6, 13.2.8 |
| Typed arrays | In progress | 2,130 of 2,472, 86% | Store and manipulate fixed-width numeric elements over a byte buffer. | ECMA-262 17th edition, clause 23.2 |
| Unicode code point escapes | Complete | 4 of 4, 100% | Write Unicode code points in string and identifier escapes. | ECMA-262 17th edition, clauses 12.7, 12.9.4 |
| Unicode regular expressions | In progress | 68 of 70, 97% | Match code points using the u flag. | ECMA-262 17th edition, clauses 22.2.1, 22.2.2 |
| WeakMap objects | In progress | 108 of 182, 59% | Store values under weakly held object keys. | ECMA-262 17th edition, clause 24.3 |
| WeakSet objects | In progress | 110 of 158, 69% | Track weakly held object values. | ECMA-262 17th edition, clause 24.4 |

## ECMAScript 2016

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Array.prototype.includes | Complete | 62 of 62, 100% | Test whether an array contains a value. | ECMA-262 17th edition, clause 23.1.3 |
| Exponentiation operator | In progress | 144 of 146, 98% | Raise a numeric value to a power. | ECMA-262 17th edition, clause 13.6 |

## ECMAScript 2017

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Async functions and await | In progress | 102 of 879, 11% | Suspend a function until a promise settles. | ECMA-262 17th edition, clauses 15.8, 15.9, 27.7 |
| Object.getOwnPropertyDescriptors | Complete | 22 of 22, 100% | Read all of an object's own property descriptors. | ECMA-262 17th edition, clause 20.1.2 |
| Object.values and Object.entries | In progress | 56 of 60, 93% | Read an object's enumerable own values or key-value pairs. | ECMA-262 17th edition, clause 20.1.2 |
| Shared memory and Atomics | In progress | 2 of 696, 0% | Share byte buffers between agents and coordinate atomic operations. | ECMA-262 17th edition, clauses 25.2, 25.4, 29 |
| String padding | Complete | 40 of 40, 100% | Pad a string to a specified length at either end. | ECMA-262 17th edition, clause 22.1.3 |
| Trailing commas in calls and parameters | Complete | 2 of 2, 100% | End function parameter and argument lists with a comma. | ECMA-262 17th edition, clauses 15.1, 13.3 |

## ECMAScript 2018

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Async iteration and generators | In progress | 4 of 7,515, 0% | Iterate asynchronous values and suspend asynchronous generators. | ECMA-262 17th edition, clauses 14.7.5, 15.6, 27.4, 27.6 |
| Named capture groups | In progress | 178 of 182, 97% | Name regular expression captures and refer to them by name. | ECMA-262 17th edition, clauses 22.2.1, 22.2.2 |
| Object rest properties | In progress | 344 of 562, 61% | Collect remaining own properties in a destructuring pattern. | ECMA-262 17th edition, clauses 13.15.5, 14.3.3 |
| Object spread properties | In progress | 180 of 214, 84% | Copy enumerable own properties into an object literal. | ECMA-262 17th edition, clause 13.2.5 |
| Promise.prototype.finally | In progress | 56 of 58, 96% | Run cleanup after a promise settles. | ECMA-262 17th edition, clause 27.2.5 |
| RegExp dotAll | In progress | 18 of 22, 81% | Allow the dot operator to match line terminators. | ECMA-262 17th edition, clauses 22.2.1, 22.2.2 |
| RegExp lookbehind | Complete | 36 of 36, 100% | Assert that a pattern matches before the current position. | ECMA-262 17th edition, clauses 22.2.1, 22.2.2 |
| Tagged template escape sequences | Complete | 2 of 2, 100% | Pass invalid escape sequences as raw text to template tags. | ECMA-262 17th edition, clause 12.9.6 |
| Unicode property escapes | Not yet | 0 of 1,170, 0% | Match Unicode character properties with regular expressions. | ECMA-262 17th edition, clauses 22.2.1, 22.2.2 |

## ECMAScript 2019

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Array flat and flatMap | In progress | 34 of 75, 45% | Flatten nested arrays or map and flatten a result. | ECMA-262 17th edition, clause 23.1.3 |
| Function source text representation | In progress | 84 of 96, 87% | Return a function's source text without reformatting it. | ECMA-262 17th edition, clause 20.2.3 |
| JSON-compatible string literals | Not yet | 0 of 8, 0% | Allow line and paragraph separators in string literals. | ECMA-262 17th edition, clause 12.9.4 |
| Object.fromEntries | Complete | 50 of 50, 100% | Build an object from an iterable of key-value pairs. | ECMA-262 17th edition, clause 20.1.2 |
| Optional catch binding | In progress | 2 of 12, 16% | Catch an exception without declaring a binding. | ECMA-262 17th edition, clause 14.15 |
| String trimStart and trimEnd | Not yet | 0 of 90, 0% | Remove white space from one end of a string. | ECMA-262 17th edition, clause 22.1.3 |
| Symbol descriptions | Not yet | 0 of 16, 0% | Read the description supplied when a symbol is created. | ECMA-262 17th edition, clause 20.4.3 |
| Well-formed JSON.stringify | Not yet | 0 of 2, 0% | Escape lone surrogates in serialized JSON text. | ECMA-262 17th edition, clause 25.5.4 |

## ECMAScript 2020

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| BigInt values and typed arrays | Not yet | 0 of 2,241, 0% | Represent arbitrary-precision integers and fixed-width BigInt array elements. | ECMA-262 17th edition, clauses 6.1.6, 21.2, 23.2 |
| Dynamic import | Not yet | 0 of 1,044, 0% | Load a module asynchronously from an expression. | ECMA-262 17th edition, clause 13.3 |
| For-in enumeration order | Not yet | 0 of 18, 0% | Enumerate object property names in the specified order. | ECMA-262 17th edition, clause 14.7.5 |
| globalThis | In progress | 4 of 124, 3% | Access the global this value using a standard name. | ECMA-262 17th edition, clause 19.1.1 |
| import.meta | Not yet | 0 of 28, 0% | Read host-provided metadata for the current module. | ECMA-262 17th edition, clause 13.3 |
| Nullish coalescing | Not yet | 0 of 48, 0% | Choose a fallback only for null or undefined. | ECMA-262 17th edition, clause 13.13 |
| Optional chaining | In progress | 100 of 114, 87% | Stop a property access or call when a value is null or undefined. | ECMA-262 17th edition, clause 13.3 |
| Promise.allSettled | In progress | 2 of 208, 0% | Wait for every input promise to settle. | ECMA-262 17th edition, clause 27.2.4 |
| String matchAll | In progress | 94 of 138, 68% | Iterate all regular expression matches in a string. | ECMA-262 17th edition, clauses 22.1.3, 22.2.6 |

## ECMAScript 2021

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Logical assignment | In progress | 121 of 128, 94% | Assign a value conditionally using logical or nullish assignment. | ECMA-262 17th edition, clause 13.15 |
| Numeric separators | In progress | 226 of 318, 71% | Separate digits in numeric literals with underscores. | ECMA-262 17th edition, clause 12.9.3 |
| Promise.any and AggregateError | In progress | 52 of 242, 21% | Take the first fulfilled promise or collect rejection errors. | ECMA-262 17th edition, clauses 27.2.4, 20.5.7 |
| String.prototype.replaceAll | In progress | 78 of 80, 97% | Replace every match of a string or global regular expression. | ECMA-262 17th edition, clause 22.1.3 |
| WeakRef and FinalizationRegistry | Not yet | 0 of 156, 0% | Observe weakly held targets and register finalization callbacks. | ECMA-262 17th edition, clause 26 |

## ECMAScript 2022

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Class fields and private members | In progress | 4 of 9,813, 0% | Declare public and private fields and private methods on instances and classes. | ECMA-262 17th edition, clause 15.7 |
| Class static blocks | Not yet | 0 of 126, 0% | Run initialization statements while defining a class. | ECMA-262 17th edition, clause 15.7 |
| Error cause | Not yet | 0 of 10, 0% | Attach the underlying cause when constructing an error. | ECMA-262 17th edition, clause 20.5 |
| Object.hasOwn | Complete | 124 of 124, 100% | Test whether an object has an own property. | ECMA-262 17th edition, clause 20.1.2 |
| Private field brand checks | Not yet | 0 of 38, 0% | Test whether a private name is present on an object. | ECMA-262 17th edition, clause 13.10 |
| RegExp match indices | In progress | 50 of 54, 92% | Report the start and end offsets of captures using the d flag. | ECMA-262 17th edition, clause 22.2.6 |
| Relative indexing with at | In progress | 46 of 70, 65% | Read array, typed array, or string elements using an index from either end. | ECMA-262 17th edition, clauses 22.1.3, 23.1.3, 23.2.3 |
| Top-level await | Not yet | 0 of 260, 0% | Suspend evaluation of a module while an asynchronous operation settles. | ECMA-262 17th edition, clause 16.2 |

## ECMAScript 2023

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Change array by copy | Not yet | 0 of 262, 0% | Sort, reverse, splice, or replace elements while producing a new array. | ECMA-262 17th edition, clauses 23.1.3, 23.2.3 |
| Find from the end | Not yet | 0 of 198, 0% | Find the last matching element or index in an array or typed array. | ECMA-262 17th edition, clauses 23.1.3, 23.2.3 |
| Hashbang comments | Not yet | 0 of 35, 0% | Start a script with a host command line comment. | ECMA-262 17th edition, clause 12.5 |
| Symbols as weak collection keys | Not yet | 0 of 42, 0% | Use non-registered symbols in weak collections and weak references. | ECMA-262 17th edition, clauses 24.3, 24.4, 26 |

## ECMAScript 2024

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| ArrayBuffer transfer | Not yet | 0 of 114, 0% | Move buffer contents to a new buffer and detach the original. | ECMA-262 17th edition, clause 25.1.6 |
| Atomics.waitAsync | Not yet | 0 of 202, 0% | Wait asynchronously for a shared-memory notification. | ECMA-262 17th edition, clause 25.4.14 |
| Object and Map grouping | In progress | 6 of 62, 9% | Group iterable values by keys computed by a callback. | ECMA-262 17th edition, clauses 20.1.2, 24.1.2 |
| Promise.withResolvers | Not yet | 0 of 15, 0% | Create a promise together with its resolve and reject functions. | ECMA-262 17th edition, clause 27.2.4 |
| Resizable and growable buffers | Not yet | 0 of 920, 0% | Change the capacity of an ArrayBuffer or grow a SharedArrayBuffer. | ECMA-262 17th edition, clauses 25.1, 25.2 |
| Unicode sets in regular expressions | Not yet | 0 of 374, 0% | Use the v flag for set operations and Unicode string properties. | ECMA-262 17th edition, clauses 22.2.1, 22.2.2 |
| Well-formed Unicode strings | Not yet | 0 of 32, 0% | Check for or replace lone surrogates in a string. | ECMA-262 17th edition, clause 22.1.3 |

## ECMAScript 2025

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Duplicate named capture groups | Complete | 36 of 36, 100% | Reuse a capture name across disjoint alternatives. | ECMA-262 17th edition, clauses 22.2.1, 22.2.2 |
| Float16 values | Not yet | 0 of 102, 0% | Store half-precision floating-point values and round to that format. | ECMA-262 17th edition, clauses 21.3.2, 23.2, 25.3.4 |
| Global eval var redeclarations | No tests yet |  | Permit repeated global var declarations introduced by eval. | ECMA-262 17th edition, clause 19.2.1 |
| Import attributes | Not yet | 0 of 147, 0% | Specify host interpretation attributes on a module import. | ECMA-262 17th edition, clause 16.2 |
| Iterator helpers | Not yet | 0 of 1,126, 0% | Create, transform, and consume iterators using standard helper methods. | ECMA-262 17th edition, clauses 27.1.2, 27.1.3 |
| JSON modules | Not yet | 0 of 14, 0% | Import JSON values through the module system. | ECMA-262 17th edition, clause 16.2 |
| Promise.try | Not yet | 0 of 24, 0% | Call a function and turn its return or exception into a promise. | ECMA-262 17th edition, clause 27.2.4 |
| RegExp.escape | In progress | 40 of 42, 95% | Escape text for use as a literal part of a regular expression. | ECMA-262 17th edition, clause 22.2.5.1 |
| Scoped regular expression modifiers | In progress | 452 of 460, 98% | Enable or disable regular expression flags for a subpattern. | ECMA-262 17th edition, clauses 22.2.1, 22.2.2 |
| Set composition and comparison | Not yet | 0 of 386, 0% | Combine sets and test subset, superset, or disjoint relationships. | ECMA-262 17th edition, clause 24.2.4 |

## ECMAScript 2026

| feature | status | Test262 runs passed | what it is | specification |
|---|---|---|---|---|
| Array.fromAsync | Not yet | 0 of 186, 0% | Create an array by consuming an asynchronous iterable. | ECMA-262 17th edition, clause 23.1.2 |
| Error.isError | Not yet | 0 of 26, 0% | Test whether a value has the internal Error identity. | ECMA-262 17th edition, clause 20.5.2 |
| Iterator sequencing | Not yet | 0 of 64, 0% | Concatenate iterable sequences into a single iterator. | ECMA-262 17th edition, clause 27.1.3 |
| JSON source text access | Not yet | 0 of 44, 0% | Inspect primitive source text while parsing and embed raw JSON text. | ECMA-262 17th edition, clause 25.5 |
| Map and WeakMap get-or-insert | Not yet | 0 of 142, 0% | Retrieve a key's value or insert a default value or callback result. | ECMA-262 17th edition, clauses 24.1.3, 24.3.3 |
| Math.sumPrecise | Not yet | 0 of 20, 0% | Sum an iterable of numbers with accurate intermediate arithmetic. | ECMA-262 17th edition, clause 21.3.2 |
| Uint8Array Base64 and hex conversion | Not yet | 0 of 138, 0% | Convert byte arrays to and from Base64 and hexadecimal strings. | ECMA-262 17th edition, clause 23.3 |
