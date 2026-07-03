# beeld-cmap

[![Crates.io](https://img.shields.io/crates/v/beeld-cmap.svg)](https://crates.io/crates/beeld-cmap)
[![Documentation](https://docs.rs/beeld-cmap/badge.svg)](https://docs.rs/beeld-cmap)

<!-- cargo-rdme start -->

A parser for cmap files, as they are found in PDFs.

This crate provides a parser for CMap files and allows you to
- Map character codes from text-showing operators to CID identifiers.
- Map CIDs to Unicode characters or strings.

### Safety
This crate forbids unsafe code via a crate-level attribute.

<!-- cargo-rdme end -->

## License
Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
