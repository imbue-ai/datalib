//! Tests that start real applet processes and bind loopback ports, so
//! they cannot be sandboxed: `//datalib/backend/http:applet_tests` runs
//! this module alone, outside the sandbox.

mod applet_endpoint;
mod applet_proxy;
