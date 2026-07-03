# beeld-interpret

[![Crates.io](https://img.shields.io/crates/v/beeld-interpret.svg)](https://crates.io/crates/beeld-interpret)
[![Documentation](https://docs.rs/beeld-interpret/badge.svg)](https://docs.rs/beeld-interpret)

<!-- cargo-rdme start -->

A crate for interpreting PDF files.

This crate provides an abstraction to interpret the content of a PDF file and render them
into an abstract [`Device`], which clients can implement as needed. This allows you, for
example, to render PDF files to bitmaps (which is what the `beeld` crate does), or other formats
such as SVG.

It should be noted that this crate is still very much in development. Therefore it currently
lacks pretty much any documentation on how to use it. It's current API also only really makes it
useful for rendering to PNG or SVG, though this will be improved upon in the future.

## Examples
See the `examples` folder on the GitHub repository. Apart from that, you can also consult
the implementation of `beeld` and `beeld-svg` to get an idea on how to use this crate.

## Safety
This crate forbids unsafe code via a crate-level attribute.

## Cargo features
This crate has one optional feature:
- `embed-fonts`: PDF processors are required to support 14 predefined fonts that do not need to be
  embedded into a PDF file. If you enable this feature, beeld will embed a (permissively-licensed)
  substitute for each font, so that you don't have to implement your custom font loading logic. This
  will add around ~240KB to your binary.

<!-- cargo-rdme end -->

## License
Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option. 
