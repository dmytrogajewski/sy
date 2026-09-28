# Sparkplane bridge reference

`sy spark [ARGS...]` delegates to a pinned, verified Sparkplane executable.
It preserves raw arguments, stdio, terminal behavior, signals and exit codes.
There is no engine, model or appliance policy in sy.

Network access is an explicit per-launch Sparkplane option and is forwarded
unchanged by the bridge:

```sh
sy spark dgx-spark launch codex --allow-network -- --sandbox workspace-write
```

The equivalent environment variable is
`SPARKPLANE_LAUNCH_ALLOW_NETWORK=true`. This opt-in does not change filesystem
sandboxing or approval policy; consult the
[Sparkplane network-control reference](https://github.com/Sumatoshi-tech/sparkplane/blob/main/docs/reference/spark.md#internet-access-for-development)
for client-specific semantics.

See [integration and installation](../how-to/install-spark.md) for release pins
and environment compatibility, and the independent
[Sparkplane reference](https://github.com/Sumatoshi-tech/sparkplane/blob/main/docs/reference/spark.md)
for commands, schemas, admission, inference and release contracts.
