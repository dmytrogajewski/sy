# Serve a model with Sparkplane

Follow the authoritative
[Sparkplane serving guide](https://github.com/Sumatoshi-tech/sparkplane/blob/main/docs/how-to/serve-a-model-on-spark.md).

Once the [sy integration](install-spark.md) is installed, substitute
`sy spark` for `sparkplane` in those commands. Engine and model configuration
is maintained and released in Sparkplane, not sy.

For an agent session that needs network access, pass the opt-in through the
bridge exactly as you would to Sparkplane:

```sh
sy spark dgx-spark launch codex --allow-network -- --sandbox workspace-write
```

The grant applies only to that launch and does not disable filesystem
sandboxing or alter approval policy. Use
`SPARKPLANE_LAUNCH_ALLOW_NETWORK=true` when an environment variable is more
convenient.
