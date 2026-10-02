# gliff-proto

The gliff wire protocol: the message types (CBOR with numeric keys), the
frame header with its out-of-band payload, and the clipboard transfer rules.
It also holds the CPU reference for the colour conversion and the 4:4:4
chroma split and recombine that the GPU shaders must match.

No I/O here; `gliff-transport` moves the bytes. The rules for changing the
protocol without a version bump are at the top of `src/msg.rs`.
