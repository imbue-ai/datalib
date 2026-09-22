//! The applet tests of `datalib-http`, one binary: both start real
//! applet processes and bind loopback ports, so neither can be
//! sandboxed. The hermetic endpoint tests are `http_tests/`.

mod applet_endpoint;
mod applet_proxy;
