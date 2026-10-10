# pdf-toolkit

A pure Rust PDF reader, built for agents and for the chuzz browser: fast, bounded and safe by construction.
No C libraries, no threads in the core, no JavaScript or actions ever executed. Layout fidelity targets 80 to
90 percent; speed and clean internals come first.

## Crates

- `pdf-core` (no_std + alloc): zero-copy object model over one shared `Arc<[u8]>`, xref tables, xref streams,
  hybrid files and `/Prev` chains, object streams, stream filters (Flate via miniz_oxide, LZW, ASCII85,
  ASCIIHex, RunLength, PNG and TIFF predictors), a caller-owned `Resolver` with a byte-capped object stream
  cache, and the page tree walk with inheritance. Every limit is a named refusal (`Limits`, `DocError`).
- `pdf-text` (no_std + alloc): fonts (Type1, TrueType, Type3, Type0/CID), encodings and `/Differences`,
  ToUnicode CMaps, a content stream interpreter producing positioned glyphs, layout with columns, reading order
  and headings, and `extract::extract_text` for plain text with a lossy note.
- `pdf-inspect` (binary): `summary`, `streams`, `text`, `receipt`, `bench`, `mutate` (deterministic mutation
  fuzzing) and `check` (fixture manifests).
- `pdf-write`: a minimal serializer and `pdf-fixtures`, which writes the house fixture set with ground-truth
  manifests. It does not depend on the reader.
- `pdf-gui`: placeholder.

## Checking

```sh
cargo build --release
./target/release/pdf-fixtures fixtures/house
./target/release/pdf-inspect check fixtures/house
./target/release/pdf-inspect mutate fixtures/house/hybrid.pdf --seed 1 --count 2000
./target/release/pdf-inspect bench some.pdf
```

Behaviour is proven by fixtures, not unit tests. See `NOTICE` for imported data.
