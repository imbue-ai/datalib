//! Locks, end to end: declared in the config, held by puppets, and kept
//! by the loop. All the parallelism they allow, and no more.

use datalib_dag::supervisor::store::RequestOutcome;

use crate::harness::{reads, source, Clocks, Harness, Seen};

/// Whichever of `steps` starts first, and its pid.
async fn first_started(h: &mut Harness, steps: &[&str]) -> String {
    let steps: Vec<String> = steps.iter().map(|s| s.to_string()).collect();
    h.wait("one of them to start", |s| match s {
        Seen::Ack { step, what, .. } if what.starts_with("started") && steps.contains(step) => {
            Some(step.clone())
        }
        _ => None,
    })
    .await
}

/// A lock of one slot: two sources sharing an account's quota never run
/// together, and a source that holds nothing runs beside either.
#[tokio::test(flavor = "multi_thread")]
async fn a_named_mutex_keeps_its_holders_apart_and_nobody_else() {
    let steps = [
        source("a").locks(r#"["quota"]"#),
        source("b").locks(r#"["quota"]"#),
        source("free"),
    ];
    let quota = "[[locks]]\nname = \"quota\"\nslots = 1\n\n";
    let mut h = Harness::with_locks(&steps, Clocks::default(), quota).await;
    let sync = h.sync(&["a", "b", "free"]).await;
    let first = first_started(&mut h, &["a", "b"]).await;
    let second = if first == "a" { "b" } else { "a" };
    h.started("free").await;
    let waiting = format!("waiting for lock quota, held by {first}");
    let what = format!("{second} to wait for the lock");
    h.until(&what, |s| {
        (s.detail(second) == Some(waiting.as_str())).then_some(())
    })
    .await;
    assert_eq!(h.state().await.started(second), 0);

    h.run(&first, "ok v1").await;
    h.started(second).await;
    h.run(second, "ok v1").await;
    h.run("free", "ok v1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    h.finish().await;
}

/// `slots = 2`: two of three holders run at once, and the third takes the
/// slot the first to end gives back.
#[tokio::test(flavor = "multi_thread")]
async fn a_lock_lets_as_many_run_as_it_has_slots() {
    let steps = [
        source("a").locks(r#"["pool"]"#),
        source("b").locks(r#"["pool"]"#),
        source("c").locks(r#"["pool"]"#),
    ];
    let pool = "[[locks]]\nname = \"pool\"\nslots = 2\n\n";
    let mut h = Harness::with_locks(&steps, Clocks::default(), pool).await;
    let sync = h.sync(&["a", "b", "c"]).await;
    let one = first_started(&mut h, &["a", "b", "c"]).await;
    let rest: Vec<&str> = ["a", "b", "c"].into_iter().filter(|s| *s != one).collect();
    let two = first_started(&mut h, &rest).await;
    let third = *rest.iter().find(|s| **s != two).unwrap();
    h.until("the third to wait for a slot", |s| {
        s.detail(third)
            .is_some_and(|d| d.starts_with("waiting for lock pool"))
            .then_some(())
    })
    .await;
    assert_eq!(h.state().await.started(third), 0);

    h.run(&one, "ok v1").await;
    h.started(third).await;
    h.run(&two, "ok v1").await;
    h.run(third, "ok v1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    h.finish().await;
}

/// Exclusive takes every slot: it waits for a shared holder, then runs
/// alone, and a shared holder asked for meanwhile waits for it.
#[tokio::test(flavor = "multi_thread")]
async fn an_exclusive_holder_waits_for_shared_ones_and_then_runs_alone() {
    let steps = [
        source("reader").locks(r#"["gpu"]"#),
        source("trainer").locks(r#"{ gpu = "exclusive" }"#),
        source("other").locks(r#"["gpu"]"#),
    ];
    let gpu = "[[locks]]\nname = \"gpu\"\nslots = 3\n\n";
    let mut h = Harness::with_locks(&steps, Clocks::default(), gpu).await;
    let first = h.sync(&["reader"]).await;
    h.started("reader").await;
    let second = h.sync(&["trainer"]).await;
    h.until("the exclusive holder to wait", |s| {
        (s.detail("trainer") == Some("waiting for lock gpu, held by reader")).then_some(())
    })
    .await;

    h.run("reader", "ok v1").await;
    h.started("trainer").await;
    let third = h.sync(&["other"]).await;
    h.until("a shared holder to wait for the exclusive one", |s| {
        (s.detail("other") == Some("waiting for lock gpu, held by trainer")).then_some(())
    })
    .await;
    h.run("trainer", "ok v1").await;
    h.started("other").await;
    h.run("other", "ok v1").await;
    for id in [&first, &second, &third] {
        assert_eq!(h.closed(id).await, RequestOutcome::Done);
    }
    h.finish().await;
}

/// A step that reads its inputs off disk runs beside no writer of them,
/// in either order: it waits out a producer that streams, and a producer
/// synced while it reads waits for it.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_reads_files_never_overlaps_a_writer_of_them() {
    let steps = [source("a"), reads("c", &["a"]).reads_files()];
    let mut h = Harness::new(&steps).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "streams").await;
    h.run("a", "seal s1").await;
    h.until("c to wait for a after its seal", |s| {
        let sealed = s.version("a").is_some_and(|v| v.contains("s1"));
        (sealed && s.detail("c") == Some("waiting for a")).then_some(())
    })
    .await;
    assert_eq!(h.state().await.started("c"), 0);
    h.run("a", "ok s1").await;
    h.started("c").await;

    let again = h.sync(&["a"]).await;
    h.until("a to wait for c, which reads its files", |s| {
        (s.detail("a") == Some("waiting for c, which reads what this writes")).then_some(())
    })
    .await;
    h.run("c", "ok c1").await;
    h.started("a").await;
    h.run("a", "ok s2").await;
    h.started("c").await;
    h.run("c", "ok c2").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    assert_eq!(h.closed(&again).await, RequestOutcome::Done);
    h.finish().await;
}
