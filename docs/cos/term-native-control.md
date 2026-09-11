# Term native control

The global TCP `term` diagnostic registration serves discovery (`INFO` and
`HELP`) and sends completion notifications. It refuses all protected reads,
mutations and property requests with `FORBIDDEN`, including when native-session
bootstrap fails. It is not an alternative control route.

Protected controls belong on the broker-allocated, verified Unix Term identity.
BROKER-023 defines their policy; a diagnostic service name is never proof of
authority.
