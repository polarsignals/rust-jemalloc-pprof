[![Discord](https://img.shields.io/discord/813669360513056790?label=Discord)](https://discord.gg/Qgh4c9tRCE)

# rust-jemalloc-pprof

A rust library to collect and convert Heap profiling data from the [jemalloc](https://jemalloc.net/) allocator and convert it to the [pprof](https://github.com/google/pprof/tree/main/proto) format.

To understand how to use this together with Polar Signals Cloud to continuously collect profiling data, refer to the [Use with Polar Signals Cloud](#use-with-polar-signals-cloud) section.

This code was originally developed as part of [Materialize](https://github.com/MaterializeInc/materialize), and then in a collaboration extracted into this standalone library.

## Requirements

Currently, this library only supports Linux.

Furthermore, you must be able to switch your allocator to `jemalloc`.
If you need to continue using the default system allocator for any reason,
this library will not be useful.

## Usage

Internally this library uses [`tikv-jemalloc-ctl`](https://docs.rs/tikv-jemalloc-ctl/latest/tikv_jemalloc_ctl/) to interact with jemalloc, so to use it, you must use the jemalloc allocator via the [`tikv-jemallocator`](https://crates.io/crates/tikv-jemallocator) library.

When adding `tikv-jemallocator` as a dependency, make sure to enable the `profiling` feature.

```toml
[dependencies]
[target.'cfg(not(target_env = "msvc"))'.dependencies]
tikv-jemallocator = { version = "0.6.0", features = ["profiling", "unprefixed_malloc_on_supported_platforms"] }
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

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

use axum::http::StatusCode;
use axum::response::IntoResponse;

pub async fn handle_get_heap() -> Result<impl IntoResponse, (StatusCode, String)> {
    let mut prof_ctl = jemalloc_pprof::PROF_CTL.as_ref().unwrap().lock().await;
    require_profiling_activated(&prof_ctl)?;
    let pprof = prof_ctl
        .dump_pprof()
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(pprof)
}

/// Checks whether jemalloc profiling is activated an returns an error response if not.
fn require_profiling_activated(prof_ctl: &jemalloc_pprof::JemallocProfCtl) -> Result<(), (StatusCode, String)> {
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

### Writeable temporary directory

The way this library works is that it creates a new temporary file (in the [platform-specific default temp dir](https://docs.rs/tempfile/latest/tempfile/struct.NamedTempFile.html)), and instructs jemalloc to dump a profile into that file. Therefore the platform respective temporary directory must be writeable by the process. After reading and converting it to pprof, the file is cleaned up via the destructor. A single profile tends to be only a few kilobytes large, so it doesn't require a significant space, but it's non-zero and needs to be writeable.

## Use with Polar Signals Cloud

Polar Signals Cloud allows continuously collecting heap profiling data, so you always have the right profiling data available, and don't need to search for the right data, you already have it!

Polar Signals Cloud supports anything in the pprof format, so a process exposing the above explained pprof endpoint, can then be scraped as elaborated in the [scraping docs](https://www.polarsignals.com/docs/setup-scraper).

## Use from C or C++

The functionality to dump the current jemalloc heap profile in pprof
format is exposed to C and C++ (or any other language that can use
jemalloc and can link against libraries via the C ABI). This
functionality is exposed via the `capi` (C API) package.

### Building

The following prerequisites are necessary to build the C API package:

* Working Rust and C toolchains. The former can be installed by
  following the instructions at https://rustup.rs . The latter can be
  installed via the distribution's package manager. For example, on
  Ubuntu, run `sudo apt install build-essential`.
* `jemalloc` and its development headers. For example, on Ubuntu, run
  `sudo apt install jemalloc-dev`.

Once the prerequisites are installed, the library can be built by
running `cargo build -p capi --release`. There are three files of
interest:

* The library itself, produced at
  `target/release/libjemalloc_pprof.so`
* A header file, at `capi/include/jemalloc_pprof.h`
* A manual page, at `capi/man/jemalloc_pprof.3`.

The procedure for installing and using these files depends on your
distribution and build system.

### Use

Ensure that your binaries link against both jemalloc and
jemalloc_pprof by passing the linker flags `-ljemalloc
-ljemalloc_pprof`. The procedure for ensuring that these flags are
passed depends on your build system and is currently outside the scope
of this document.

Once that is done, profiling can be enabled either by setting the
`MALLOC_CONF` variable or by defining a symbol called `malloc_conf` in
the binary. For example:

``` shell
export MALLOC_CONF="prof:true,prof_active:true,lg_prof_sample:19"
```

See the `jemalloc` man page for more details. When profiling is
enabled, a profile may be dumped in pprof format via the
`dump_jemalloc_pprof` function.

### Example

This program allocates between 1 and 10 MiB every 100 milliseconds,
and dumps a profile to the file `my_profile` every 2 seconds.

``` c
#include <assert.h>
#include <errno.h>
#include <unistd.h>
#include <stdlib.h>
#include <pthread.h>
#include <stdio.h>

#include <jemalloc_pprof.h>

void
a()
{
        size_t sz = 1 * 1024 * 1024;
        char *x = malloc(sz);
        for (size_t i = 0; i < sz; ++i) {
                x[i] = '\0';
        }
}

void
b()
{
        size_t sz = 2 * 1024 * 1024;
        char *x = malloc(sz);
        for (size_t i = 0; i < sz; ++i) {
                x[i] = '\0';
        }
}

void
c()
{
        size_t sz = 3 * 1024 * 1024;
        char *x = malloc(sz);
        for (size_t i = 0; i < sz; ++i) {
                x[i] = '\0';
        }
}

void
d()
{
        size_t sz = 4 * 1024 * 1024;
        char *x = malloc(sz);
        for (size_t i = 0; i < sz; ++i) {
                x[i] = '\0';
        }
}

void
e()
{
        size_t sz = 5 * 1024 * 1024;
        char *x = malloc(sz);
        for (size_t i = 0; i < sz; ++i) {
                x[i] = '\0';
        }
}

void
f()
{
        size_t sz = 6 * 1024 * 1024;
        char *x = malloc(sz);
        for (size_t i = 0; i < sz; ++i) {
                x[i] = '\0';
        }
}

void
g()
{
        size_t sz = 7 * 1024 * 1024;
        char *x = malloc(sz);
        for (size_t i = 0; i < sz; ++i) {
                x[i] = '\0';
        }
}

void
h()
{
        size_t sz = 8 * 1024 * 1024;
        char *x = malloc(sz);
        for (size_t i = 0; i < sz; ++i) {
                x[i] = '\0';
        }
}

void
j()
{
        size_t sz = 9 * 1024 * 1024;
        char *x = malloc(sz);
        for (size_t i = 0; i < sz; ++i) {
                x[i] = '\0';
        }
}

void
k()
{
        size_t sz = 10 * 1024 * 1024;
        char *x = malloc(sz);
        for (size_t i = 0; i < sz; ++i) {
                x[i] = '\0';
        }
}

void *
repeatedly_dump(void *ignored)
{
        char *buf;
        size_t len = 0;
        int result;
        for (;;) {
                sleep(2);
                result = dump_jemalloc_pprof(&buf, &len);
                if (result != JP_SUCCESS) {
                        fprintf(stderr, "errno: %d\n", errno);
                        continue;
                }
                if (buf) {                        
                        FILE *file = fopen("my_profile", "w");
                        assert(file);

                        fwrite(buf, sizeof(char), len, file);    
                        fclose(file);
                        printf("dumped pprof of size %lu\n", len);
                        free(buf);
                }
        }
        return NULL;
}

int
main()
{
        pthread_t tid;
        int result;

        result = pthread_create(&tid, NULL, repeatedly_dump, NULL);
        assert(!result);
        for (;;) {
                usleep(100000);
                switch (rand() % 10) {
                case 0:
                        a();
                        break;
                case 1:
                        b();
                        break;
                case 2:
                        c();
                        break;
                case 3:
                        d();
                        break;
                case 4:
                        e();
                        break;
                case 5:
                        f();
                        break;
                case 6:
                        g();
                        break;
                case 7:
                        h();
                        break;
                case 8:
                        j();
                        break;
                case 9:
                        k();
                        break;
                }
        }
}
```
