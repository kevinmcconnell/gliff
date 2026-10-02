# gliff-transport

Length-prefixed framing over any tokio `AsyncRead + AsyncWrite`, the
clipboard transfer engine and file spool that both peers run, and the ssh
process spawn for the client.

Payloads (video and cursor images) never go through the CBOR
codec: the writer sends them with `write_vectored` and the reader returns
them as a `Bytes` slice. The frame header covers the payload, so a message
this build does not know is skipped whole.
