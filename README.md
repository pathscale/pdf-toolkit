# pdf-toolkit

`pdf-toolkit` is a Rust workspace for PDF primitives, text extraction,
inspection, writing, and a graphical interface. The workspace is currently a
scaffold. It does not yet implement PDF parsing, extraction, inspection,
writing, or a GUI.

## Crates

- `pdf-core`: foundational PDF types and primitives. It is `no_std` and uses
  `alloc`; its optional `std` feature is reserved for integrations such as file
  I/O.
- `pdf-text`: text extraction APIs.
- `pdf-inspect`: document inspection APIs.
- `pdf-write`: PDF writing APIs.
- `pdf-gui`: graphical user interface APIs.

Each crate currently contains only a minimal library placeholder. The workspace
uses stable Rust and is licensed under MIT. See `NOTICE` for the current
third-party import record and the attribution format for future imports.
