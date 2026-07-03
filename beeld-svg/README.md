# beeld-svg

[![Crates.io](https://img.shields.io/crates/v/beeld-svg.svg)](https://crates.io/crates/beeld-svg)
[![Documentation](https://docs.rs/beeld-svg/badge.svg)](https://docs.rs/beeld-svg)

<!-- cargo-rdme start -->

A crate for converting PDF pages to SVG files.

This is the pendant to [`beeld`](https://crates.io/crates/beeld), but allows you to export to
SVG instead of bitmap images. See the description of that crate for more information on the
supported features and limitations.

### Safety
This crate forbids unsafe code via a crate-level attribute.

### Cargo features
This crate has one optional feature:
- `embed-fonts`: See the description of [`beeld-interpret`](https://docs.rs/beeld-interpret/latest/beeld_interpret/#cargo-features) for more information.

<!-- cargo-rdme end -->

## License
Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or <http://www.apache.org/licenses/LICENSE-2.0>)
- MIT license ([LICENSE-MIT](LICENSE-MIT) or <http://opensource.org/licenses/MIT>)

at your option.
