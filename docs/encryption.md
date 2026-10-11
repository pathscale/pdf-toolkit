# Private PDF encryption

`Writer::new_encrypted(version, password, costs)` writes PDFs using the private
`PSChaCha` security handler. Other PDF readers will refuse these files.
`Writer::new` and the existing document builder APIs retain their existing behavior.
Standard security-handler reading is unchanged.

The password is an exact byte sequence, without normalization or truncation.
An explicitly supplied empty password is supported and provides obfuscation only.
Use `None` for default costs, or `Some(KdfParams { memory_kib, iterations,
parallelism })`. Defaults are M=65536 KiB, T=3, P=1.

## Version 1 format

The trailer points to an indirect security dictionary:

```text
/Filter /PSChaCha
/V 1
/Length 256
/Salt <16 random bytes>
/KDF /Argon2id
/M 65536
/T 3
/P 1
/Check <16 authentication bytes>
```

These fields are direct values. Salt and Check may use PDF hexadecimal or literal
string syntax. The file key is the 32-byte Argon2id output, version 0x13, using
the recorded salt and costs.

Check is the XChaCha20-Poly1305 detached tag for empty plaintext, nonce zero, and
associated data consisting of the ASCII bytes `PSChaCha/Check/v1`, the 16-byte
salt, and M, T, P as three little-endian u32 values. Nonce zero is reserved for
this operation and is refused for object values.

Each string and stream is represented as:

```text
24-byte nonce || ciphertext || 16-byte authentication tag
```

Object associated data is exactly eight bytes: the object number as a
little-endian u32 followed by the generation promoted to a little-endian u32.
Each string receives its own nonce, including strings nested in arrays,
dictionaries, and stream dictionaries. Streams are encrypted after their
declared filters have been applied. Length records the resulting encrypted size.

The security dictionary, cross-reference streams and their dictionaries, and
trailer identifiers remain plaintext. Writer output uses individual indirect
objects; encrypted object-stream authoring is refused so every object-level
string receives separate authentication. Both classic cross-reference tables
and cross-reference streams are supported.

Production salt, initial identifiers and nonces come from `getrandom`.
The internal house-fixture constructor alone substitutes fixed entropy:
salt is sixteen 0x53 bytes, identifiers are sixteen 0x49 bytes, and each nonce
is sixteen 0x4e bytes followed by a little-endian u64 counter starting at one.
This mode is not a public authoring API.

## Opening and editing

Existing `Document::open` and `Document::open_with` continue to work. Opening
PSChaCha without a password returns `DocError::Encrypted`.

New APIs:

```text
Document::open_with_password(source, limits, password)
Document::open_with_password_mode(source, limits, mode, Option<password>)
Editor::open_with_password(bytes, limits, password)
```

`Some(b"")` supplies the empty password; `None` supplies no password.
The new Document APIs leave standard-handler behavior unchanged.

Wrong passwords return `DocError::Malformed` with
`what = "PSChaCha password authentication"`. Corrupt object payloads return
`DocError::Malformed` with `what = "PSChaCha authentication"`.

Opening authenticates all current uncompressed objects, including unused objects
and object-stream containers, before returning the document. String plaintext
is attached to the returned object's nodes using Arc storage; the original file
buffer and spans are retained. Stream plaintext is not cached.
This prevents the existing Vec-returning text APIs from hiding a string tag
failure behind a warning. Authentication scanning observes the cumulative
decoded-byte limit.

KDF allocation is fallible and bounded before derivation. Memory is capped at
the lesser of `limits.max_decoded_bytes` and 256 MiB; iterations are capped at
10 and parallelism at 16. Argon2 also validates its minimum and structural
parameter requirements. Passwords are bounded by the lesser of
`limits.max_object_bytes` and 1 MiB.

`Document::chacha_key` returns an opaque Arc-backed capability for incremental
writing. `Document::encryption_ref` exposes the existing security dictionary
reference. The editor retains the file key, salt and Check, preserves the
original byte prefix and first identifier, and encrypts every pending string
and already-filtered stream using fresh nonces and the actual generation.
The new trailer retains Encrypt. Its cross-reference section stays plaintext.
Repeated encrypted saves have fresh ciphertext and need not be byte-identical.
Replacing the private security dictionary or staging a cross-reference
dictionary as an ordinary encrypted object is refused.

The writer's `encryption::encrypt_value` and `encryption::encrypt_stream`
helpers support authenticated incremental writers using the opaque file key.
Never reuse a nonce with a file key.

## House proof

`encrypted-chacha` uses password `house-chacha-password`. Its visible text is
`Empty user password Encrypted strings and streams`; the first line is fixture
text and does not describe its password.

In a fixture manifest, `open.password` supplies the password to the checker.
The checker also records `open.without_password`, `open.wrong_password`, and
`open.corrupt_ciphertext`. The corruption probe flips one byte after the nonce
in an encrypted stream.

The `pdf-roundtrip` command additionally exercises classic and stream
cross-references, an empty password, nested and binary strings, stream
dictionary strings, distinct nonces, corrupt strings and streams, object
payload swapping, and encrypted incremental metadata, links and content.
