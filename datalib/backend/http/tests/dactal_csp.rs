//! CI-visible guard for the DACTAL page's CSP (issue #138, mitigation 4).
//!
//! `datalib/ui/tests/e2e/dactal-csp.spec.ts` is the real test: it drives
//! a browser and proves the policy both blocks dactal.org and leaves the
//! engine working. But `//datalib/ui:e2e_test` is excluded from the CI
//! merge gate today (see the `FIXME(e2e)` in
//! `.github/workflows/test.yml` — the devcontainer image lacks rsync and
//! a Chromium cache until the next `v*` republish). So on a pull request
//! nothing currently catches a deleted or gutted CSP.
//!
//! This closes that window with checks that need no browser: the policy
//! is present with its two load-bearing directives, and the page still
//! has no inline script for `'unsafe-inline'` to be demanded for. It
//! asserts the *shape* of the defense, not its behavior — when the e2e
//! suite is back in the gate, that one supersedes this and this can go.
//!
//! It reads the page out of the embedded UI bundle through the real
//! router, so it also proves the file survives the vite/bazel build into
//! what we actually ship.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use datalib_core::app_store::AppStore;
use datalib_http::{router, ApiToken, AppState};
use std::path::PathBuf;
use std::sync::Arc;
use tower::ServiceExt;

const TOKEN: &str = "dactal-csp-itest";

async fn fetch(path: &str) -> (StatusCode, String) {
    let db_path = tempfile::TempDir::with_prefix("datalib-http-csp-itest-")
        .expect("create tempdir")
        .keep()
        .join("backend_index.doltlite_db");
    let root: Arc<PathBuf> = Arc::new(db_path.parent().unwrap().to_path_buf());
    let app = AppStore::open(root.as_path())
        .await
        .expect("open app stores");
    let state = AppState {
        root: root.clone(),
        app: Arc::new(app),
        progress_tx: tokio::sync::broadcast::channel(16).0,
        root_tx: tokio::sync::broadcast::channel(16).0,
        // No sampler running here, so the monitor is empty and every
        // tree reports as absent — the state a root nobody has walked
        // is in.
        usage: Default::default(),
        applets: Arc::new(datalib_http::applets::AppletRegistry::build(
            Vec::new(),
            (*root).clone(),
            None,
        )),
        api_token: ApiToken::from_value(TOKEN, root.as_path()),
    };
    let req = Request::get(path)
        .header("x-datalib-token", TOKEN)
        .body(Body::empty())
        .unwrap();
    let resp = router(state).oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn the_dactal_page_still_carries_its_csp() {
    let (status, html) = fetch("/dactal/index.html").await;
    assert_eq!(status, StatusCode::OK);

    assert!(
        html.contains("Content-Security-Policy"),
        "the DACTAL page must declare a CSP — without it the vendored \
         engine's dactal.org paths are live again"
    );

    // The two directives that do the work. `script-src 'self'` kills the
    // <script src="https://dactal.org/…"> injections in
    // dactal_utils.js:325 and :381; `connect-src 'self'` kills the
    // fetch() in :393 that feeds `new Function`.
    assert_eq!(
        directive(&html, "script-src"),
        "script-src 'self' 'unsafe-eval'",
        "script-src must stay 'self' — plus 'unsafe-eval', which the \
         query language genuinely needs. Full policy:\n{}",
        csp_of(&html)
    );
    assert_eq!(
        directive(&html, "connect-src"),
        "connect-src 'self'",
        "connect-src must stay 'self'. Full policy:\n{}",
        csp_of(&html)
    );

    // The failure mode this is really here for: someone hits a blocked
    // inline script and "fixes" it by widening script-src, which undoes
    // the whole mitigation.
    let script_src = directive(&html, "script-src");
    assert!(
        !script_src.contains("'unsafe-inline'"),
        "script-src must never allow 'unsafe-inline' — it re-opens \
         exactly what the CSP closes. If an inline script is in the way, \
         move it into a file (that is why main.js exists). Found: \
         {script_src:?}"
    );
}

/// The `content="…"` of the page's CSP meta tag. Anchored on the whole
/// `http-equiv=` attribute, not on the bare policy name: the comment
/// above the tag explains the policy and names it too, so a looser
/// search lands in the prose instead of the markup.
fn csp_of(html: &str) -> String {
    const ANCHOR: &str = r#"http-equiv="Content-Security-Policy""#;
    let Some((_, rest)) = html.split_once(ANCHOR) else {
        return String::new();
    };
    let Some((_, rest)) = rest.split_once("content=\"") else {
        return String::new();
    };
    rest.split('"').next().unwrap_or_default().to_string()
}

/// One directive out of that policy, whitespace-normalized — the source
/// spreads the policy over several lines for readability.
fn directive(html: &str, name: &str) -> String {
    let csp = csp_of(html);
    csp.split(';')
        .map(|d| d.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|d| d.starts_with(name))
        .unwrap_or_default()
}

/// The other half of the same invariant, from the page's side: the CSP
/// forbids inline script, so every `<script>` on the page must load from
/// a file. Moving `main.js` back inline would break the page at runtime
/// — in a browser nobody runs in CI — so catch it here instead.
#[tokio::test]
async fn the_dactal_page_has_no_inline_script() {
    let (_, html) = fetch("/dactal/index.html").await;
    for tag in html.split("<script").skip(1) {
        let open = tag.split('>').next().unwrap_or_default();
        assert!(
            open.contains("src="),
            "found a <script{open}> with no src — the page's CSP forbids \
             inline script, so this cannot run. Put it in a file next to \
             main.js."
        );
    }
    assert!(
        html.contains("main.js"),
        "the page's own logic should still load from main.js"
    );
}
