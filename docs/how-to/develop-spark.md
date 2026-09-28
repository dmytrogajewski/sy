# Develop and release Sparkplane

The authoritative developer guide moved to
[Sparkplane: develop, release and deploy](https://github.com/Sumatoshi-tech/sparkplane/blob/main/docs/how-to/develop-spark.md).

Builds, engine/model catalogs, GPU qualification, signed releases, deployment
and rollback belong to Sparkplane. It has its own Rust workspace and CI.
A sy checkout is not required and must not be introduced as a dependency.

Within sy, changes are limited to `src/sparkplane_bridge.rs`, release pins and
integration documentation. Run `make test-spark` and `make lint-spark` plus
the normal sy gates. See [client installation](install-spark.md).
