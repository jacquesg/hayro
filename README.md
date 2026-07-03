# beeld

> **beeld** is Bloudraad's private, proprietary hard fork of
> [hayro](https://github.com/LaurenzV/hayro) by Laurenz Stampfl, used under its
> MIT OR Apache-2.0 licences. It is not affiliated with or endorsed by the
> upstream project. See [Licence](#licence) for how the fork is licensed.

An experimental, work-in-progress PDF interpreter and renderer.

`beeld` is a Rust crate with a simple task: It allows you to interpret one or many pages of a PDF file to for example convert them into PNG or SVG files. This is a difficult task, as the PDF specification is _huge_ and contains many features. In addition to that, there are millions of PDF files out there with many edge cases, so a solid PDF renderer should be able to handle those as well as possible.

This is not the first attempt at writing a PDF renderer in Rust, but, to the best of my knowledge, this is currently the most feature-complete library. There are still certain features and edge cases that `beeld` currently doesn't support (for example rendering knockout groups or PDFs with non-embedded CID-fonts). However, the vast majority of common features is supported meaning that you should be able to render the "average" PDF file without encountering any issues. This statement is underpinned by the fact that `beeld` is able to handle the 1400+ PDFs in our test suite, which to a large part have been scraped from the `PDFBOX` and `pdf.js` test regression suites.

But, this crate is still in a very development stage, and there are issues that remain to be addressed, most notably performance, which has not been a focus at all so far but will become a priority in the near future.

## Crates
While the main goal of `beeld` is rendering PDF files, the `beeld` project actually encompasses a number of different crates which can in theory used independently. These include:
- [`beeld-syntax`](beeld-syntax): Low-level parsing and reading of PDF files.
- [`beeld-interpret`](beeld-interpret): A PDF interpreter emitting commands into an abstract `Device`.
- [`beeld`](beeld): Rendering PDF pages into bitmaps.
- [`beeld-svg`](beeld-svg): Converting PDF pages into SVG images.
- [`beeld-jpeg2000`](beeld-jpeg2000): A JPEG2000 image decoder.
- [`beeld-jbig2`](beeld-jbig2): A JBIG2 image decoder.
- [`beeld-ccitt`](beeld-ccitt): A decoder for group 3 and group 4 CCITT-encoded images.
- [`beeld-postscript`](beeld-postscript): A lightweight scanner for a specific subset of PostScript.
- [`beeld-cmap`](beeld-cmap): A parser for CMap files in PDFs.

## Minimum Supported Rust Version (MSRV)
The minimum supported Rust version is **1.92**.

# Licence

beeld is a private, proprietary hard fork of
[hayro](https://github.com/LaurenzV/hayro) by Laurenz Stampfl, and is **not**
released under an open-source licence.

- Upstream hayro code remains licensed under **MIT OR Apache-2.0** (see
  `LICENSE_MIT`, `LICENSE_APACHE`, and `NOTICE.md`), with all original copyright
  notices retained.
- All Bloudraad modifications and new code are **proprietary and confidential**
  (see `LICENCE`). All rights reserved. Not for redistribution.
