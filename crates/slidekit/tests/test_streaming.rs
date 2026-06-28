//! Streaming / byte-range efficiency tests.
//!
//! A synthetic tiled TIFF is served from a mock HTTP server (`wiremock`) that
//! honours `Range` requests, then opened through `OpenSlide::open_url_async`
//! — i.e. the `object_store` HTTP backend, the exact path used for real remote
//! slides on S3/GCS/Azure/HTTP. These tests assert two things the in-memory
//! `object_store` tests can't:
//!
//!   1. The HTTP range-request wire protocol works end to end (HEAD for size,
//!      `Range:` GET -> `206 Partial Content` + `Content-Range`).
//!   2. Reading a small region fetches only the relevant tiles' bytes, not the
//!      whole file — proving reads stream rather than downloading everything.
//!
//! No network and no downloaded test data: everything is served from memory on
//! localhost, so this runs anywhere.

mod common;

use std::sync::Arc;

use object_store::http::HttpBuilder;
use object_store::path::Path as StorePath;
use object_store::{ClientOptions, ObjectStore};
use slidekit::{OpenSlide, color};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate, matchers::any};

const TILE: u32 = 64;
const TILES_X: u32 = 8;
const TILES_Y: u32 = 8;

/// Distinct solid color per tile so a decoded pixel identifies its source tile.
fn tile_color(tx: u32, ty: u32) -> (u8, u8, u8) {
    ((tx as u8) * 16 + 8, (ty as u8) * 16 + 8, 128)
}

/// A `wiremock` responder that serves a byte buffer with HTTP range support:
///
/// - `HEAD`            -> `200` with the correct `Content-Length`.
/// - `GET` + `Range:`  -> `206` with `Content-Range` and just the requested slice.
/// - bare `GET`        -> `200` with the full body.
struct RangeFile {
    body: Arc<Vec<u8>>,
}

impl Respond for RangeFile {
    fn respond(&self, req: &Request) -> ResponseTemplate {
        let total = self.body.len();

        if req.method.as_str().eq_ignore_ascii_case("HEAD") {
            // Set the body so the framework reports the right Content-Length;
            // a HEAD response's body is discarded by the client.
            return ResponseTemplate::new(200)
                .append_header("Accept-Ranges", "bytes")
                .set_body_bytes(self.body.as_ref().clone());
        }

        if let Some((start, end)) = req
            .headers
            .get("range")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| parse_byte_range(v, total))
        {
            let slice = self.body[start..=end].to_vec();
            return ResponseTemplate::new(206)
                .append_header("Content-Range", format!("bytes {start}-{end}/{total}"))
                .set_body_bytes(slice);
        }

        ResponseTemplate::new(200).set_body_bytes(self.body.as_ref().clone())
    }
}

/// Parse an HTTP `bytes=start-end` range (closed interval) against `total`.
/// Returns inclusive, clamped `(start, end)`, or `None` if unsatisfiable.
fn parse_byte_range(value: &str, total: usize) -> Option<(usize, usize)> {
    let spec = value.trim().strip_prefix("bytes=")?;
    let (s, e) = spec.split_once('-')?;
    let start: usize = s.trim().parse().ok()?;
    let end: usize = if e.trim().is_empty() {
        total.checked_sub(1)?
    } else {
        e.trim().parse().ok()?
    };
    if start > end || start >= total {
        return None;
    }
    Some((start, end.min(total - 1)))
}

/// Start a mock server that serves `body` at `/slide.tif` with range support.
/// Returns the live server (kept alive by the caller) and its base URI.
async fn serve(body: Vec<u8>) -> (MockServer, String) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(RangeFile {
            body: Arc::new(body),
        })
        .mount(&server)
        .await;
    let base = server.uri();
    (server, base)
}

/// Open `slide.tif` from `base` through the `object_store` HTTP backend — the
/// same code path as a real remote slide. `allow_http` is enabled only because
/// the mock serves plaintext on localhost; production `OpenSlide::open_url`
/// goes over HTTPS and needs no such accommodation.
async fn open_http_slide(base: &str) -> OpenSlide {
    let store = HttpBuilder::new()
        .with_url(base.to_string())
        .with_client_options(ClientOptions::default().with_allow_http(true))
        .build()
        .expect("build HTTP object store");
    let store: Arc<dyn ObjectStore> = Arc::new(store);
    OpenSlide::open_store_async(store, StorePath::from("slide.tif"))
        .await
        .expect("open synthetic slide over HTTP")
}

/// Sum the requested lengths of every `Range:` GET the server received.
async fn range_bytes_requested(server: &MockServer, total: usize) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str().eq_ignore_ascii_case("GET"))
        .filter_map(|r| r.headers.get("range").and_then(|v| v.to_str().ok()))
        .filter_map(|v| parse_byte_range(v, total))
        .map(|(s, e)| e - s + 1)
        .sum()
}

async fn count_method(server: &MockServer, method: &str) -> usize {
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r| r.method.as_str().eq_ignore_ascii_case(method))
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn open_url_reads_region_over_http() {
    let body = common::build_tiled_rgb_tiff(TILE, TILES_X, TILES_Y, tile_color);
    let (server, base) = serve(body).await;

    let slide = open_http_slide(&base).await;

    let (w, h) = slide.level0_dimensions();
    assert_eq!(w as u32, TILES_X * TILE);
    assert_eq!(h as u32, TILES_Y * TILE);

    // Read exactly the top-left tile and the tile diagonally opposite; each must
    // decode to its own distinct color, confirming correct range placement.
    let tl = slide
        .read_region_async(0, 0, 0, TILE as i64, TILE as i64)
        .await
        .unwrap();
    let (r, g, b) = tile_color(0, 0);
    assert_eq!(tl[0], color::argb(0xFF, r, g, b), "top-left tile color");
    assert_eq!(tl.len(), (TILE * TILE) as usize);

    let far_x = ((TILES_X - 1) * TILE) as i64;
    let far_y = ((TILES_Y - 1) * TILE) as i64;
    let br = slide
        .read_region_async(far_x, far_y, 0, TILE as i64, TILE as i64)
        .await
        .unwrap();
    let (r, g, b) = tile_color(TILES_X - 1, TILES_Y - 1);
    assert_eq!(br[0], color::argb(0xFF, r, g, b), "bottom-right tile color");

    // Keep the server alive for the duration of the reads.
    drop(server);
}

#[tokio::test(flavor = "multi_thread")]
async fn small_region_streams_only_relevant_tiles() {
    let body = common::build_tiled_rgb_tiff(TILE, TILES_X, TILES_Y, tile_color);
    let total = body.len();
    let (server, base) = serve(body).await;

    let slide = open_http_slide(&base).await;

    // Reading a single 64x64 tile should transfer that one tile plus a little
    // metadata — never the whole multi-tile file.
    let _ = slide
        .read_region_async(0, 0, 0, TILE as i64, TILE as i64)
        .await
        .unwrap();

    let heads = count_method(&server, "HEAD").await;
    let bytes_fetched = range_bytes_requested(&server, total).await;

    assert_eq!(heads, 1, "exactly one HEAD to discover the size on open");
    assert!(bytes_fetched > 0, "reads should use range requests");
    assert!(
        bytes_fetched < total / 4,
        "fetched {bytes_fetched} of {total} bytes reading one tile; \
         expected a small fraction (streaming, not whole-file download)"
    );

    drop(server);
}
