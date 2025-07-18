use axum::http::StatusCode;
use axum::response::IntoResponse;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[allow(non_upper_case_globals)]
#[export_name = "malloc_conf"]
pub static malloc_conf: &[u8] = b"prof:true,prof_active:true,lg_prof_sample:19\0";

#[tokio::main]
async fn main() {
    let mut v = vec![];
    for i in 0..1000000 {
        v.push(i);
    }

    let app = axum::Router::new().route("/debug/pprof/allocs", axum::routing::get(handle_get_heap));

    // Add a flamegraph SVG route if enabled via `cargo run -F flamegraph`.
    #[cfg(feature = "flamegraph")]
    let app = app.route(
        "/debug/pprof/allocs/flamegraph",
        axum::routing::get(handle_get_heap_flamegraph),
    );

    // run our app with hyper, listening globally on port 3000
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}

pub async fn handle_get_heap() -> Result<impl IntoResponse, (StatusCode, String)> {
    let mut prof_ctl = jemalloc_pprof::PROF_CTL.as_ref().unwrap().lock().await;
    require_profiling_activated(&prof_ctl)?;
    let pprof = prof_ctl
        .dump_pprof()
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Ok(pprof)
}

#[cfg(feature = "flamegraph")]
pub async fn handle_get_heap_flamegraph() -> Result<impl IntoResponse, (StatusCode, String)> {
    use axum::body::Body;
    use axum::http::header::CONTENT_TYPE;
    use axum::response::Response;

    let mut prof_ctl = jemalloc_pprof::PROF_CTL.as_ref().unwrap().lock().await;
    require_profiling_activated(&prof_ctl)?;
    let svg = prof_ctl
        .dump_flamegraph()
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    Response::builder()
        .header(CONTENT_TYPE, "image/svg+xml")
        .body(Body::from(svg))
        .map_err(|err| (StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

/// Checks whether jemalloc profiling is activated an returns an error response if not.
fn require_profiling_activated(
    prof_ctl: &jemalloc_pprof::JemallocProfCtl,
) -> Result<(), (axum::http::StatusCode, String)> {
    if prof_ctl.activated() {
        Ok(())
    } else {
        Err((
            axum::http::StatusCode::FORBIDDEN,
            "heap profiling not activated".into(),
        ))
    }
}
