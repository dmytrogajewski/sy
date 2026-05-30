//! `define_plugin!` macro — the ergonomic surface the PDK exists for.
//!
//! Plugin authors write:
//!
//! ```ignore
//! use sy_plugin_pdk::prelude::*;
//!
//! define_plugin! {
//!     id: "echo",
//!     api: "1",
//!     capabilities: [Previewer { mime: "text/plain" }],
//!     handlers: {
//!         "preview": |req: PreviewReq| -> Result<PreviewResp> {
//!             Ok(PreviewResp::text(format!("hello {}", req.path)))
//!         }
//!     }
//! }
//! ```
//!
//! The macro is a `macro_rules!` declaration — no proc-macro split is
//! required for the surface Step 11 ships. The expansion is one
//! `main()` that builds a [`crate::runtime::PluginInfo`] and a
//! [`crate::runtime::HandlerTable`] then hands both to
//! [`crate::runtime::run`].

/// Build a [`crate::types::Capability`] from a `Kind { field: expr,
/// ... }` shape. Internal helper for [`define_plugin!`].
#[doc(hidden)]
#[macro_export]
macro_rules! __sy_pdk_cap {
    (Previewer, $( $k:ident : $v:expr ),* $(,)? ) => {{
        #[allow(unused_mut)]
        let mut cap = $crate::types::Capability {
            kind: "previewer".to_string(),
            url: None,
            mime: None,
        };
        $( $crate::__sy_pdk_cap!(@set cap, $k, $v); )*
        cap
    }};
    (@set $cap:ident, mime, $v:expr) => { $cap.mime = Some(($v).to_string()); };
    (@set $cap:ident, url, $v:expr) => { $cap.url = Some(($v).to_string()); };
}

/// Wrap a handler closure into the type-erased
/// [`crate::runtime::HandlerFn`] bridge. The user writes
/// `|req: TypedReq, host| -> Result<TypedResp> { ... }` (or omits the
/// `host` parameter); the bridge deserialises the JSON-RPC `params`
/// into `TypedReq`, runs the body, and serialises `TypedResp` back to
/// `serde_json::Value`.
#[doc(hidden)]
#[macro_export]
macro_rules! __sy_pdk_handler {
    ($map:expr, $name:literal, |$req:ident : $reqty:ty , $host:ident $(: $_htty:ty)?| -> $rty:ty $body:block) => {{
        let entry: $crate::runtime::HandlerFn = ::std::sync::Arc::new(
            move |__params: $crate::__priv::serde_json::Value,
                  $host: ::std::sync::Arc<$crate::runtime::HostHandle>|
                  -> ::std::pin::Pin<
                ::std::boxed::Box<
                    dyn ::std::future::Future<
                            Output = ::std::result::Result<
                                $crate::__priv::serde_json::Value,
                                $crate::__priv::anyhow::Error,
                            >,
                        > + Send,
                >,
            > {
                ::std::boxed::Box::pin(async move {
                    let $req: $reqty = $crate::__priv::serde_json::from_value(__params)
                        .map_err(|e| $crate::__priv::anyhow::anyhow!("invalid params: {e}"))?;
                    // Run the user body inline so it can `.await`. The
                    // body returns `$rty` (a `Result<T, _>`); we
                    // unwrap it here so the bridge surfaces the inner
                    // `T` for serialisation and propagates the `Err`
                    // through the outer `HandlerFn` chain.
                    let __step: $rty = async { $body }.await;
                    let __out = __step?;
                    let v = $crate::__priv::serde_json::to_value(&__out)
                        .map_err(|e| $crate::__priv::anyhow::anyhow!("serialise result: {e}"))?;
                    Ok(v)
                })
            },
        );
        $map.insert($name, entry);
    }};
    // Author omitted the host param; default it to `_host`.
    ($map:expr, $name:literal, |$req:ident : $reqty:ty| -> $rty:ty $body:block) => {
        $crate::__sy_pdk_handler!($map, $name, |$req: $reqty, _host| -> $rty $body)
    };
}

/// The plugin author's entry point. Expands into a `main` that drives
/// the PDK runtime against `tokio::io::{stdin, stdout}`.
///
/// Accepts:
///
/// * `id: <str literal>` — plugin id (kebab-case, matches manifest).
/// * `api: <str literal>` — plugin API version (`"1"` today).
/// * `version: <str literal>` (optional) — plugin version; defaults
///   to `env!("CARGO_PKG_VERSION")`.
/// * `capabilities: [Kind { field: value, ... }, ...]` — compile-time
///   capability list, mirroring `[[capability]]` rows. `Kind` is one
///   of `Previewer` today; the macro is extensible by adding new
///   arms to [`__sy_pdk_cap!`].
/// * `handlers: { <name>: |req: Ty| -> Result<Ty> { ... }, ... }` —
///   one async-ready closure per capability method.
#[macro_export]
macro_rules! define_plugin {
    (
        id: $id:literal,
        api: $api:literal,
        $(version: $ver:literal,)?
        capabilities: [ $( $capkind:ident { $( $capk:ident : $capv:expr ),* $(,)? } ),* $(,)? ],
        handlers: { $( $hname:literal : | $hreq:ident : $hreqty:ty $(, $hhost:ident $(: $hhostty:ty)? )? | -> $hrty:ty $hbody:block ),* $(,)? }
        $(,)?
    ) => {
        fn main() -> ::std::io::Result<()> {
            let rt = $crate::__priv::tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async {
                let info = $crate::runtime::PluginInfo {
                    id: $id,
                    version: $crate::define_plugin!(@ver $($ver)?),
                    api: $api,
                    capabilities: vec![ $( $crate::__sy_pdk_cap!($capkind, $( $capk : $capv ),* ) ),* ],
                };
                let mut __pdk_handlers: $crate::runtime::HandlerTable =
                    ::std::collections::HashMap::new();
                $( $crate::define_plugin!(@reg __pdk_handlers, $hname, $hreq, $hreqty, $($hhost,)? $hrty, $hbody); )*
                $crate::runtime::run(info, __pdk_handlers).await
            })
        }
    };
    (@ver $ver:literal) => { $ver };
    (@ver) => { env!("CARGO_PKG_VERSION") };
    // Two-arg form: req + host.
    (@reg $map:ident, $name:literal, $req:ident, $reqty:ty, $host:ident, $rty:ty, $body:block) => {
        $crate::__sy_pdk_handler!($map, $name, |$req: $reqty, $host| -> $rty $body)
    };
    // One-arg form: req only.
    (@reg $map:ident, $name:literal, $req:ident, $reqty:ty, $rty:ty, $body:block) => {
        $crate::__sy_pdk_handler!($map, $name, |$req: $reqty| -> $rty $body)
    };
}
