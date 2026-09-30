The socket-test certificate and private key are disposable, publicly known
self-signed loopback test fixtures generated with OpenSSL. They identify
localhost solely in native TLS tests and must never be used for a deployed
service. Tests explicitly opt into the self-signed verifier; production
certificate verification is unchanged.
