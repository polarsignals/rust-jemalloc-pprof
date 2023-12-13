[![Discord](https://img.shields.io/discord/813669360513056790?label=Discord)](https://discord.gg/Qgh4c9tRCE)

# rust-jemalloc-pprof

A rust library to collect and convert Heap profiling data from the [jemalloc](https://jemalloc.net/) allocator and convert it to the [pprof](https://github.com/google/pprof/tree/main/proto) format.

To understand how to use this together with Polar Signals Cloud to continuously collect profiling data, refer to the [Use with Polar Signals Cloud](#use-with-polar-signals-cloud) section.

This code was originally developed as part of [Materialize](https://github.com/MaterializeInc/materialize), and then in a collaboration extracted into this standalone library.

## Usage

Internally this library uses [`tikv-jemalloc-ctl`](https://docs.rs/tikv-jemalloc-ctl/latest/tikv_jemalloc_ctl/) to interact with jemalloc, so to use it, you must use the jemalloc allocator via the [`tikv-jemallocator`](https://crates.io/crates/tikv-jemallocator) library.

When adding `tikv-jemallocator` as a dependency, make sure to enable the `profiling` feature.

```toml
[dependencies]
[target.'cfg(not(target_env = "msvc"))'.dependencies]
tikv-jemallocator = { version = "0.5.4", features = ["profiling", "unprefixed_malloc_on_supported_platforms"] }
```

> Note: We also recommend enabling the `unprefixed_malloc_on_supported_platforms` feature, not strictly necessary, but will influence the rest of the usage.

Then configure the global allocator and configure it with profiling enabled.

```rust
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static malloc_conf: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";
```

> If you do not use the `unprefixed_malloc_on_supported_platforms` feature, you have to name it `_rjem_malloc_conf` it instead of `malloc_conf`.

2^19 bytes (512KiB) is the default configuration for the sampling period, but we recommend being explicit. To understand more about jemalloc sampling check out the [detailed docs](https://github.com/jemalloc/jemalloc/blob/dev/doc_internal/PROFILING_INTERNALS.md#sampling) on it.

We recommend serving the profiling data on an HTTP server such as [axum](https://github.com/tokio-rs/axum), that could look like this, and we'll intentionally include a 4mb allocation to trigger sampling.

```rust
#[tokio::main]
async fn main() {
    let mut v = vec![];
    for i in 0..1000000 {
        v.push(i);
    }

    let app = axum::Router::new()
        .route("/debug/pprof/heap", axum::routing::get(handle_get_heap));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

pub async fn handle_get_heap() -> Result<impl axum::response::IntoResponse, (axum::http::StatusCode, String)> {
    let mut prof_ctl = jemalloc_pprof::jemalloc::PROF_CTL.as_ref().unwrap().lock().await;
    require_profiling_activated(&prof_ctl)?;
    let dump_file = prof_ctl
        .dump()
        .map_err(|err| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let dump_reader = BufReader::new(dump_file);
    let profile = jemalloc_pprof::jemalloc::parse_jeheap(dump_reader)
        .map_err(|err| (axum::http::StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    let pprof = profile.to_pprof(("inuse_space", "bytes"), ("space", "bytes"), None);
    Ok(pprof)
}

/// Checks whether jemalloc profiling is activated an returns an error response if not.
fn require_profiling_activated(prof_ctl: &jemalloc_pprof::jemalloc::JemallocProfCtl) -> Result<(), (axum::http::StatusCode, String)> {
    if prof_ctl.activated() {
        Ok(())
    } else {
        Err((axum::http::StatusCode::FORBIDDEN, "heap profiling not activated".into()))
    }
}
```

Then running the application, we can capture a profile and view it the pprof toolchain.

```
curl localhost:3000/debug/pprof/heap > heap.pb.gz
pprof -http=:8080 heap.pb.gz
```

> Note: The profiling data is not symbolized, so either `addr2line` or `llvm-addr2line` needs to be available in the path and pprof needs to be able to discover the respective debuginfos.

## Use with Polar Signals Cloud

Polar Signals Cloud allows continuously collecting heap profiling data, so you always have the right profiling data available, and don't need to search for the right data, you already have it!

Polar Signals Cloud supports anything in the pprof format, so a process exposing the above explained pprof endpoint, can then be scraped as elaborated in the [scraping docs](https://www.polarsignals.com/docs/setup-scraper).
