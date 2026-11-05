# QUIC Initial test vectors

The binary files contain the 1200-byte protected Client Initial packets from:

- RFC 9001, Appendix A.2: https://www.rfc-editor.org/rfc/rfc9001.html#appendix-A.2
- RFC 9369, Appendix A.2: https://www.rfc-editor.org/rfc/rfc9369.html#appendix-A.2

Both encode `example.com`. These independent vectors test header protection,
Initial key derivation, authenticated decryption, CRYPTO framing, and SNI parsing.
