//! Suite 1: does the loop manage processes correctly? Puppet steps that
//! do only what they are told, the loop run as the server runs it, and a
//! person at the controls. Only what the loop owns is asserted: processes
//! started and ended, one at a time per step, each request's outcome, each
//! run's recorded status, the versions it recorded and handed on, and the
//! queue it keeps. Nothing about what a step wrote.

mod harness;
mod locks;
mod scenarios;
mod walk;
