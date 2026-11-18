//! Every route of the contacts app, registered together with what it says
//! about itself, so what `GET /` and `--help` list is what the router serves.

use std::sync::Arc;

use axum::{
    handler::Handler,
    response::Json,
    routing::{get, on, MethodFilter},
    Router,
};
use datalib_contacts::Store;
use serde::Serialize;
use serde_json::json;
use strum::{IntoStaticStr, VariantArray};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, IntoStaticStr)]
#[serde(rename_all = "UPPERCASE")]
#[strum(serialize_all = "UPPERCASE")]
pub(super) enum Verb {
    Get,
    Post,
    Put,
    Delete,
}

impl Verb {
    fn filter(self) -> MethodFilter {
        match self {
            Verb::Get => MethodFilter::GET,
            Verb::Post => MethodFilter::POST,
            Verb::Put => MethodFilter::PUT,
            Verb::Delete => MethodFilter::DELETE,
        }
    }
}

/// What a call to a route means, which an agent has to know before it makes
/// one: the store writes down who said so by which route was called.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, IntoStaticStr, VariantArray)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub(super) enum RouteKind {
    Read,
    Propose,
    Person,
}

impl RouteKind {
    fn means(self) -> &'static str {
        match self {
            RouteKind::Read => "Changes nothing.",
            RouteKind::Propose => {
                "An agent's way to write: recorded under the agent's name, and waits for a \
                 person to confirm it."
            }
            RouteKind::Person => {
                "What the app's buttons call: recorded as the person's own decision, final, \
                 with no review. An agent calls one only when the person asked for that very \
                 change."
            }
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub(super) struct Route {
    pub method: Verb,
    pub path: &'static str,
    pub kind: RouteKind,
    pub does: &'static str,
}

const ABOUT: &str = "The contacts app: a person's own record of who a handle is. A contact \
                     holds handles (an email address, a number, a Slack id), and every chip \
                     and `with:contact:<id>` search follows those links at once, with no \
                     re-render. It also holds labels on people and senders (`real`: not spam), \
                     and what a person turned down.";
const MOUNT: &str = "/applet/datalib_contacts/";
const AUTH: &str =
    "Through datalib-http, with `Authorization: Bearer <data_root>/system/api-token`.";
const FOR_AGENTS: &str = "To link handles to contacts, send them to `POST /proposals`, not \
                          `POST /link`. Read `GET /review/items` and `GET /not_linked` first.";
const GUIDE: &str = "datalib-step help contacts-app";

#[derive(Default)]
pub(super) struct Api {
    router: Router<Arc<Store>>,
    routes: Vec<Route>,
}

impl Api {
    pub(super) fn add<H, T>(
        mut self,
        method: Verb,
        path: &'static str,
        kind: RouteKind,
        does: &'static str,
        handler: H,
    ) -> Self
    where
        H: Handler<T, Arc<Store>>,
        T: 'static,
    {
        self.router = self.router.route(path, on(method.filter(), handler));
        self.routes.push(Route {
            method,
            path,
            kind,
            does,
        });
        self
    }

    /// The router, with `GET /` answering the list of everything added.
    pub(super) fn into_router(self) -> Router<Arc<Store>> {
        let answer = about(&self.routes);
        self.router.route(
            "/",
            get(move || {
                let answer = answer.clone();
                async move { Json(answer) }
            }),
        )
    }

    pub(super) fn routes(&self) -> &[Route] {
        &self.routes
    }
}

fn about(routes: &[Route]) -> serde_json::Value {
    let kinds: serde_json::Map<String, serde_json::Value> = RouteKind::VARIANTS
        .iter()
        .map(|k| {
            let name: &'static str = k.into();
            (name.to_string(), json!(k.means()))
        })
        .collect();
    json!({
        "about": ABOUT,
        "mount": MOUNT,
        "auth": AUTH,
        "for_agents": FOR_AGENTS,
        "handles": datalib_handle::SPELLING,
        "guide": GUIDE,
        "kinds": kinds,
        "routes": routes,
    })
}

/// The same list as `GET /`, for `--help`, where there is no server to ask.
pub(super) fn help_text(routes: &[Route]) -> String {
    let mut out = format!(
        "{ABOUT}\n\nServed under {MOUNT}\n{AUTH}\n{FOR_AGENTS}\nThe handles: {}.\nThe guide: \
         `{GUIDE}`. `GET /` on the running applet answers this list as JSON.\n",
        datalib_handle::SPELLING
    );
    for &kind in RouteKind::VARIANTS {
        let name: &'static str = kind.into();
        out.push_str(&format!("\nRoutes that are `{name}`. {}\n", kind.means()));
        for r in routes.iter().filter(|r| r.kind == kind) {
            let method: &'static str = r.method.into();
            out.push_str(&format!("  {method} {}\n      {}\n", r.path, r.does));
        }
    }
    out
}
